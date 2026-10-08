// Package updater ports rust/crates/hearth-cli/src/update.rs: the `hearth update`
// self-updater.
//
// It installs the GitHub release asset `hearth-vX.Y.Z` as
// `~/.local/share/hearth/bin/hearth-X.Y.Z` and points `~/.local/bin/hearth` at it.
//
// The swap renames a new inode into place. Overwriting a mapped ad-hoc-signed binary, or running
// `codesign --force` on that inode, SIGKILLs the process on macOS, so a download is never
// re-signed and the previous file is kept for one generation.
package updater

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"strings"

	"golang.org/x/mod/semver"
)

const latestReleaseURL = "https://api.github.com/repos/ngosangns/hearth/releases/latest"
const usage = "usage: hearth update [--check] [--json] [--force]"
const restartNote = "running daemons keep the previous binary until `hearth --root <project> manager restart` (services stay up) and until `hearth shared` is restarted for smp"

// Options configures Run. The zero value is usable: Version may be empty (then the binary's
// version is not X.Y.Z and the command fails), and Stdout/Stderr default to os.Stdout/os.Stderr.
type Options struct {
	// Args are the arguments after `update` (e.g. --check, --json, --force). They are parsed by
	// Run exactly as the Rust command parses them, including duplicate/unknown-flag errors.
	Args []string
	// Version is the hearth package version, not the CLI's. It is a build-time ldflags variable.
	Version string
	// Stdout receives human output and JSON reports.
	Stdout io.Writer
	// Stderr receives error output.
	Stderr io.Writer
	// Force, when true, is equivalent to passing --force.
	Force bool

	// CurrentExe overrides os.Executable (tests).
	CurrentExe string
	// Home overrides the HOME environment variable (tests).
	Home string
	// PlatformSupported overrides the darwin-arm64 gate (tests).
	PlatformSupported *bool
	// Transport overrides the GitHub release client (tests).
	Transport ReleaseTransport
	// Smoke overrides the staged `--version` smoke test (tests).
	Smoke func(path string) (string, error)
}

// ExitError carries the process exit status Run would have returned: 1 for a failed update,
// 2 for a usage error.
type ExitError struct{ Code int }

func (e *ExitError) Error() string {
	return fmt.Sprintf("hearth update exited with status %d", e.Code)
}

// ExitCode maps a Run error to a process exit status: 0 for nil, the carried code for an
// *ExitError, and 1 otherwise.
func ExitCode(err error) int {
	if err == nil {
		return 0
	}
	var exit *ExitError
	if errors.As(err, &exit) {
		return exit.Code
	}
	return 1
}

// Run executes `hearth update`. It writes human or JSON output to Options.Stdout/Stderr and
// returns nil on success or an *ExitError on failure.
func Run(ctx context.Context, opts Options) error {
	out := opts.Stdout
	if out == nil {
		out = os.Stdout
	}
	errOut := opts.Stderr
	if errOut == nil {
		errOut = os.Stderr
	}
	currentExe := opts.CurrentExe
	if currentExe == "" {
		exe, err := os.Executable()
		if err != nil {
			fmt.Fprintf(errOut, "hearth update: cannot resolve the current executable: %v\n", err)
			return &ExitError{Code: 1}
		}
		currentExe = exe
	}
	home := opts.Home
	if home == "" {
		home = os.Getenv("HOME")
		if home == "" {
			fmt.Fprintln(errOut, "hearth update: HOME is not set")
			return &ExitError{Code: 1}
		}
	}
	transport := opts.Transport
	if transport == nil {
		transport = newGitHubReleaseClient(latestReleaseURL, githubToken())
	}
	smoke := opts.Smoke
	if smoke == nil {
		smoke = smokeBinary
	}
	platformSupported := runtime.GOOS == "darwin" && runtime.GOARCH == "arm64"
	if opts.PlatformSupported != nil {
		platformSupported = *opts.PlatformSupported
	}
	env := updateEnv{
		args:              opts.Args,
		currentVersion:    opts.Version,
		currentExe:        currentExe,
		layout:            layoutFromHome(home),
		platformSupported: platformSupported,
	}
	code := runWith(ctx, env, transport, smoke, out, errOut, opts.Force)
	if code != 0 {
		return &ExitError{Code: code}
	}
	return nil
}

type updateEnv struct {
	args              []string
	currentVersion    string
	currentExe        string
	layout            layout
	platformSupported bool
}

type updateFlags struct {
	check bool
	json  bool
	force bool
}

func runWith(
	ctx context.Context,
	env updateEnv,
	transport ReleaseTransport,
	smoke func(path string) (string, error),
	out, errOut io.Writer,
	force bool,
) int {
	if len(env.args) == 1 && (env.args[0] == "--help" || env.args[0] == "-h") {
		fmt.Fprintln(out, usage)
		return 0
	}
	flags, err := parseUpdateArgs(env.args)
	if err != nil {
		fmt.Fprintln(errOut, err.Error())
		return 2
	}
	if force {
		flags.force = true
	}
	current, ok := parseSemver(env.currentVersion)
	if !ok {
		return fail(out, errOut, flags.json, reportBase(env, nil),
			fmt.Sprintf("this binary's version %q is not X.Y.Z", env.currentVersion))
	}
	if !env.platformSupported {
		return fail(out, errOut, flags.json, reportBase(env, nil),
			"the release asset is darwin-arm64 only")
	}
	// A notarized app cannot have one nested binary replaced. Refuse before any download.
	if insideAppBundle(env.currentExe) {
		return fail(out, errOut, flags.json, reportBase(env, nil),
			"This binary is inside a Hearth.app. Update the Hearth app. hearth update does not replace it.")
	}
	if !installOwned(env.currentExe, env.layout) {
		return fail(out, errOut, flags.json, reportBase(env, nil),
			fmt.Sprintf("hearth update only replaces %s (this process is %s)", env.layout.link, env.currentExe))
	}

	document, err := transport.FetchLatest(ctx)
	if err != nil {
		return fail(out, errOut, flags.json, reportBase(env, nil), fetchMessage(err, "GitHub request failed"))
	}
	release, err := parseRelease(document)
	if err != nil {
		return fail(out, errOut, flags.json, reportBase(env, nil), err.Error())
	}
	latest, _ := parseSemver(release.version) // parseRelease only returns X.Y.Z
	report := reportBase(env, &release)
	report.updateAvailable = semver.Compare("v"+latest, "v"+current) > 0

	if flags.check || semver.Compare("v"+latest, "v"+current) < 0 || (latest == current && !flags.force) {
		var human string
		switch {
		case semver.Compare("v"+latest, "v"+current) > 0:
			human = fmt.Sprintf("hearth %s is available (current %s)", latest, current)
		case latest == current:
			human = fmt.Sprintf("hearth %s is already up to date", current)
		default:
			human = fmt.Sprintf("hearth %s is newer than the latest release %s", current, latest)
		}
		return succeed(out, errOut, flags.json, report, human)
	}

	lock, err := acquireUpdateLock(env.layout.dir)
	if err != nil {
		return fail(out, errOut, flags.json, report, err.Error())
	}
	defer lock.release()
	inode, err := fileID(env.layout.link)
	if err != nil {
		return fail(out, errOut, flags.json, report,
			fmt.Sprintf("cannot stat %s: %v", env.layout.link, err))
	}

	partial := filepath.Join(env.layout.dir, fmt.Sprintf(".hearth-%s.%d.partial", release.version, os.Getpid()))
	// Removed on return, including when a later step moves the bytes back to this path.
	guard := &partialFile{path: partial}
	defer guard.remove()
	if err := transport.Download(ctx, release.url, partial); err != nil {
		return fail(out, errOut, flags.json, report, fetchMessage(err, "download failed"))
	}
	if err := verifyFile(partial, release.size, release.sha256); err != nil {
		return fail(out, errOut, flags.json, report, err.Error())
	}
	if err := ensureExecutable(partial); err != nil {
		return fail(out, errOut, flags.json, report, err.Error())
	}
	printed, err := smoke(partial)
	if err != nil {
		return fail(out, errOut, flags.json, report, err.Error())
	}
	if printed != release.version {
		return fail(out, errOut, flags.json, report,
			fmt.Sprintf("smoke test printed \"hearth %s\", expected hearth %s", printed, release.version))
	}

	finalPath := env.layout.versioned(release.version)
	// The bytes are moved into place by rename, never written over the live inode and never
	// re-signed: `codesign --force` on a mapped ad-hoc binary SIGKILLs that process, and so does
	// overwriting its inode. A running daemon keeps the old inode until it is restarted.
	previousFile, err := rotateIntoPlace(partial, finalPath)
	if err != nil {
		return fail(out, errOut, flags.json, report, err.Error())
	}
	var previousVersion *string
	if target, err := os.Readlink(env.layout.link); err == nil {
		if version, ok := versionFromName(target); ok {
			previousVersion = &version
		}
	}
	sameVersion := previousVersion != nil && *previousVersion == release.version
	// Last look at the link. The swap is next, and a rotated file is put back when this check
	// or the swap fails.
	if err := ensureLinkUnchanged(env.layout.link, inode); err != nil {
		undoRotation(partial, finalPath, previousFile, sameVersion)
		return fail(out, errOut, flags.json, report, err.Error())
	}
	if _, err := publishLink(env.layout, finalPath); err != nil {
		undoRotation(partial, finalPath, previousFile, sameVersion)
		return fail(out, errOut, flags.json, report, err.Error())
	}
	// The command was `hearthd` through 0.17.0. A successful install drops that symlink
	// and leaves the old versioned file alone (a running daemon may still be mapped to it).
	removeLegacyCommandLink(env.layout)

	keep := []string{release.version}
	if previousVersion != nil {
		keep = append(keep, *previousVersion)
	}
	pruneOldVersions(env.layout.dir, keep)

	report.updateAvailable = false
	report.installPath = finalPath
	var human string
	if semver.Compare("v"+latest, "v"+current) > 0 {
		human = fmt.Sprintf("updated hearth to %s\n%s", latest, restartNote)
	} else {
		human = fmt.Sprintf("reinstalled hearth %s\n%s", latest, restartNote)
	}
	return succeed(out, errOut, flags.json, report, human)
}

func parseUpdateArgs(args []string) (updateFlags, error) {
	var flags updateFlags
	for _, arg := range args {
		switch {
		case arg == "--check" && !flags.check:
			flags.check = true
		case arg == "--json" && !flags.json:
			flags.json = true
		case arg == "--force" && !flags.force:
			flags.force = true
		case arg == "--check" || arg == "--json" || arg == "--force":
			return flags, fmt.Errorf("duplicate flag: %s", arg)
		case strings.HasPrefix(arg, "-"):
			return flags, fmt.Errorf("unknown flag: %s", arg)
		default:
			return flags, errors.New(usage)
		}
	}
	return flags, nil
}

type report struct {
	current         string
	latest          *string
	updateAvailable bool
	asset           *string
	installPath     string
	err             *string
}

func reportBase(env updateEnv, release *Release) report {
	r := report{
		current:     env.currentVersion,
		installPath: env.layout.link,
	}
	if release != nil {
		latest := release.version
		asset := release.asset
		r.latest = &latest
		r.asset = &asset
		r.installPath = env.layout.versioned(release.version)
	}
	return r
}

func succeed(out, errOut io.Writer, jsonMode bool, r report, human string) int {
	emit(out, errOut, jsonMode, r, human, true)
	return 0
}

func insideAppBundle(path string) bool {
	return strings.Contains(path, ".app/Contents/")
}

func fail(out, errOut io.Writer, jsonMode bool, r report, message string) int {
	r.err = &message
	emit(out, errOut, jsonMode, r, message, false)
	return 1
}

func emit(out, errOut io.Writer, jsonMode bool, r report, human string, ok bool) {
	if jsonMode {
		fmt.Fprintln(out, jsonReport(r))
		return
	}
	if ok {
		for _, line := range strings.Split(human, "\n") {
			fmt.Fprintln(out, line)
		}
	} else {
		fmt.Fprintf(errOut, "hearth update: %s\n", human)
	}
}

func jsonReport(r report) string {
	type pair struct {
		key   string
		value any
	}
	pairs := []pair{
		{"current", r.current},
		{"latest", r.latest},
		{"updateAvailable", r.updateAvailable},
		{"asset", r.asset},
		{"installPath", r.installPath},
	}
	if r.err != nil {
		pairs = append(pairs, pair{"error", *r.err})
	}
	var b strings.Builder
	b.WriteByte('{')
	for i, p := range pairs {
		if i > 0 {
			b.WriteByte(',')
		}
		b.WriteString(marshalJSON(p.key))
		b.WriteByte(':')
		b.WriteString(marshalJSON(p.value))
	}
	b.WriteByte('}')
	return b.String()
}
