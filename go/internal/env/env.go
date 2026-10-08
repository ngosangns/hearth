// Package env resolves the base environment a daemon hands to every process it
// spawns. A daemon started outside a login shell (launchd, Finder) inherits a
// bare PATH — the login-shell capture and known-tool-directory augmentation
// make a launchd-spawned daemon resolve the same environment a terminal one
// gets for free.
package env

import (
	"bytes"
	"os"
	"os/exec"
	"strings"
	"sync"
	"syscall"
	"time"
)

const (
	startMarker              = "__hearth_env_start__"
	endMarker                = "__hearth_env_end__"
	defaultLoginShellTimeout = 5 * time.Second
)

var loginShellEnvCache sync.Map // shell -> map[string]string

// parseEnvBlock parses `env -0` output: NUL-terminated KEY=VALUE entries.
func parseEnvBlock(text string) map[string]string {
	out := map[string]string{}
	for _, entry := range strings.Split(text, "\x00") {
		if k, v, ok := splitKeyValue(entry); ok {
			out[k] = v
		}
	}
	return out
}

func splitKeyValue(line string) (string, string, bool) {
	eq := strings.IndexByte(line, '=')
	if eq <= 0 {
		return "", "", false
	}
	key := line[:eq]
	first := key[0]
	if !(first >= 'a' && first <= 'z' || first >= 'A' && first <= 'Z' || first == '_') {
		return "", "", false
	}
	for i := 1; i < len(key); i++ {
		c := key[i]
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '_') {
			return "", "", false
		}
	}
	return key, line[eq+1:], true
}

// runLoginShellEnv runs the user's login shell with `-ilc` and captures `env
// -0` between markers. Spawn failure, timeout, or markerless output is None —
// never cached — so the next startup retries.
func runLoginShellEnv(shell string, timeout time.Duration) map[string]string {
	script := "printf '%s' '" + startMarker + "'; env -0; printf '%s' '" + endMarker + "'"
	cmd := exec.Command(shell, "-ilc", script)
	cmd.Stdin = nil
	cmd.Stderr = nil
	// Its own process group, so the timeout can kill everything rc files
	// started — a grandchild inheriting stdout would otherwise keep the read
	// blocked long past the timeout.
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return nil
	}
	if err := cmd.Start(); err != nil {
		return nil
	}
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	var buf bytes.Buffer
	readDone := make(chan struct{})
	go func() {
		_, _ = buf.ReadFrom(stdout)
		close(readDone)
	}()
	select {
	case <-done:
		<-readDone
	case <-time.After(timeout):
		if pgid, err := syscall.Getpgid(cmd.Process.Pid); err == nil {
			_ = syscall.Kill(-pgid, syscall.SIGKILL)
		} else {
			_ = cmd.Process.Kill()
		}
		<-done
		return nil
	}
	text := buf.String()
	start := strings.Index(text, startMarker)
	end := strings.Index(text, endMarker)
	if start < 0 || end < 0 || end < start {
		return nil
	}
	return parseEnvBlock(text[start+len(startMarker) : end])
}

// KnownToolDirectories are where the tools this daemon invokes (`docker`,
// `tailscale`, `bun`, `ps`) live on a typical macOS dev machine.
var KnownToolDirectories = []string{"/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin"}

// WithKnownToolDirectories appends KNOWN_TOOL_DIRECTORIES (plus ~/.bun/bin and
// ~/.cargo/bin) to path if not already present. Appending rather than
// prepending keeps any explicit ordering in the inherited PATH authoritative.
func WithKnownToolDirectories(path, home string) string {
	var entries []string
	if path != "" {
		entries = strings.Split(path, string(os.PathListSeparator))
	}
	var homeDirs []string
	if home != "" {
		homeDirs = []string{home + "/.bun/bin", home + "/.cargo/bin"}
	}
	seen := map[string]bool{}
	for _, e := range entries {
		seen[e] = true
	}
	for _, dir := range append(append([]string{}, KnownToolDirectories...), homeDirs...) {
		if !seen[dir] {
			entries = append(entries, dir)
			seen[dir] = true
		}
	}
	return strings.Join(entries, string(os.PathListSeparator))
}

// ResolveLoginShellEnv memoizes per shell path; a failed probe is not cached.
func ResolveLoginShellEnv(shell string, timeout time.Duration) map[string]string {
	if shell == "" {
		shell = os.Getenv("SHELL")
	}
	if shell == "" {
		shell = "/bin/zsh"
	}
	if timeout == 0 {
		timeout = defaultLoginShellTimeout
	}
	if cached, ok := loginShellEnvCache.Load(shell); ok {
		return cached.(map[string]string)
	}
	resolved := runLoginShellEnv(shell, timeout)
	if resolved == nil {
		return map[string]string{}
	}
	loginShellEnvCache.Store(shell, resolved)
	return resolved
}

// LoadEnvFile parses a `.env` file with dotenv semantics: `export ` prefixes,
// quoted values, and `${VAR}` interpolation against earlier entries and the
// process environment. An unreadable file resolves to {}.
func LoadEnvFile(path string) map[string]string {
	data, err := os.ReadFile(path)
	if err != nil {
		return map[string]string{}
	}
	return parseDotenv(string(data))
}

// BaseEnvironmentOptions controls BaseEnvironment layering.
type BaseEnvironmentOptions struct {
	// Shell is the login shell to resolve; "" uses $SHELL, "-"
	// skips shell resolution entirely.
	Shell        string
	ShellTimeout time.Duration
	EnvFile      string
	Root         string
	Extra        map[string]string
}

// ResolveBaseEnvironment returns the process's own environment as the floor,
// overlaid with the login shell's environment (unless skipped), then an
// optional .env file, then explicit overrides — each layer only adds or
// replaces keys, never removes.
func ResolveBaseEnvironment(opts BaseEnvironmentOptions) map[string]string {
	base := map[string]string{}
	for _, kv := range os.Environ() {
		if k, v, ok := strings.Cut(kv, "="); ok {
			base[k] = v
		}
	}
	if opts.Shell != "-" {
		shell := opts.Shell
		for k, v := range ResolveLoginShellEnv(shell, opts.ShellTimeout) {
			base[k] = v
		}
	}
	if opts.EnvFile != "" {
		path := opts.EnvFile
		if !strings.HasPrefix(path, "/") {
			root := opts.Root
			if root == "" {
				root, _ = os.Getwd()
			}
			path = root + "/" + path
		}
		for k, v := range LoadEnvFile(path) {
			base[k] = v
		}
	}
	for k, v := range opts.Extra {
		base[k] = v
	}
	return base
}
