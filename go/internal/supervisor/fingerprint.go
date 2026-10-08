// Command fingerprinting — the identity hash a managed process is
// re-recognized by across restarts.
//
// Sharp edge (AGENTS.md): NormalizeCommandFingerprint must hash the *logical*
// command text (argv joined, or the bare shell string) — never the physical
// `sh -c` spawn wrapper — so it matches NormalizeObservedCommandFingerprint's
// prefix-stripped `ps`-observed output. Get this pairing wrong and
// cross-restart process re-adoption silently breaks.
package supervisor

import (
	"crypto/sha256"
	"encoding/hex"
	"regexp"
	"strings"

	"github.com/ngosangns/hearth/go/internal/catalog"
)

func sha256Hex(text string) string {
	digest := sha256.Sum256([]byte(text))
	return hex.EncodeToString(digest[:])
}

// `.trim().replace(/\s+/g, " ")` — Fields already trims and collapses runs of
// whitespace.
func collapseWhitespace(text string) string {
	return strings.Join(strings.Fields(text), " ")
}

// CommandArgv resolves a CommandSpec to the argv it is actually spawned with —
// a Shell always spawns via `sh -c <text>`; the bool is its `exec` flag.
func CommandArgv(spec *catalog.CommandSpec) ([]string, bool) {
	if spec.IsArgv() {
		return append([]string{}, spec.Argv...), false
	}
	exec := spec.Exec != nil && *spec.Exec
	return []string{"sh", "-c", spec.Shell}, exec
}

// NormalizeCommandFingerprint identifies the *logical* command (argv joined,
// or the bare shell text) — never the `sh -c` spawn wrapper itself.
func NormalizeCommandFingerprint(command *catalog.ServiceCommand) string {
	var text string
	if command.Command.IsArgv() {
		text = strings.Join(command.Command.Argv, " ")
	} else {
		text = command.Command.Shell
	}
	return sha256Hex(collapseWhitespace(strings.TrimSpace(text)))
}

var shCPrefixRe = regexp.MustCompile(`^(?:/bin/)?sh\s+-l?c\s+`)

func NormalizeObservedCommandFingerprint(command string) string {
	stripped := shCPrefixRe.ReplaceAllString(command, "")
	return sha256Hex(collapseWhitespace(strings.TrimSpace(stripped)))
}

// IsShellWrapperCommand reports whether a raw `ps` command line is still the
// `sh -c` wrapper, not the program it will exec.
func IsShellWrapperCommand(command string) bool {
	return shCPrefixRe.MatchString(strings.TrimSpace(command))
}
