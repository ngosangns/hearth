// Package doctor ports rust/crates/hearth-core/src/doctor.rs: a thin generic
// engine (tcp probe / command-exec probe / path-exists probe) driving a
// caller-supplied check list. Nothing project-specific belongs here.
package doctor

import (
	"bytes"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/env"
	"github.com/ngosangns/hearth/go/internal/platform"
)

type DoctorCheckResult struct {
	Name   string `json:"name"`
	OK     bool   `json:"ok"`
	Detail string `json:"detail"`
}

type DoctorReport struct {
	OK                 bool                `json:"ok"`
	Checks             []DoctorCheckResult `json:"checks"`
	UnresolvedProfiles []string            `json:"unresolvedProfiles"`
}

type CommandResult struct {
	OK     bool   `json:"ok"`
	Output string `json:"output"`
}

// DoctorAdapter is the seam every doctor probe goes through.
type DoctorAdapter interface {
	Command(command string, args []string) CommandResult
	Path(path string) bool
	Port(port uint16) bool
}

// PlatformAdapter is the optional platform override. An adapter that does not
// implement it reports the current platform — the Rust trait's provided
// `platform()` method.
type PlatformAdapter interface {
	Platform() string
}

func tcpProbe(port uint16) bool {
	conn, err := net.DialTimeout("tcp", fmt.Sprintf("127.0.0.1:%d", port), 250*time.Millisecond)
	if err != nil {
		return false
	}
	_ = conn.Close()
	return true
}

type DefaultDoctorAdapter struct{}

func (DefaultDoctorAdapter) Command(command string, args []string) CommandResult {
	// `command -v`-style checks must see the daemon's own PATH — the caller's
	// bare launchd PATH lacks the tool dirs env.WithKnownToolDirectories
	// appends. Rust searches the *child's* PATH for the program too, so resolve
	// against the augmented PATH rather than the parent's.
	path := env.WithKnownToolDirectories(os.Getenv("PATH"), os.Getenv("HOME"))
	resolved, err := lookPathIn(command, path)
	if err != nil {
		return CommandResult{OK: false, Output: ""}
	}
	cmd := exec.Command(resolved, args...)
	cmd.Env = envWithPath(path)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	runErr := cmd.Run()
	output := strings.TrimSpace(
		strings.ToValidUTF8(stdout.String(), "\uFFFD") + "\n" +
			strings.ToValidUTF8(stderr.String(), "\uFFFD"))
	return CommandResult{OK: runErr == nil, Output: output}
}

func (DefaultDoctorAdapter) Path(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

func (DefaultDoctorAdapter) Port(port uint16) bool { return tcpProbe(port) }

// lookPathIn resolves name against path, mirroring exec.LookPath but using the
// child's PATH (Rust searches the child's environment for the program). A name
// containing a path separator is used as-is.
func lookPathIn(name, path string) (string, error) {
	if strings.ContainsRune(name, filepath.Separator) {
		return name, nil
	}
	for _, dir := range filepath.SplitList(path) {
		if dir == "" {
			dir = "."
		}
		candidate := filepath.Join(dir, name)
		info, err := os.Stat(candidate)
		if err != nil || info.IsDir() {
			continue
		}
		if info.Mode()&0o111 != 0 {
			return candidate, nil
		}
	}
	return "", exec.ErrNotFound
}

// envWithPath returns the current environment with PATH replaced by path —
// Rust's `Command::env("PATH", path)` inherits the parent env and overrides
// just PATH.
func envWithPath(path string) []string {
	environ := os.Environ()
	out := make([]string, 0, len(environ)+1)
	for _, kv := range environ {
		if strings.HasPrefix(kv, "PATH=") {
			continue
		}
		out = append(out, kv)
	}
	return append(out, "PATH="+path)
}

// DoctorCheckPredicate overrides a command check's pass/fail. `Send + Sync` in
// Rust so the whole check list can cross an Arc<dyn ... + Send + Sync>
// boundary; a plain Go func value is already safe to share.
type DoctorCheckPredicate func(result CommandResult) bool

// DoctorCheckDetailFormatter overrides a command check's detail text.
type DoctorCheckDetailFormatter func(result CommandResult) string

type DoctorCommandCheck struct {
	Name    string
	Command string
	Args    []string
	OK      DoctorCheckPredicate
	Detail  DoctorCheckDetailFormatter
}

type DoctorPathCheck struct {
	Name string
	Path string
}

type DoctorPortCheck struct {
	Name string
	Port uint16
}

type DoctorChecks struct {
	Commands []DoctorCommandCheck
	Paths    []DoctorPathCheck
	Ports    []DoctorPortCheck
}

// DefaultDoctorChecks is the host-tool check list the CLI runs: the tools the
// daemon and the test suite shell out to (AGENTS.md "Build, test, release").
// Ported from hearth-cli's `default_doctor_checks`.
func DefaultDoctorChecks() DoctorChecks {
	onPath := func(name string) DoctorCommandCheck {
		return DoctorCommandCheck{
			Name:    name,
			Command: "sh",
			Args:    []string{"-c", "command -v " + name},
		}
	}
	checks := DoctorChecks{}
	for _, name := range []string{"docker", "tailscale", "nc", "ps", "sh"} {
		checks.Commands = append(checks.Commands, onPath(name))
	}
	return checks
}

func RunDoctor(cat *catalog.ServiceCatalog, checks *DoctorChecks, adapter DoctorAdapter) DoctorReport {
	platformName := platform.CurrentPlatform()
	if pa, ok := adapter.(PlatformAdapter); ok {
		platformName = pa.Platform()
	}
	platformOK := platform.IsSupportedHearthPlatform(platformName)
	platformDetail := platformName
	if !platformOK {
		platformDetail = platform.UnsupportedPlatformMessage(platformName)
	}
	results := []DoctorCheckResult{{Name: "platform", OK: platformOK, Detail: platformDetail}}

	for _, check := range checks.Commands {
		result := adapter.Command(check.Command, check.Args)
		ok := result.OK
		if check.OK != nil {
			ok = check.OK(result)
		}
		detail := check.Command
		if check.Detail != nil {
			detail = check.Detail(result)
		}
		results = append(results, DoctorCheckResult{Name: check.Name, OK: ok, Detail: detail})
	}
	for _, check := range checks.Paths {
		results = append(results, DoctorCheckResult{
			Name:   check.Name,
			OK:     adapter.Path(check.Path),
			Detail: check.Path,
		})
	}
	for _, check := range checks.Ports {
		results = append(results, DoctorCheckResult{
			Name:   check.Name,
			OK:     adapter.Port(check.Port),
			Detail: fmt.Sprintf("127.0.0.1:%d", check.Port),
		})
	}

	validation := catalog.ValidateCatalog(cat)
	var unresolvedProfiles []string
	for _, warning := range validation.Warnings {
		if strings.Contains(warning, "command is unresolved") {
			unresolvedProfiles = append(unresolvedProfiles, warning)
		}
	}

	ok := len(validation.Errors) == 0
	for _, result := range results {
		if !result.OK {
			ok = false
			break
		}
	}
	return DoctorReport{OK: ok, Checks: results, UnresolvedProfiles: unresolvedProfiles}
}
