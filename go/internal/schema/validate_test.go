package schema_test

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/cli"
	"github.com/ngosangns/hearth/go/internal/doctor"
	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/schema"
	"github.com/ngosangns/hearth/go/internal/shared"
	"github.com/ngosangns/hearth/go/internal/state"
	"github.com/ngosangns/hearth/go/internal/updater"
)

func validate(doc, instance any) error {
	switch s := doc.(type) {
	case bool:
		if s {
			return nil
		}
		return errors.New("boolean schema false")
	case map[string]any:
		return validateMap(s, instance)
	default:
		return fmt.Errorf("schema is %T", doc)
	}
}

func validateMap(doc map[string]any, instance any) error {
	if oneOf, ok := doc["oneOf"].([]any); ok {
		matches := 0
		var first error
		for _, branch := range oneOf {
			err := validate(branch, instance)
			if err == nil {
				matches++
			} else if first == nil {
				first = err
			}
		}
		if matches != 1 {
			return fmt.Errorf("oneOf matched %d (%v)", matches, first)
		}
	}
	if anyOf, ok := doc["anyOf"].([]any); ok {
		okBranch := false
		var first error
		for _, branch := range anyOf {
			err := validate(branch, instance)
			if err == nil {
				okBranch = true
				break
			}
			if first == nil {
				first = err
			}
		}
		if !okBranch {
			return fmt.Errorf("anyOf failed: %v", first)
		}
	}
	if types, ok := schemaTypes(doc["type"]); ok {
		if !matchesType(types, instance) {
			return fmt.Errorf("%v is not %v", jsonKind(instance), types)
		}
	}
	if constValue, ok := doc["const"]; ok {
		if !sameValue(constValue, instance) {
			return fmt.Errorf("const %v != %v", constValue, instance)
		}
	}
	if enum, ok := doc["enum"].([]any); ok {
		found := false
		for _, item := range enum {
			if sameValue(item, instance) {
				found = true
				break
			}
		}
		if !found {
			return fmt.Errorf("%v is not in enum", instance)
		}
	}
	if required, ok := doc["required"].([]any); ok {
		if object, isObject := instance.(map[string]any); isObject {
			for _, name := range required {
				key, _ := name.(string)
				if _, present := object[key]; !present {
					return fmt.Errorf("missing %s", key)
				}
			}
		}
	}
	object, isObject := instance.(map[string]any)
	if props, ok := doc["properties"].(map[string]any); ok && isObject {
		for key, sub := range props {
			if value, present := object[key]; present {
				if err := validate(sub, value); err != nil {
					return fmt.Errorf("%s: %w", key, err)
				}
			}
		}
	}
	if isObject {
		if err := checkAdditional(doc, object); err != nil {
			return err
		}
	}
	if items, ok := doc["items"]; ok {
		if list, isList := instance.([]any); isList {
			for i, item := range list {
				if err := validate(items, item); err != nil {
					return fmt.Errorf("[%d]: %w", i, err)
				}
			}
		}
	}
	return nil
}

func checkAdditional(doc map[string]any, object map[string]any) error {
	props, _ := doc["properties"].(map[string]any)
	extra, hasExtra := doc["additionalProperties"]
	for key, value := range object {
		if _, known := props[key]; known {
			continue
		}
		if !hasExtra {
			continue
		}
		switch rule := extra.(type) {
		case bool:
			if !rule {
				return fmt.Errorf("unexpected property %s", key)
			}
		default:
			if err := validate(rule, value); err != nil {
				return fmt.Errorf("%s: %w", key, err)
			}
		}
	}
	return nil
}

func schemaTypes(raw any) ([]string, bool) {
	switch t := raw.(type) {
	case string:
		return []string{t}, true
	case []any:
		out := make([]string, 0, len(t))
		for _, item := range t {
			text, ok := item.(string)
			if !ok {
				return nil, false
			}
			out = append(out, text)
		}
		return out, true
	default:
		return nil, false
	}
}

func matchesType(types []string, instance any) bool {
	for _, kind := range types {
		switch kind {
		case "object":
			if _, ok := instance.(map[string]any); ok {
				return true
			}
		case "array":
			if _, ok := instance.([]any); ok {
				return true
			}
		case "string":
			if _, ok := instance.(string); ok {
				return true
			}
		case "boolean":
			if _, ok := instance.(bool); ok {
				return true
			}
		case "null":
			if instance == nil {
				return true
			}
		case "number":
			if isNumber(instance) {
				return true
			}
		case "integer":
			if isInteger(instance) {
				return true
			}
		}
	}
	return false
}

func isNumber(v any) bool {
	switch v.(type) {
	case float64, json.Number, int, int64:
		return true
	default:
		return false
	}
}

func isInteger(v any) bool {
	switch n := v.(type) {
	case float64:
		return !math.IsInf(n, 0) && n == math.Trunc(n)
	case json.Number:
		_, err := n.Int64()
		return err == nil
	case int, int64:
		return true
	default:
		return false
	}
}

func sameValue(left, right any) bool {
	if isNumber(left) && isNumber(right) {
		return numberOf(left) == numberOf(right)
	}
	return fmt.Sprint(left) == fmt.Sprint(right) && jsonKind(left) == jsonKind(right)
}

func numberOf(v any) float64 {
	switch n := v.(type) {
	case float64:
		return n
	case json.Number:
		f, _ := n.Float64()
		return f
	case int:
		return float64(n)
	case int64:
		return float64(n)
	default:
		return 0
	}
}

func jsonKind(v any) string {
	switch v.(type) {
	case nil:
		return "null"
	case string:
		return "string"
	case bool:
		return "boolean"
	case float64, json.Number, int, int64:
		return "number"
	case map[string]any:
		return "object"
	case []any:
		return "array"
	default:
		return fmt.Sprintf("%T", v)
	}
}

func parseSchema(t *testing.T, body []byte) any {
	t.Helper()
	var doc any
	if err := json.Unmarshal(body, &doc); err != nil {
		t.Fatalf("schema: %v", err)
	}
	return doc
}

func TestValidatorBasics(t *testing.T) {
	if err := validate(map[string]any{}, "anything"); err != nil {
		t.Fatal(err)
	}
	if err := validate(map[string]any{"type": "integer"}, 1.0); err != nil {
		t.Fatal(err)
	}
	if err := validate(map[string]any{"type": "integer"}, 1.5); err == nil {
		t.Fatal("expected 1.5 to fail integer")
	}
	schemaDoc := map[string]any{
		"type":                 "object",
		"additionalProperties": false,
		"properties":           map[string]any{"a": map[string]any{"const": 1.0}},
	}
	if err := validate(schemaDoc, map[string]any{"a": 1.0, "b": true}); err == nil {
		t.Fatal("expected an extra property to fail")
	}
	one := map[string]any{"oneOf": []any{
		map[string]any{"type": "string"},
		map[string]any{"const": "x"},
	}}
	if err := validate(one, "x"); err == nil {
		t.Fatal("expected oneOf to reject two matches")
	}
}

func TestEmbeddedSchemasAreJSON(t *testing.T) {
	for name, body := range schema.Files() {
		if !json.Valid(body) {
			t.Errorf("%s is not valid JSON", name)
		}
	}
	if len(schema.Files()) < 20 {
		t.Fatalf("embedded %d schema files", len(schema.Files()))
	}
}

func TestGoldensMatchEmbeddedSchemas(t *testing.T) {
	paths := map[string]string{
		"status.json":               "status",
		"urls.json":                 "urls",
		"logs.json":                 "logs",
		"events-connection.json":    "events",
		"events-snapshot.json":      "events",
		"events-unavailable.json":   "events",
		"events-manager.json":       "events",
		"operation.json":            "operation get",
		"doctor.json":               "doctor",
		"ensure.json":               "manager ensure",
		"manager-status.json":       "manager status",
		"reload.json":               "manager reload",
		"shared-list.json":          "shared list",
		"shared-installed.json":     "shared installed",
		"shared-status.json":        "shared status",
		"shared-connection.json":    "shared connection",
		"attach.json":               "shared attach",
		"lifecycle-operations.json": "start",
		"lifecycle-operation.json":  "restart",
		"dry-run.json":              "start",
		"workspaces.json":           "workspaces list",
		"update.json":               "update",
		"skill-check.json":          "skill check",
		"version.json":              "version",
		"mcp-install.json":          "mcp install",
		"text.json":                 "cleanup",
		"schema.json":               "schema",
	}
	for file, path := range paths {
		body := readGolden(t, file)
		raw, ok := schema.OutputSchema(path)
		if !ok {
			t.Fatalf("no schema for %s", path)
		}
		checkGolden(t, file, raw, body)
	}
	checkGolden(t, "error.json", schema.ErrorSchema(), readGolden(t, "error.json"))
}

func readGolden(t *testing.T, name string) []byte {
	t.Helper()
	body, err := os.ReadFile(filepath.Join("testdata", "contract", name))
	if err != nil {
		t.Fatal(err)
	}
	return body
}

func checkGolden(t *testing.T, name string, raw, body []byte) {
	t.Helper()
	var value any
	if err := json.Unmarshal(body, &value); err != nil {
		t.Fatalf("%s: %v", name, err)
	}
	if err := validate(parseSchema(t, raw), value); err != nil {
		t.Errorf("%s: %v", name, err)
	}
}

func TestHelpCoversEverySchemaCommand(t *testing.T) {
	help := cli.HelpText()
	lines := strings.Split(help, "\n")
	for _, path := range schema.Paths() {
		if !covers(lines, path) {
			t.Errorf("help does not cover %q", path)
		}
	}
	for _, path := range helpCommands(help) {
		if _, ok := schema.Lookup(path); !ok {
			t.Errorf("help command %q is not in the schema", path)
		}
	}
	stop, ok := schema.Lookup("manager stop")
	if !ok || !stop.Destructive {
		t.Fatal("manager stop should be destructive")
	}
	detach, ok := schema.Lookup("shared detach")
	if !ok || !detach.Destructive {
		t.Fatal("shared detach should be destructive")
	}
	remove, ok := schema.Lookup("shared remove")
	if !ok || !remove.Destructive {
		t.Fatal("shared remove should be destructive")
	}
	start, ok := schema.Lookup("start")
	if !ok || !start.Mutates || start.Destructive {
		t.Fatal("start should mutate and not be destructive")
	}
	if !strings.Contains(start.Summary, "--kill-unowned") {
		t.Fatal("start summary should mention --kill-unowned")
	}
	for _, path := range []string{"shared list", "shared status", "shared installed", "manager ensure", "manager restart", "shared ensure"} {
		command, ok := schema.Lookup(path)
		if !ok {
			t.Fatal(path)
		}
		flags := " " + strings.Join(command.Flags, " ") + " "
		if !strings.Contains(flags, " --mask ") || !strings.Contains(flags, " --reveal ") {
			t.Fatalf("%s flags = %v", path, command.Flags)
		}
		if !strings.Contains(command.Summary, "masked") || !strings.Contains(command.Summary, "--reveal") {
			t.Fatalf("%s summary = %q", path, command.Summary)
		}
	}
	for _, path := range []string{"manager ensure", "shared ensure"} {
		command, ok := schema.Lookup(path)
		if !ok || !command.Mutates || command.Destructive {
			t.Fatalf("%s should mutate and not be destructive", path)
		}
	}
	for _, path := range []string{"start", "stop", "restart"} {
		command, ok := schema.Lookup(path)
		if !ok || !containsFlag(command.Flags, "--dry-run") {
			t.Fatalf("%s flags = %v", path, command.Flags)
		}
	}
	stop, _ = schema.Lookup("manager stop")
	if !containsFlag(stop.Flags, "--yes") {
		t.Fatalf("manager stop flags = %v", stop.Flags)
	}
}

func containsFlag(flags []string, name string) bool {
	for _, flag := range flags {
		if flag == name {
			return true
		}
	}
	return false
}

func covers(lines []string, path string) bool {
	words := strings.Fields(path)
	for _, line := range lines {
		ok := true
		for _, word := range words {
			if !strings.Contains(line, word) {
				ok = false
				break
			}
		}
		if ok {
			return true
		}
	}
	return false
}

var bracketPattern = regexp.MustCompile(`\[[^\]]*\]|<[^>]*>`)

func helpCommands(help string) []string {
	var out []string
	for _, line := range strings.Split(help, "\n") {
		if !strings.HasPrefix(line, "  ") || strings.HasPrefix(line, "   ") {
			continue
		}
		body := line[2:]
		if parts := regexp.MustCompile(`\s{2,}`).Split(body, 2); len(parts) > 0 {
			body = parts[0]
		}
		body = bracketPattern.ReplaceAllString(body, " ")
		var tokens []string
		for _, token := range strings.Fields(body) {
			if token == "..." || strings.HasPrefix(token, "--") {
				continue
			}
			tokens = append(tokens, token)
		}
		if len(tokens) == 0 {
			continue
		}
		prefix := []string{}
		last := tokens[0]
		if len(tokens) > 1 {
			prefix = tokens[:len(tokens)-1]
			last = tokens[len(tokens)-1]
		}
		for _, alt := range strings.Split(last, "|") {
			name := strings.TrimSpace(strings.Join(append(append([]string{}, prefix...), alt), " "))
			if name != "" {
				out = append(out, name)
			}
		}
	}
	return out
}

func TestLiveVersionSchemaAndSkill(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	code, outLines, errLines := runMain(t, t.TempDir(), "version", "--json")
	if code != 0 {
		t.Fatalf("version: %d %v", code, errLines)
	}
	checkOutput(t, "version", schema.OutputSchema, strings.Join(outLines, "\n"))
	var version map[string]any
	if err := json.Unmarshal([]byte(outLines[0]), &version); err != nil {
		t.Fatal(err)
	}
	if version["name"] != "hearth" || version["schemaVersion"] != 1.0 {
		t.Fatalf("version = %#v", version)
	}
	if _, ok := version["commit"]; !ok {
		t.Fatal("commit key missing")
	}

	code, outLines, errLines = runMain(t, t.TempDir(), "schema", "--json")
	if code != 0 {
		t.Fatalf("schema: %d %v", code, errLines)
	}
	raw, _ := schema.OutputSchema("schema")
	checkRaw(t, "schema --json", raw, strings.Join(outLines, "\n"))

	code, outLines, errLines = runMain(t, t.TempDir(), "schema", "shared", "connection")
	if code != 0 || len(outLines) != 1 || !strings.Contains(outLines[0], "shared connection  R") {
		t.Fatalf("schema text: %d %v %v", code, outLines, errLines)
	}
	code, outLines, errLines = runMain(t, t.TempDir(), "schema", "status", "extra", "--json")
	if code != 2 {
		t.Fatalf("unknown schema: %d %v %v", code, outLines, errLines)
	}
	checkRaw(t, "schema error", schema.ErrorSchema(), outLines[0])

	root := t.TempDir()
	code, _, errLines = runMain(t, root, "skill", "install", "--dest", "skill")
	if code != 0 {
		t.Fatalf("install: %d %v", code, errLines)
	}
	code, outLines, errLines = runMain(t, root, "skill", "check", "--dest", "skill", "--json")
	if code != 0 {
		t.Fatalf("check: %d %v %v", code, outLines, errLines)
	}
	checkOutput(t, "skill check", schema.OutputSchema, outLines[0])
}

func TestLiveSharedAndUpdate(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	root := t.TempDir()
	projectID := shared.ProjectID(root)
	restore := cli.SetSharedInstanceLoader(func(context.Context) (map[string]any, error) {
		return map[string]any{
			"instances": []any{map[string]any{
				"id":           "minio@1",
				"name":         "minio",
				"version":      "1",
				"port":         43100,
				"extraPorts":   []any{},
				"installState": "installed",
				"installError": nil,
				"state":        map[string]any{"actualState": "ready"},
				"attachments": []any{map[string]any{
					"projectId":   projectID,
					"projectRoot": root,
					"provisioned": true,
					"connection": map[string]any{
						"url": "http://user:fake-secret-123@127.0.0.1:9000",
						"env": map[string]any{"AWS_SECRET_ACCESS_KEY": "fake-secret-123", "S3_BUCKET": "photos"},
					},
				}},
			}},
		}, nil
	})
	t.Cleanup(restore)

	code, outLines, errLines := runShared(t, root, "status", "--json")
	if code != 0 {
		t.Fatalf("status: %d %v", code, errLines)
	}
	checkOutput(t, "shared status", schema.OutputSchema, strings.Join(outLines, "\n"))
	if strings.Contains(strings.Join(outLines, "\n"), "fake-secret-123") {
		t.Fatal("default status json leaked a connection secret")
	}
	code, outLines, errLines = runShared(t, root, "status", "--json", "--reveal")
	if code != 0 {
		t.Fatalf("status --reveal: %d %v", code, errLines)
	}
	checkOutput(t, "shared status", schema.OutputSchema, strings.Join(outLines, "\n"))
	if !strings.Contains(strings.Join(outLines, "\n"), "fake-secret-123") {
		t.Fatal("status --reveal hid a connection value")
	}
	code, outLines, errLines = runShared(t, root, "status", "--json", "--mask")
	if code != 0 {
		t.Fatalf("status --mask: %d %v", code, errLines)
	}
	checkOutput(t, "shared status", schema.OutputSchema, strings.Join(outLines, "\n"))
	if strings.Contains(strings.Join(outLines, "\n"), "fake-secret-123") {
		t.Fatal("status --mask left a connection secret")
	}
	code, outLines, errLines = runShared(t, root, "status", "--mask", "--reveal")
	if code != 2 {
		t.Fatalf("mask+reveal: %d %v %v", code, outLines, errLines)
	}

	code, outLines, errLines = runShared(t, root, "connection", "minio", "--json")
	if code != 0 {
		t.Fatalf("connection: %d %v %v", code, outLines, errLines)
	}
	checkOutput(t, "shared connection", schema.OutputSchema, outLines[0])
	if strings.Contains(outLines[0], "fake-secret-123") {
		t.Fatal("connection json leaked a secret")
	}

	reg, err := shared.LoadRegistry(fileio.New(false), shared.SharedRoot())
	if err != nil {
		t.Fatal(err)
	}
	url := "http://127.0.0.1:9000"
	if err := reg.Update(func(data *shared.SharedRegistryData) error {
		data.Instances["minio@1"] = shared.SharedInstance{
			Name: "minio", Version: "1", Port: 43100, InstallState: shared.InstallInstalled,
			Recipe: shared.SharedRecipe{
				Artifacts: map[string]shared.SharedArtifact{},
				Run:       catalog.CommandSpec{Argv: []string{"minio"}},
				Readiness: shared.RecipeReadiness{PrimaryTcp: true},
				Connection: &shared.SharedConnection{
					URL: &url,
					Env: map[string]string{"S3_BUCKET": "photos"},
				},
			},
			Attachments: map[string]shared.SharedAttachment{},
		}
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	code, outLines, errLines = runShared(t, root, "installed", "--json")
	if code != 0 {
		t.Fatalf("installed: %d %v", code, errLines)
	}
	checkOutput(t, "shared installed", schema.OutputSchema, strings.Join(outLines, "\n"))

	supported := true
	var stdout, stderr bytes.Buffer
	exe := filepath.Join(home, ".local", "share", "hearth", "bin", "hearth-0.1.0")
	err = updater.Run(context.Background(), updater.Options{
		Args:              []string{"--check", "--json"},
		Version:           "0.1.0",
		Stdout:            &stdout,
		Stderr:            &stderr,
		CurrentExe:        exe,
		Home:              home,
		PlatformSupported: &supported,
		Transport: memTransport{doc: `{
			"tag_name":"v0.2.0","draft":false,"prerelease":false,
			"assets":[{"name":"hearth-v0.2.0","state":"uploaded","size":12,
				"digest":"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
				"browser_download_url":"https://github.com/ngosangns/hearth/releases/download/v0.2.0/hearth-v0.2.0"}]
		}`},
	})
	if err != nil {
		t.Fatalf("update: %v\n%s", err, stderr.String())
	}
	checkOutput(t, "update", schema.OutputSchema, strings.TrimSpace(stdout.String()))
}

func TestMutatesAudit(t *testing.T) {
	mutate := map[string]bool{
		"cleanup": true, "manager ensure": true, "manager stop": true, "manager restart": true,
		"manager reload": true, "daemon": true, "smp": true, "shared ensure": true,
		"shared attach": true, "shared detach": true, "shared install": true, "shared start": true,
		"shared stop": true, "shared remove": true, "start": true, "stop": true, "restart": true,
		"update": true, "mcp install": true, "skill install": true,
	}
	destructive := map[string]bool{
		"manager stop": true, "shared detach": true, "shared remove": true,
	}
	seen := map[string]bool{}
	for _, command := range schema.Commands() {
		seen[command.Path] = true
		if command.Mutates != mutate[command.Path] {
			t.Errorf("%s mutates=%v", command.Path, command.Mutates)
		}
		if command.Destructive != destructive[command.Path] {
			t.Errorf("%s destructive=%v", command.Path, command.Destructive)
		}
		if command.Destructive && !command.Mutates {
			t.Errorf("%s is destructive without mutates", command.Path)
		}
	}
	for path := range mutate {
		if !seen[path] {
			t.Errorf("missing mutate entry %s", path)
		}
	}
}

func TestP2JSONDocuments(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	root := t.TempDir()
	checkCLIError(t, "version", root, 2, "usage", "version", "nope", "--json")
	checkCLIError(t, "unknown", root, 2, "usage", "nope", "--json")
	checkCLIError(t, "logs", root, 2, "usage", "logs", "--json")
	checkCLIError(t, "status", root, 3, "unavailable", "status", "--json")
	checkCLIError(t, "manager stop", root, 6, "needs-confirm", "manager", "stop", "--json")

	cat := catalog.ServiceCatalog{
		Services: []catalog.ServiceDefinition{{
			ID: "api",
			Profiles: catalog.ServiceProfiles{
				Run: catalog.ServiceRunProfile{CommandStatus: "verified"},
			},
		}},
		Groups: map[string][]string{},
	}
	runtime := ".hearth/runtime-v1"
	cat.RuntimeDirectory = &runtime
	guard := false
	cat.PrivateFileGuard = &guard
	code, outLines, errLines := runOptions(t, root, cat, "start", "api", "--kill-unowned", "--json")
	checkLines(t, "kill-unowned", code, 6, "needs-confirm", outLines, errLines)
	code, outLines, errLines = runOptions(t, root, cat, "restart", "api", "--dry-run")
	if code != 0 || len(errLines) != 0 || len(outLines) != 1 {
		t.Fatalf("dry-run: %d %v %v", code, outLines, errLines)
	}
	checkOutput(t, "start", schema.OutputSchema, outLines[0])

	code, outLines, errLines = runShared(t, root, "remove", "minio@1", "--json")
	checkLines(t, "remove", code, 6, "needs-confirm", outLines, errLines)
	code, outLines, errLines = runShared(t, root, "detach", "minio@1", "--stop-if-unused", "--json")
	checkLines(t, "detach", code, 6, "needs-confirm", outLines, errLines)
	code, outLines, errLines = runShared(t, root, "status", "--mask", "--reveal", "--json")
	checkLines(t, "mask+reveal", code, 2, "usage", outLines, errLines)

	file := filepath.Join(home, "workspaces.json")
	t.Setenv("HEARTH_WORKSPACE_FILE", file)
	if err := os.WriteFile(file, []byte(`[{"id":"w1","path":"/tmp/app","trusted":true,"addedAt":"2026-01-01T00:00:00Z"}]`), 0o644); err != nil {
		t.Fatal(err)
	}
	code, outLines, errLines = runMain(t, root, "workspaces", "list", "--json")
	if code != 0 {
		t.Fatalf("workspaces: %d %v", code, errLines)
	}
	checkOutput(t, "workspaces list", schema.OutputSchema, outLines[0])
	if err := os.WriteFile(file, []byte(`{"nope":true}`), 0o644); err != nil {
		t.Fatal(err)
	}
	checkCLIError(t, "workspaces", root, 4, "protocol", "workspaces", "list", "--json")

	var stdout, stderr bytes.Buffer
	err := updater.Run(context.Background(), updater.Options{
		Args: []string{"--bogus", "--json"}, Version: "0.1.0",
		Stdout: &stdout, Stderr: &stderr, Home: home, CurrentExe: filepath.Join(home, "hearth"),
	})
	if updater.ExitCode(err) != 2 || stderr.Len() != 0 {
		t.Fatalf("update usage: %v %s", err, stderr.String())
	}
	checkRaw(t, "update usage", schema.ErrorSchema(), strings.TrimSpace(stdout.String()))
}

func checkCLIError(t *testing.T, name, root string, wantExit int, wantCode string, args ...string) {
	t.Helper()
	code, outLines, errLines := runMain(t, root, args...)
	checkLines(t, name, code, wantExit, wantCode, outLines, errLines)
}

func checkLines(t *testing.T, name string, code, wantExit int, wantCode string, outLines, errLines []string) {
	t.Helper()
	if code != wantExit || len(errLines) != 0 || len(outLines) != 1 {
		t.Fatalf("%s: %d out=%v err=%v", name, code, outLines, errLines)
	}
	checkRaw(t, name, schema.ErrorSchema(), outLines[0])
	var doc map[string]any
	if err := json.Unmarshal([]byte(outLines[0]), &doc); err != nil {
		t.Fatal(err)
	}
	errObj := doc["error"].(map[string]any)
	if errObj["code"] != wantCode || errObj["exit"] != float64(wantExit) {
		t.Fatalf("%s error = %#v", name, errObj)
	}
	if wantExit == 6 && errObj["hint"] != "pass --yes" {
		t.Fatalf("%s hint = %#v", name, errObj["hint"])
	}
}

func runOptions(t *testing.T, root string, cat catalog.ServiceCatalog, args ...string) (int, []string, []string) {
	t.Helper()
	var outLines, errLines []string
	code := cli.Main(context.Background(), &cli.Options{
		Catalog:     cat,
		SpawnDaemon: func(string) { t.Fatal("spawned a daemon") },
	}, root, args, &cli.Io{
		Out: func(s string) { outLines = append(outLines, s) },
		Err: func(s string) { errLines = append(errLines, s) },
	})
	return code, outLines, errLines
}

func TestFixtureEncoders(t *testing.T) {
	op := state.Operation{
		ID: "op", RequestID: "req", Kind: state.OperationKindService, Status: state.OpStatusSucceeded,
		CreatedAt: "2026-01-01T00:00:00Z", UpdatedAt: "2026-01-01T00:00:01Z",
		Trace: []state.OperationTraceEntry{{At: "2026-01-01T00:00:00Z", Message: "ok"}},
	}
	checkMarshaled(t, "operation get", op)
	checkMarshaled(t, "logs", state.LogSlice{ServiceID: "api", Data: "hi"})
	checkMarshaled(t, "manager status", state.ManagerInfo{
		ProtocolVersion: 3, InstanceID: "i", Pid: 1, Port: 1, StartedAt: "t", MetadataVersion: 1, RuntimeDirectory: "/rt",
	})
	checkMarshaled(t, "events", state.ManagerEvent{Sequence: 1, At: "t", Type: "service.lifecycle", Data: map[string]interface{}{}})
	checkMarshaled(t, "doctor", doctor.DoctorReport{OK: true, Checks: []doctor.DoctorCheckResult{}, UnresolvedProfiles: []string{}})
	checkMarshaled(t, "manager ensure", map[string]any{
		"instanceId": "i", "port": 1, "token": "fake-secret-123", "protocolVersion": 3, "runtimeDirectory": "/rt", "root": "/p",
	})
	checkMarshaled(t, "events", map[string]any{
		"kind": "snapshot",
		"services": []any{map[string]any{
			"serviceId": "api", "desiredState": "running", "actualState": "ready", "readiness": "ready",
			"generation": 1, "createdAt": "t", "updatedAt": "t",
		}},
	})
	checkMarshaled(t, "manager reload", map[string]any{"stopped": []string{}, "changed": []string{"api"}})
}

type memTransport struct{ doc string }

func (m memTransport) FetchLatest(context.Context) (string, error) { return m.doc, nil }
func (m memTransport) Download(context.Context, string, string) error {
	return errors.New("download should not run")
}

func checkMarshaled(t *testing.T, path string, value any) {
	t.Helper()
	body, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	raw, ok := schema.OutputSchema(path)
	if !ok {
		t.Fatalf("missing %s", path)
	}
	checkRaw(t, path, raw, string(body))
}

func checkOutput(t *testing.T, path string, lookup func(string) (json.RawMessage, bool), body string) {
	t.Helper()
	raw, ok := lookup(path)
	if !ok {
		t.Fatalf("missing schema %s", path)
	}
	checkRaw(t, path, raw, body)
}

func checkRaw(t *testing.T, name string, raw []byte, body string) {
	t.Helper()
	var value any
	if err := json.Unmarshal([]byte(body), &value); err != nil {
		t.Fatalf("%s: %v\n%s", name, err, body)
	}
	if err := validate(parseSchema(t, raw), value); err != nil {
		t.Errorf("%s: %v\n%s", name, err, body)
	}
}

func runMain(t *testing.T, root string, args ...string) (int, []string, []string) {
	t.Helper()
	var outLines, errLines []string
	code := cli.Main(context.Background(), &cli.Options{}, root, args, &cli.Io{
		Out: func(s string) { outLines = append(outLines, s) },
		Err: func(s string) { errLines = append(errLines, s) },
	})
	return code, outLines, errLines
}

func runShared(t *testing.T, root string, args ...string) (int, []string, []string) {
	t.Helper()
	var outLines, errLines []string
	code := cli.RunShared(context.Background(), root, args, &cli.Io{
		Out: func(s string) { outLines = append(outLines, s) },
		Err: func(s string) { errLines = append(errLines, s) },
	}, func(string) { t.Fatal("spawn smp") })
	return code, outLines, errLines
}
