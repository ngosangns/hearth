// Package schema is the embedded JSON Schema (draft 2020-12) catalog for
// hearth CLI output, schemaVersion 1.
//
// v1 is the JSON each command printed on 2026-10-23, plus the additive
// commands from the agent CLI contract (version, schema, skill check,
// shared connection, workspaces list). Connection objects keep the same keys
// and types. `shared list`, `shared status`, and `shared installed` mask every
// secret-shaped string unless `--reveal`. `--mask` is accepted and changes
// nothing. `shared connection` and `shared attach` mask unless `--reveal`.
// `manager ensure` and `shared ensure` mutate: they can start a daemon.
package schema

import (
	"encoding/json"
	"fmt"
	"io/fs"
	"strings"
)

// Version is the CLI schema version. It is not the hearth release version.
const Version = 1

// Command is one schema entry. Output is the embedded JSON Schema for that
// command's --json document (or a string schema when the command prints text).
type Command struct {
	Path        string
	Summary     string
	Mutates     bool
	Destructive bool
	Flags       []string
	output      json.RawMessage
}

var commands []Command
var byPath map[string]int
var files map[string]json.RawMessage
var errorSchema json.RawMessage

func init() {
	files = map[string]json.RawMessage{}
	entries, err := fs.ReadDir(schemaFS, "v1")
	if err != nil {
		panic("schema: " + err.Error())
	}
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".json") {
			continue
		}
		body, err := schemaFS.ReadFile("v1/" + entry.Name())
		if err != nil {
			panic("schema: " + err.Error())
		}
		if !json.Valid(body) {
			panic("schema: " + entry.Name() + " is not valid JSON")
		}
		files[entry.Name()] = body
	}
	errorSchema = mustFile("error.json")
	commands = []Command{
		read("status", "current state of one service, a group, or all", "status.json", "--json"),
		read("urls", "where each service can be reached", "urls.json", "--json"),
		read("logs", "a service log tail", "logs.json", "--json", "--tail", "--follow"),
		read("events", "daemon changes as newline-delimited JSON", "events.json", "--json", "--follow"),
		read("operation get", "one operation", "operation.json", "--json"),
		read("operation watch", "wait for one operation and print it", "operation.json", "--json"),
		read("doctor", "environment and catalog diagnostics", "doctor.json", "--json"),
		mutate("cleanup", "remove stale runtime state no live daemon owns", false, "text.json"),
		mutate("manager ensure", "find or start the project daemon and print its connection", false, "ensure.json", "--json"),
		read("manager status", "daemon pid, port, and version", "manager-status.json", "--json"),
		mutate("manager stop", "stop the daemon, its services, and detach shared services", true, "operation.json", "--json", "--yes"),
		mutate("manager restart", "restart the daemon and leave services running", false, "ensure.json", "--json"),
		mutate("manager reload", "reload the project catalog", false, "reload.json", "--json"),
		mutate("daemon", "run the project daemon in the foreground", false, "text.json"),
		mutate("smp", "run the machine-global shared-services daemon", false, "text.json"),
		read("shared list", "remote shared-service recipes (secrets masked unless --reveal; --mask is accepted and changes nothing)", "shared-list.json", "--json", "--mask", "--reveal"),
		read("shared installed", "installed shared instances (secrets masked unless --reveal; --mask is accepted and changes nothing)", "shared-installed.json", "--json", "--mask", "--reveal"),
		read("shared status", "live shared instances (secrets masked unless --reveal; --mask is accepted and changes nothing)", "shared-status.json", "--json", "--mask", "--reveal"),
		read("shared connection", "this project's connection for an attached shared service (secrets masked unless --reveal)", "shared-connection.json", "--json", "--reveal"),
		mutate("shared ensure", "find or start smp and print its connection", false, "ensure.json", "--json"),
		mutate("shared attach", "attach this project to a shared instance (connection secrets masked unless --reveal)", false, "attach.json", "--json", "--reveal"),
		mutate("shared detach", "detach this project; --stop-if-unused stops the instance when nothing else is attached", true, "text.json", "--json", "--stop-if-unused", "--yes"),
		read("shared probe", "exit 0 when the instance is ready and this project is attached", "text.json"),
		mutate("shared install", "download and install a shared instance", false, "text.json"),
		mutate("shared start", "install if needed and start a shared instance", false, "text.json"),
		mutate("shared stop", "stop a shared instance", false, "text.json"),
		mutate("shared remove", "delete a shared instance and its data", true, "text.json", "--json", "--force", "--yes"),
		mutate("start", "start a service or group; --kill-unowned kills an unowned process holding the port (only after the user asked)", false, "lifecycle.json", "--json", "--wait", "--dry-run", "--kill-unowned", "--yes"),
		mutate("stop", "stop a service or group", false, "lifecycle.json", "--json", "--wait", "--dry-run", "--yes"),
		mutate("restart", "restart a service or group", false, "lifecycle.json", "--json", "--wait", "--dry-run", "--yes"),
		mutate("update", "download and install a GitHub release; --check only reads", false, "update.json", "--json", "--check", "--force"),
		read("mcp", "serve the MCP tool surface over stdio", "text.json"),
		mutate("mcp install", "register this binary in an MCP client config", false, "mcp-install.json", "--json", "--name", "--key"),
		mutate("skill install", "write the embedded skill into a directory", false, "text.json", "--dest"),
		read("skill check", "compare the installed skill with the embedded copy", "skill-check.json", "--json", "--dest"),
		read("version", "name, version, schemaVersion, and commit", "version.json", "--json"),
		read("schema", "command list with JSON Schemas", "schema.json", "--json"),
		read("workspaces list", "workspace folders the Hearth window knows", "workspaces.json", "--json"),
	}
	byPath = map[string]int{}
	for i, command := range commands {
		if _, ok := byPath[command.Path]; ok {
			panic("schema: duplicate command " + command.Path)
		}
		byPath[command.Path] = i
	}
}

func read(path, summary, file string, flags ...string) Command {
	return command(path, summary, false, false, file, flags)
}

func mutate(path, summary string, destructive bool, file string, flags ...string) Command {
	return command(path, summary, true, destructive, file, flags)
}

func command(path, summary string, mutates, destructive bool, file string, flags []string) Command {
	if flags == nil {
		flags = []string{}
	}
	return Command{
		Path:        path,
		Summary:     summary,
		Mutates:     mutates,
		Destructive: destructive,
		Flags:       flags,
		output:      mustFile(file),
	}
}

func mustFile(name string) json.RawMessage {
	body, ok := files[name]
	if !ok {
		panic("schema: missing v1/" + name)
	}
	return body
}

// Commands returns the catalog in display order.
func Commands() []Command {
	out := make([]Command, len(commands))
	copy(out, commands)
	return out
}

// Paths returns every command path in display order.
func Paths() []string {
	out := make([]string, len(commands))
	for i, command := range commands {
		out[i] = command.Path
	}
	return out
}

// Lookup returns the command for a space-separated path.
func Lookup(path string) (Command, bool) {
	index, ok := byPath[path]
	if !ok {
		return Command{}, false
	}
	return commands[index], true
}

// OutputSchema returns the embedded JSON Schema for path.
func OutputSchema(path string) (json.RawMessage, bool) {
	command, ok := Lookup(path)
	if !ok {
		return nil, false
	}
	return command.output, true
}

// ErrorSchema is the schema of {"error":{code,exit,message,hint?}}.
func ErrorSchema() json.RawMessage { return errorSchema }

// Files returns every embedded schema file, keyed by its base name.
func Files() map[string]json.RawMessage {
	out := make(map[string]json.RawMessage, len(files))
	for name, body := range files {
		out[name] = body
	}
	return out
}

// DocumentJSON is `hearth schema --json`: name, schemaVersion, commands, error.
func DocumentJSON() string {
	var b strings.Builder
	b.WriteString(`{"name":"hearth","schemaVersion":`)
	b.WriteString(fmt.Sprintf("%d", Version))
	b.WriteString(`,"commands":{`)
	for i, command := range commands {
		if i > 0 {
			b.WriteByte(',')
		}
		b.WriteString(jsonString(command.Path))
		b.WriteByte(':')
		writeEntry(&b, command)
	}
	b.WriteString(`},"error":`)
	b.Write(errorSchema)
	b.WriteByte('}')
	return b.String()
}

// EntryJSON is `hearth schema <command> --json`. The bool is false when path
// is not in the catalog.
func EntryJSON(path string) (string, bool) {
	command, ok := Lookup(path)
	if !ok {
		return "", false
	}
	var b strings.Builder
	writeEntry(&b, command)
	return b.String(), true
}

func writeEntry(b *strings.Builder, command Command) {
	b.WriteString(`{"summary":`)
	b.WriteString(jsonString(command.Summary))
	b.WriteString(`,"mutates":`)
	b.WriteString(jsonBool(command.Mutates))
	b.WriteString(`,"destructive":`)
	b.WriteString(jsonBool(command.Destructive))
	b.WriteString(`,"flags":`)
	encoded, err := json.Marshal(command.Flags)
	if err != nil {
		encoded = []byte("[]")
	}
	b.Write(encoded)
	b.WriteString(`,"output":`)
	b.Write(command.output)
	b.WriteByte('}')
}

// Text is `hearth schema`: one line per command, "path  R|M|D  summary".
func Text() string {
	var b strings.Builder
	for i, command := range commands {
		if i > 0 {
			b.WriteByte('\n')
		}
		kind := "R"
		switch {
		case command.Destructive:
			kind = "D"
		case command.Mutates:
			kind = "M"
		}
		fmt.Fprintf(&b, "%s  %s  %s", command.Path, kind, command.Summary)
	}
	return b.String()
}

func jsonString(s string) string {
	encoded, err := json.Marshal(s)
	if err != nil {
		return `""`
	}
	return string(encoded)
}

func jsonBool(v bool) string {
	if v {
		return "true"
	}
	return "false"
}
