// Duplicate-instance reaping for daemon and service restarts.
//
// A restart replaces one instance. Other processes that are the same instance — another
// `hearth daemon --root <this project>` (or `hearth smp` for the shared root), or another
// process running a service's command from that service's directory — are stopped first.
// Only those pids are signalled, never their process group: a daemon's services live in
// their own groups and must keep running, and an unrelated job must not be pulled in by a
// shared pgid. `lstart` is checked again at signal time so a recycled pid is left alone.
// If the process table cannot be read, the restart fails instead of starting a second copy.
package supervisor

import (
	"bytes"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/ngosangns/hearth/go/internal/shared"
)

const (
	psTimeout = 5 * time.Second
	termGrace = 5 * time.Second
	killGrace = 2 * time.Second
)

// LiveProcess is one row of `ps -axww -o pid=,lstart=,command=`.
type LiveProcess struct {
	Pid           int64
	StartIdentity string
	Command       string
}

// DaemonRole separates the two daemon roles: a project restart must not touch
// `hearth smp`, and an smp restart must not touch a project daemon.
type DaemonRole struct {
	Shared bool
	Root   string
}

func DaemonRoleFor(root string) DaemonRole {
	sharedRoot := shared.SharedRoot()
	if pathsEqual(root, sharedRoot) {
		return DaemonRole{Shared: true, Root: sharedRoot}
	}
	return DaemonRole{Root: root}
}

var psCommandRe = regexp.MustCompile(`^(\d+)\s+(.{24})\s+(.+)$`)

// ParsePsCommandRows parses `ps -axww -o pid=,lstart=,command=` output. A line
// that does not match is skipped — one odd row must not hide every other
// process — but the caller treats a failed `ps` as unknown.
func ParsePsCommandRows(stdout string) []LiveProcess {
	var out []LiveProcess
	for _, line := range strings.Split(stdout, "\n") {
		caps := psCommandRe.FindStringSubmatch(strings.TrimLeft(line, " \t"))
		if caps == nil {
			continue
		}
		pid, err := strconv.ParseInt(caps[1], 10, 64)
		if err != nil {
			continue
		}
		out = append(out, LiveProcess{
			Pid:           pid,
			StartIdentity: strings.TrimSpace(caps[2]),
			Command:       strings.TrimSpace(caps[3]),
		})
	}
	return out
}

// ParseLsofCwdMap parses `lsof -Fn` cwd records, keyed by pid. The first cwd
// for a pid wins.
func ParseLsofCwdMap(stdout string) map[int64]string {
	out := map[int64]string{}
	var pid int64
	havePid := false
	atCwd := false
	for _, line := range strings.Split(stdout, "\n") {
		if rest, ok := strings.CutPrefix(line, "p"); ok {
			if parsed, err := strconv.ParseInt(rest, 10, 64); err == nil {
				pid = parsed
				havePid = true
				atCwd = false
				continue
			}
		}
		if fd, ok := strings.CutPrefix(line, "f"); ok {
			atCwd = fd == "cwd"
			continue
		}
		if atCwd {
			if path, ok := strings.CutPrefix(line, "n"); ok && havePid {
				if _, seen := out[pid]; !seen {
					out[pid] = path
				}
			}
			atCwd = false
		}
	}
	return out
}

// SelectDuplicateDaemons filters the process table down to daemons serving
// this role's root — never self, never pid <= 1.
func SelectDuplicateDaemons(processes []LiveProcess, role DaemonRole, selfPid int64) []LiveProcess {
	var out []LiveProcess
	for _, p := range processes {
		if p.Pid > 1 && p.Pid != selfPid && isDuplicateDaemon(p, role) {
			out = append(out, p)
		}
	}
	return out
}

func isDuplicateDaemon(p LiveProcess, role DaemonRole) bool {
	if role.Shared {
		if isSmpDaemon(p.Command) {
			return true
		}
		if root := projectDaemonRoot(p.Command); root != nil && pathsEqual(*root, role.Root) {
			return true
		}
		return false
	}
	if root := projectDaemonRoot(p.Command); root != nil && pathsEqual(*root, role.Root) {
		return true
	}
	return false
}

// CommandFingerprintMatches reports whether an observed `ps` command line
// hashes (after sh-c stripping) to one of `fingerprints`.
func CommandFingerprintMatches(command string, fingerprints []string) bool {
	observed := NormalizeObservedCommandFingerprint(command)
	for _, f := range fingerprints {
		if f == observed {
			return true
		}
	}
	return false
}

// CommandArgv0 is the first token of a `ps` command line. A command with no
// spaces is itself.
func CommandArgv0(command string) string {
	command = strings.TrimSpace(command)
	if command == "" {
		return ""
	}
	if i := strings.IndexAny(command, " \t\n\r"); i >= 0 {
		return command[:i]
	}
	return command
}

func fileName(token string) string {
	if i := strings.LastIndexAny(token, "/\\"); i >= 0 {
		return token[i+1:]
	}
	return token
}

// Interpreters and the hearth binary run many different commands. Matching
// them by argv0 would reap a daemon, a TUI, or another script in the same
// directory.
func dedicatedServerBinary(exe string) bool {
	name := fileName(exe)
	if isHearthBinaryName(name) || name == "hearthd" {
		return false
	}
	switch name {
	case "node", "nodejs", "python", "python3", "java", "ruby", "perl",
		"sh", "bash", "zsh", "bun", "deno":
		return false
	}
	return true
}

// ExecutableMatches reports whether `command`'s executable is one of
// `executables`. Both sides must be absolute paths of a dedicated server
// binary. A bare `node`, or `hearth`, does not count.
func ExecutableMatches(command string, executables []string) bool {
	if len(executables) == 0 {
		return false
	}
	exe := CommandArgv0(command)
	if exe == "" || !strings.HasPrefix(exe, "/") || !dedicatedServerBinary(exe) {
		return false
	}
	for _, expected := range executables {
		if strings.HasPrefix(expected, "/") && dedicatedServerBinary(expected) && pathsEqual(exe, expected) {
			return true
		}
	}
	return false
}

func pathsEqual(left, right string) bool {
	if normalizeLexical(left) == normalizeLexical(right) {
		return true
	}
	l, lerr := filepath.EvalSymlinks(left)
	r, rerr := filepath.EvalSymlinks(right)
	if lerr != nil || rerr != nil {
		return false
	}
	// EvalSymlinks returns relative results for relative inputs.
	if la, err := filepath.Abs(l); err == nil {
		l = la
	}
	if ra, err := filepath.Abs(r); err == nil {
		r = ra
	}
	return l == r
}

func normalizeLexical(path string) string {
	// filepath.Clean resolves . and .. lexically, matching the Rust
	// component-wise normalize.
	return filepath.Clean(path)
}

// IsHearthBinaryName matches `hearth` or the versioned install name
// `hearth-<digits>.<digits>...`. `hearthd` and `hearth-mcp` are different
// programs.
func isHearthBinaryName(name string) bool {
	if name == "hearth" {
		return true
	}
	rest, ok := strings.CutPrefix(name, "hearth-")
	if !ok {
		return false
	}
	any := false
	for _, part := range strings.Split(rest, ".") {
		if part == "" {
			return false
		}
		for _, c := range part {
			if c < '0' || c > '9' {
				return false
			}
		}
		any = true
	}
	return any
}

func splitExe(command string) (exe, rest string, ok bool) {
	command = strings.TrimSpace(command)
	i := strings.IndexAny(command, " \t\n\r")
	if i < 0 {
		return "", "", false
	}
	exe = command[:i]
	if exe == "" {
		return "", "", false
	}
	return exe, strings.TrimLeft(command[i:], " \t\n\r"), true
}

func stripToken(input, token string) (string, bool) {
	input = strings.TrimLeft(input, " \t\n\r")
	rest, ok := strings.CutPrefix(input, token)
	if !ok {
		return "", false
	}
	if rest == "" {
		return "", true
	}
	if c := rest[0]; c == ' ' || c == '\t' || c == '\n' || c == '\r' {
		return strings.TrimLeft(rest, " \t\n\r"), true
	}
	return "", false
}

// ProjectDaemonRoot extracts the `--root` argument of a hearth daemon,
// preserving spaces inside the path. Accepts `hearth daemon --root <path>`
// and `hearth --root <path> daemon`.
func projectDaemonRoot(command string) *string {
	exe, rest, ok := splitExe(command)
	if !ok || !isHearthBinaryName(fileName(exe)) {
		return nil
	}
	if afterDaemon, ok := stripToken(rest, "daemon"); ok {
		path, ok := stripToken(afterDaemon, "--root")
		if !ok || path == "" {
			return nil
		}
		return &path
	}
	afterRoot, ok := stripToken(rest, "--root")
	if !ok {
		return nil
	}
	afterRoot = strings.TrimRight(afterRoot, " \t\n\r")
	if !strings.HasSuffix(afterRoot, "daemon") {
		return nil
	}
	path := afterRoot[:len(afterRoot)-len("daemon")]
	if path == "" {
		return nil
	}
	last := path[len(path)-1]
	if last != ' ' && last != '\t' {
		return nil
	}
	path = strings.TrimSpace(path)
	if path == "" {
		return nil
	}
	return &path
}

func isSmpDaemon(command string) bool {
	exe, rest, ok := splitExe(command)
	return ok && isHearthBinaryName(fileName(exe)) && rest == "smp"
}

// PIDIsLive reports whether pid is a live process. A zombie is not live:
// kill(pid, 0) still succeeds for one, and init reaps it, so it must not
// block the new daemon from starting.
func PIDIsLive(pid int64) (bool, error) {
	if pid <= 0 {
		return false, nil
	}
	code, stat, err := runPs([]string{"ps", "-o", "stat=", "-p", strconv.FormatInt(pid, 10)})
	if err != nil {
		return false, err
	}
	if code != 0 {
		return false, nil
	}
	stat = strings.TrimSpace(stat)
	return stat != "" && !strings.HasPrefix(stat, "Z"), nil
}

// ReapDuplicateDaemons stops every other daemon process for `root`. Signals
// the pid only.
func ReapDuplicateDaemons(root string) error {
	role := DaemonRoleFor(root)
	selfPid := int64(os.Getpid())
	processes, err := listLiveProcesses()
	if err != nil {
		return err
	}
	return reapChecked(role, SelectDuplicateDaemons(processes, role, selfPid))
}

func reapChecked(role DaemonRole, targets []LiveProcess) error {
	if len(targets) == 0 {
		return nil
	}
	if err := signalStillMatching(role, targets, SignalTerm); err != nil {
		return err
	}
	var err error
	if targets, err = waitUntilGone(role, targets, termGrace); err != nil {
		return err
	}
	if len(targets) == 0 {
		return nil
	}
	if err := signalStillMatching(role, targets, SignalKill); err != nil {
		return err
	}
	if targets, err = waitUntilGone(role, targets, killGrace); err != nil {
		return err
	}
	if len(targets) == 0 {
		return nil
	}
	var pids []string
	for _, p := range targets {
		pids = append(pids, strconv.FormatInt(p.Pid, 10))
	}
	return NewError("duplicate hearth daemon still running (pid " + strings.Join(pids, ", ") + ")")
}

func signalStillMatching(role DaemonRole, targets []LiveProcess, signal ProcessSignal) error {
	for _, t := range targets {
		still, err := stillDuplicate(role, t)
		if err != nil {
			return err
		}
		if still {
			sendSignal(t.Pid, signal)
		}
	}
	return nil
}

func waitUntilGone(role DaemonRole, targets []LiveProcess, grace time.Duration) ([]LiveProcess, error) {
	deadline := time.Now().Add(grace)
	for {
		var still []LiveProcess
		for _, t := range targets {
			ok, err := stillDuplicate(role, t)
			if err != nil {
				return nil, err
			}
			if ok {
				still = append(still, t)
			}
		}
		targets = still
		if len(targets) == 0 || !time.Now().Before(deadline) {
			return targets, nil
		}
		time.Sleep(100 * time.Millisecond)
	}
}

// stillDuplicate is the revalidation before every signal: same pid, same
// `lstart`, same daemon command. A zombie, a reused pid, or a `ps` row that
// no longer matches is not signalled and does not fail the reap.
func stillDuplicate(role DaemonRole, target LiveProcess) (bool, error) {
	pid := strconv.FormatInt(target.Pid, 10)
	code, stdout, err := runPs([]string{"ps", "-ww", "-o", "pid=,lstart=,command=", "-p", pid})
	if err != nil {
		return false, err
	}
	if code != 0 {
		return false, nil
	}
	var row *LiveProcess
	for _, r := range ParsePsCommandRows(stdout) {
		if r.Pid == target.Pid {
			row = &r
			break
		}
	}
	if row == nil || row.StartIdentity != target.StartIdentity || !isDuplicateDaemon(*row, role) {
		return false, nil
	}
	statCode, stat, err := runPs([]string{"ps", "-o", "stat=", "-p", pid})
	if err != nil {
		return false, err
	}
	if statCode != 0 {
		return false, nil
	}
	stat = strings.TrimSpace(stat)
	return stat != "" && !strings.HasPrefix(stat, "Z"), nil
}

func listLiveProcesses() ([]LiveProcess, error) {
	code, stdout, err := runPs([]string{"ps", "-axww", "-o", "pid=,lstart=,command="})
	if err != nil {
		return nil, err
	}
	if code != 0 {
		return nil, NewError("could not list processes; refusing to start beside an unchecked duplicate daemon")
	}
	return ParsePsCommandRows(stdout), nil
}

// MatchingServiceProcesses lists live host processes in `projectRoot/cwd`
// that are this service: the observed command hashes to one of `fingerprints`,
// or its absolute executable is one of `executables` (a title rewrite keeps
// the binary and replaces the arguments). nil means the table could not be
// read, or a match was still alive but its cwd could not be checked — the
// caller must not start another copy.
func MatchingServiceProcesses(projectRoot string, fingerprints, executables []string, cwd string) *[]CommandMatch {
	if len(fingerprints) == 0 && len(executables) == 0 {
		return &[]CommandMatch{}
	}
	code, stdout, err := runPs([]string{"ps", "-axww", "-o", "pid=,lstart=,command="})
	if err != nil || code != 0 {
		return nil
	}
	selfPid := int64(os.Getpid())
	var candidates []LiveProcess
	for _, p := range ParsePsCommandRows(stdout) {
		if p.Pid > 1 && p.Pid != selfPid &&
			(CommandFingerprintMatches(p.Command, fingerprints) || ExecutableMatches(p.Command, executables)) {
			candidates = append(candidates, p)
		}
	}
	if len(candidates) == 0 {
		return &[]CommandMatch{}
	}
	var spec []string
	for _, c := range candidates {
		spec = append(spec, strconv.FormatInt(c.Pid, 10))
	}
	lsofCode, lsofOut, err := runPs([]string{"lsof", "-nP", "-d", "cwd", "-a", "-p", strings.Join(spec, ","), "-Fn"})
	if err != nil || lsofCode == -1 {
		return nil
	}
	cwds := ParseLsofCwdMap(lsofOut)
	expected := filepath.Join(projectRoot, cwd)
	var matches []CommandMatch
	for _, candidate := range candidates {
		actual, ok := cwds[candidate.Pid]
		switch {
		case ok && pathsEqual(actual, expected):
			matches = append(matches, CommandMatch{Pid: candidate.Pid, StartIdentity: candidate.StartIdentity})
		case ok:
			// Different cwd — not this service.
		default:
			// Gone between the two probes is not a duplicate. Still alive
			// with no cwd is unknown — killing it would be a guess, and
			// ignoring it could leave a second copy.
			code, stdout, err := runPs([]string{"ps", "-ww", "-o", "pid=,lstart=,command=", "-p", strconv.FormatInt(candidate.Pid, 10)})
			if err != nil {
				return nil
			}
			if code == 0 {
				for _, row := range ParsePsCommandRows(stdout) {
					if row.Pid == candidate.Pid && row.StartIdentity == candidate.StartIdentity {
						return nil
					}
				}
			}
		}
	}
	return &matches
}

func sendSignal(pid int64, signal ProcessSignal) {
	if pid <= 1 {
		return
	}
	sig := syscall.SIGTERM
	if signal == SignalKill {
		sig = syscall.SIGKILL
	}
	_ = syscall.Kill(int(pid), sig)
}

// runPs runs a `ps`/`lsof`-style probe: `(exit code, stdout)`. A spawn failure
// or a timeout is an error, because the caller must not read that as "no such
// process". The child runs in its own process group and is killed on timeout.
func runPs(argv []string) (int, string, error) {
	if len(argv) == 0 {
		return -1, "", NewError("could not list processes")
	}
	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Stdin = nil
	cmd.Stderr = nil
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	var buf bytes.Buffer
	cmd.Stdout = &buf
	if err := cmd.Start(); err != nil {
		return -1, "", NewError("could not list processes")
	}
	waitCh := make(chan error, 1)
	go func() { waitCh <- cmd.Wait() }()
	timer := time.NewTimer(psTimeout)
	defer timer.Stop()
	select {
	case werr := <-waitCh:
		if werr != nil {
			if ee, ok := werr.(*exec.ExitError); ok {
				return ee.ExitCode(), buf.String(), nil
			}
			return -1, "", NewError("could not list processes")
		}
		return 0, buf.String(), nil
	case <-timer.C:
		if cmd.Process != nil {
			_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
			_ = cmd.Process.Kill()
		}
		<-waitCh
		return -1, "", NewError("could not list processes")
	}
}
