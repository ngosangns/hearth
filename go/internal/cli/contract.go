package cli

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"runtime/debug"
	"sort"
	"strings"

	"github.com/ngosangns/hearth/go/internal/omap"
	"github.com/ngosangns/hearth/go/internal/redact"
	"github.com/ngosangns/hearth/go/internal/schema"
)

const skillUsage = "usage: hearth skill install [--dest DIR]\n       hearth skill check [--dest DIR] [--json]"

const versionUsage = "usage: hearth version [--json]"

const schemaUsage = "usage: hearth schema [--json] [<command>]"

const connectionUsage = "usage: hearth shared connection <service|name@version> [--json] [--reveal]"

const workspacesUsage = "usage: hearth workspaces list [--json]"

const sharedUsage = "usage: hearth shared ensure|list|installed|status|connection|attach|detach|probe|install|start|stop|remove [--json] [--mask] [--reveal] [--force] [--yes] [--stop-if-unused] [<name>@<version>] [attach-args...]"

// HelpText is the top-level `hearth --help` page. `version`, `schema`, `skill`,
// `workspaces`, `shared`, and `update` are on it because they run without a project catalog.
func HelpText() string {
	return `hearth — local dev services daemon, CLI, and MCP server

usage: hearth [--root <path>] <command> [options]
  status [target] [--json]              current state of one service, a group, or all
  events --follow --json                stream daemon changes as newline-delimited JSON
  start|stop|restart <target> [--wait] [--json] [--dry-run] [--yes]
                                        lifecycle (--kill-unowned on start; --yes confirms it off a TTY)
  logs <service> [--tail N] [--follow]  read a service's log
  urls [target] [--json]                where each service can be reached (live URLs)
  operation get|watch <id> [--json]     inspect one operation
  doctor [--json]                       environment and catalog diagnostics
  cleanup                               remove stale runtime state
  manager ensure|status|stop|restart|reload [--json] [--yes]
                                        daemon lifecycle (stop needs --yes off a TTY; restart keeps services running)
  daemon --root <path>                  run the daemon in the foreground (spawned internally)
  smp                                   run the machine-global shared-services daemon
  shared list [--json]                  remote shared-service recipes
  shared installed|status [--json] [--mask] [--reveal]
                                        secrets masked unless --reveal (--mask changes nothing)
  shared connection <service|name@version> [--json] [--reveal]
                                        this project's connection (secrets masked unless --reveal)
  shared ensure [--json]                find or start smp
  shared attach|detach|probe <id> [--yes] [--stop-if-unused]
                                        attach, detach, or probe (detach --stop-if-unused needs --yes off a TTY)
  shared install|start|stop|remove <id> [--json] [--force] [--yes]
                                        manage a shared instance (remove needs --yes off a TTY)
  update [--check] [--json] [--force]   install the latest stable GitHub release
  mcp                                   serve the MCP tool surface over stdio
  mcp install [--name N] [--key K] <config-file>...
                                        register this binary in an MCP client config
  skill install [--dest DIR]            install SKILL.md + scripts (default ~/.agents/skills/hearth)
  skill check [--dest DIR] [--json]     compare the installed skill with this binary
  version [--json]                      name, version, schemaVersion, commit
  schema [--json] [<command>]           command list and JSON Schemas
  workspaces list [--json]              workspace folders the Hearth window knows

shared, update, version, schema, skill, and workspaces do not need a hearth.yaml in the current directory.
The workspace window is hearth-gpui, not this binary.
mcp install records this process path. A binary inside Hearth.app makes those configs point into the bundle.
Every other command resolves a catalog from --root (default: cwd):
hearth.yaml, .yml, or .json.
With --json, a failure prints {"error":{code,exit,message}} on stdout. Exit 6 is needs-confirm: pass --yes.`
}

func isHelp(args []string) bool {
	if len(args) == 1 && args[0] == "help" {
		return true
	}
	for _, arg := range args {
		if arg == "--" {
			return false
		}
		if arg == "--help" || arg == "-h" {
			return true
		}
	}
	return false
}

// HelpFor reports whether args ask for help before a `--` passthrough, and the
// usage text to print. A positional named "help" is help only for a bare
// `hearth help`, `hearth skill help`, and `hearth shared connection help`.
func HelpFor(args []string) (string, bool) {
	cut := args
	for i, arg := range args {
		if arg == "--" {
			cut = args[:i]
			break
		}
	}
	if len(cut) == 0 {
		return "", false
	}
	if cut[0] == "--help" || cut[0] == "-h" || cut[0] == "help" {
		return HelpText(), true
	}
	if !askedHelp(cut) {
		return "", false
	}
	return usageFor(cut), true
}

func askedHelp(args []string) bool {
	if len(args) >= 2 && args[0] == "skill" && args[1] == "help" {
		return true
	}
	if len(args) >= 1 && args[0] == "shared" {
		sawConnection := false
		for _, arg := range args[1:] {
			if arg == "connection" {
				sawConnection = true
				continue
			}
			if sawConnection && arg == "help" {
				return true
			}
		}
	}
	for _, arg := range args {
		if arg == "--help" || arg == "-h" {
			return true
		}
	}
	return false
}

func usageFor(args []string) string {
	if len(args) == 0 {
		return HelpText()
	}
	switch args[0] {
	case "status":
		return "usage: hearth status [target] [--json]"
	case "events":
		return "usage: hearth events --follow --json"
	case "start":
		return "usage: hearth start <service|group> [--wait] [--json] [--dry-run] [--kill-unowned] [--yes]"
	case "stop":
		return "usage: hearth stop <service|group> [--wait] [--json] [--dry-run] [--yes]"
	case "restart":
		return "usage: hearth restart <service|group> [--wait] [--json] [--dry-run] [--yes]"
	case "logs":
		return "usage: hearth logs <service> [--tail N] [--follow] [--json]"
	case "urls":
		return "usage: hearth urls [target] [--json]"
	case "operation":
		return "usage: hearth operation get|watch <operationId> [--json]"
	case "doctor":
		return "usage: hearth doctor [--json]"
	case "cleanup":
		return "usage: hearth cleanup"
	case "manager":
		return "usage: hearth manager ensure|status|stop|restart|reload [--json] [--yes]"
	case "daemon":
		return "usage: hearth daemon --root <path>"
	case "smp":
		return "usage: hearth smp"
	case "shared":
		if sharedConnectionHelp(args[1:]) {
			return connectionUsage
		}
		return sharedUsage
	case "update":
		return "usage: hearth update [--check] [--json] [--force]"
	case "mcp":
		for _, arg := range args[1:] {
			if arg == "install" {
				return "usage: hearth mcp install [--name <name>] [--key <topLevelKey>] [--json] <config-file>..."
			}
		}
		return "usage: hearth mcp"
	case "skill":
		return skillUsage
	case "version":
		return versionUsage
	case "schema":
		return schemaUsage
	case "workspaces":
		return workspacesUsage
	default:
		return HelpText()
	}
}

// JSONRequested reports whether args contain --json.
func JSONRequested(args []string) bool { return hasFlag(args, "--json") }

// JSONError is the contract error object for code, as one JSON line.
func JSONError(code int, message, hint string) string {
	return formatJSONError(code, message, hint)
}

// ReportFailure prints err as text, or as the JSON error object when args
// contain --json, and returns the contract exit code.
func ReportFailure(io *Io, args []string, err error) int {
	return emitFailure(io, hasFlag(args, "--json"), err)
}

func emitFailure(io *Io, jsonMode bool, err error) int {
	code := exitCodeOf(err)
	message := ""
	hint := ""
	if err != nil {
		message = err.Error()
	}
	var local *LocalctlError
	if errors.As(err, &local) {
		hint = local.Hint
	}
	if jsonMode {
		out(io, formatJSONError(code, message, hint))
		return code
	}
	errOut(io, message)
	return code
}

// requireYes allows a destructive action when --yes was passed or stdin is a
// TTY (Confirm is set). Off a TTY it returns exit 6.
func requireYes(io *Io, yes bool, message string) error {
	if yes || (io != nil && io.Confirm != nil) {
		return nil
	}
	return failErrHint(ExitNeedsConfirm, message, "pass --yes")
}

func hasFlag(args []string, name string) bool {
	for _, arg := range args {
		if arg == name {
			return true
		}
	}
	return false
}

// finishNew prints a contract error for a new command and returns the exit
// code with a nil error so Main does not print it again. --json writes the
// error object to stdout. Text writes `hearth: …` and an optional hint to stderr.
func finishNew(io *Io, jsonMode bool, err error) (int, error) {
	code := exitCodeOf(err)
	message := ""
	hint := ""
	if err != nil {
		message = err.Error()
	}
	var local *LocalctlError
	if errors.As(err, &local) {
		hint = local.Hint
	}
	if jsonMode {
		out(io, formatJSONError(code, message, hint))
		return code, nil
	}
	errOut(io, "hearth: "+message)
	if hint != "" {
		errOut(io, "hint: "+hint)
	}
	return code, nil
}

func formatJSONError(code int, message, hint string) string {
	body := omap.New()
	body.Set("code", codeName(code))
	body.Set("exit", int64(code))
	body.Set("message", message)
	if hint != "" {
		body.Set("hint", hint)
	}
	doc := omap.New()
	doc.Set("error", body)
	return compactJSON(doc)
}

func codeName(code int) string {
	switch code {
	case ExitUsage:
		return "usage"
	case ExitUnavailable:
		return "unavailable"
	case ExitProtocol:
		return "protocol"
	case ExitFailed:
		return "failed"
	case ExitNeedsConfirm:
		return "needs-confirm"
	case ExitUnauthorized:
		return "unauthorized"
	case ExitConflict:
		return "conflict"
	default:
		return "failed-generic"
	}
}

func versionCommand(rest []string, io *Io) (int, error) {
	if isHelp(rest) {
		out(io, versionUsage)
		return 0, nil
	}
	flags, err := ParseCommandFlags(rest, []Flag{FlagJSON})
	if err != nil {
		return finishNew(io, hasFlag(rest, "--json"), err)
	}
	if len(flags.Positionals) > 0 {
		return finishNew(io, flags.JSON, usageErr(versionUsage))
	}
	if !flags.JSON {
		out(io, "hearth "+Version)
		return 0, nil
	}
	doc := omap.New()
	doc.Set("name", "hearth")
	doc.Set("version", Version)
	doc.Set("schemaVersion", int64(schema.Version))
	doc.Set("commit", versionCommit())
	out(io, compactJSON(jsonValue(doc)))
	return 0, nil
}

func versionCommit() any {
	info, ok := debug.ReadBuildInfo()
	if !ok {
		return nil
	}
	for _, setting := range info.Settings {
		if setting.Key == "vcs.revision" && setting.Value != "" {
			return setting.Value
		}
	}
	return nil
}

func schemaCommand(rest []string, io *Io) (int, error) {
	if isHelp(rest) {
		out(io, schemaUsage)
		return 0, nil
	}
	flags, err := ParseCommandFlags(rest, []Flag{FlagJSON})
	if err != nil {
		return finishNew(io, hasFlag(rest, "--json"), err)
	}
	path := strings.Join(flags.Positionals, " ")
	if path == "" {
		if flags.JSON {
			out(io, schema.DocumentJSON())
			return 0, nil
		}
		for _, line := range strings.Split(schema.Text(), "\n") {
			out(io, line)
		}
		return 0, nil
	}
	if flags.JSON {
		text, ok := schema.EntryJSON(path)
		if !ok {
			return finishNew(io, true, usageErr("unknown command: "+path))
		}
		out(io, text)
		return 0, nil
	}
	command, ok := schema.Lookup(path)
	if !ok {
		return finishNew(io, false, usageErr("unknown command: "+path))
	}
	out(io, schemaLine(command))
	return 0, nil
}

func schemaLine(command schema.Command) string {
	kind := "R"
	switch {
	case command.Destructive:
		kind = "D"
	case command.Mutates:
		kind = "M"
	}
	return fmt.Sprintf("%s  %s  %s", command.Path, kind, command.Summary)
}

func defaultSkillDir() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return "", failErr(ExitFailed, "HOME is not set")
	}
	return filepath.Join(home, ".agents", "skills", "hearth"), nil
}

// resolveSkillDest picks the skill directory. A relative --dest joins root.
// The default (~/.agents/skills/hearth) and any absolute dest do not. A dest
// whose base name is SKILL.md names that file's parent.
func resolveSkillDest(root string, dest *string) (string, error) {
	var path string
	if dest == nil {
		resolved, err := defaultSkillDir()
		if err != nil {
			return "", err
		}
		path = resolved
	} else {
		path = *dest
		if !filepath.IsAbs(path) {
			path = filepath.Join(root, path)
		}
	}
	if filepath.Base(path) == "SKILL.md" {
		path = filepath.Dir(path)
	}
	return path, nil
}

type skillFile struct {
	body string
	exec bool
}

type skillDiff struct {
	missing   []string
	different []string
	extra     []string
}

func (d skillDiff) inSync() bool {
	return len(d.missing) == 0 && len(d.different) == 0 && len(d.extra) == 0
}

func embeddedSkillFiles() map[string]skillFile {
	files := map[string]skillFile{
		"SKILL.md": {body: skillMarkdown, exec: false},
	}
	for _, script := range skillScripts {
		files["scripts/"+script.name] = skillFile{body: script.body, exec: true}
	}
	return files
}

func diffSkill(dest string) skillDiff {
	expected := embeddedSkillFiles()
	found := map[string]bool{}
	var extra []string
	_ = filepath.WalkDir(dest, func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		rel, relErr := filepath.Rel(dest, path)
		if relErr != nil || rel == "." {
			return nil
		}
		rel = filepath.ToSlash(rel)
		if entry.IsDir() {
			if rel == "scripts" {
				return nil
			}
			extra = append(extra, rel)
			return fs.SkipDir
		}
		if _, ok := expected[rel]; ok {
			found[rel] = true
			return nil
		}
		extra = append(extra, rel)
		return nil
	})
	var missing, different []string
	for name, want := range expected {
		if !found[name] {
			missing = append(missing, name)
			continue
		}
		path := filepath.Join(dest, filepath.FromSlash(name))
		body, readErr := os.ReadFile(path)
		info, statErr := os.Stat(path)
		exec := statErr == nil && info.Mode()&0o111 != 0
		if readErr != nil || string(body) != want.body || exec != want.exec {
			different = append(different, name)
		}
	}
	sort.Strings(missing)
	sort.Strings(different)
	sort.Strings(extra)
	return skillDiff{missing: missing, different: different, extra: extra}
}

func skillCheckCommand(root string, rest []string, io *Io) (int, error) {
	flags, err := ParseCommandFlags(rest, []Flag{FlagDest, FlagJSON})
	if err != nil {
		return finishNew(io, hasFlag(rest, "--json"), err)
	}
	if len(flags.Positionals) > 0 {
		return finishNew(io, flags.JSON, usageErr("skill check takes no positional arguments"))
	}
	dest, err := resolveSkillDest(root, flags.Dest)
	if err != nil {
		return finishNew(io, flags.JSON, err)
	}
	info, statErr := os.Stat(dest)
	if statErr != nil || !info.IsDir() {
		return finishNew(io, flags.JSON, failErrHint(ExitUnavailable, "skill directory is missing: "+dest, "run: hearth skill install"))
	}
	diff := diffSkill(dest)
	emitSkillCheck(io, dest, diff, flags.JSON)
	if !diff.inSync() {
		return ExitConflict, nil
	}
	return 0, nil
}

func emitSkillCheck(io *Io, dest string, diff skillDiff, jsonMode bool) {
	if jsonMode {
		doc := omap.New()
		doc.Set("dest", dest)
		doc.Set("inSync", diff.inSync())
		doc.Set("missing", stringList(diff.missing))
		doc.Set("different", stringList(diff.different))
		doc.Set("extra", stringList(diff.extra))
		out(io, compactJSON(doc))
		return
	}
	if diff.inSync() {
		out(io, "in sync "+dest)
		return
	}
	out(io, "drift "+dest)
	for _, name := range diff.missing {
		out(io, "missing "+name)
	}
	for _, name := range diff.different {
		out(io, "different "+name)
	}
	for _, name := range diff.extra {
		out(io, "extra "+name)
	}
}

func stringList(items []string) []any {
	if items == nil {
		items = []string{}
	}
	out := make([]any, len(items))
	for i, item := range items {
		out[i] = item
	}
	return out
}

// emitSharedJSON prints payload as pretty JSON. mask runs the redact helper
// over the whole tree. `shared status` and `shared installed` mask unless
// --reveal; --mask is accepted and changes nothing.
func emitSharedJSON(io *Io, payload any, mask bool) {
	value := jsonValue(payload)
	if mask {
		value = maskOrdered(value)
	}
	out(io, prettyJSON(value))
}

// maskOrdered applies redact.Values. jsonValue's *omap.OMap keeps key order.
func maskOrdered(v any) any {
	return redact.Values(v)
}
