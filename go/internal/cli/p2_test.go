package cli

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/schema"
	"github.com/ngosangns/hearth/go/internal/shared"
)

func TestRequireYes(t *testing.T) {
	err := requireYes(&Io{}, false, "stopping the daemon needs confirmation")
	if exitCodeOf(err) != ExitNeedsConfirm {
		t.Fatalf("exit = %d", exitCodeOf(err))
	}
	var local *LocalctlError
	if !errors.As(err, &local) || local.Hint != "pass --yes" || local.Message != "stopping the daemon needs confirmation" {
		t.Fatalf("error = %#v", err)
	}
	if requireYes(&Io{Confirm: func(string) bool { return false }}, false, "x") != nil {
		t.Fatal("a TTY should allow the action")
	}
	if requireYes(&Io{}, true, "x") != nil {
		t.Fatal("--yes should allow the action")
	}
}

func TestDryRunPrintsTheResolvedList(t *testing.T) {
	cat := testCatalog([]catalog.ServiceDefinition{
		verifiedService("worker"),
		verifiedService("api"),
		disabledService("gone"),
	})
	cat.Groups = map[string][]string{
		"web":   {"worker", "gone", "api"},
		"empty": {"gone"},
	}
	root := t.TempDir()

	code, outLines, errLines := runCatalog(t, root, cat, "restart", "web", "--dry-run")
	if code != 0 || len(errLines) != 0 || len(outLines) != 1 {
		t.Fatalf("group: %d out=%v err=%v", code, outLines, errLines)
	}
	if outLines[0] != `{"dryRun":true,"action":"restart","target":"web","services":["worker","api"]}` {
		t.Fatalf("json = %s", outLines[0])
	}

	code, outLines, errLines = runCatalog(t, root, cat, "start", "api", "--dry-run", "--kill-unowned")
	if code != 0 || len(outLines) != 1 || len(errLines) != 0 {
		t.Fatalf("kill dry-run: %d out=%v err=%v", code, outLines, errLines)
	}
	if !strings.Contains(outLines[0], `"action":"start"`) || !strings.Contains(outLines[0], `"services":["api"]`) {
		t.Fatalf("json = %s", outLines[0])
	}

	code, outLines, errLines = runCatalog(t, root, cat, "stop", "web", "--json", "--dry-run")
	if code != 0 || outLines[0] != `{"dryRun":true,"action":"stop","target":"web","services":["worker","api"]}` {
		t.Fatalf("stop: %d %v %v", code, outLines, errLines)
	}

	code, outLines, errLines = runCatalog(t, root, cat, "restart", "nope", "--dry-run", "--json")
	assertJSONError(t, "unknown", code, ExitUsage, "usage", outLines, errLines)

	code, _, errLines = runCatalog(t, root, cat, "start", "gone", "--dry-run")
	if code != ExitUsage || len(errLines) == 0 || !strings.Contains(errLines[0], "disabled") {
		t.Fatalf("disabled: %d %v", code, errLines)
	}

	code, _, errLines = runCatalog(t, root, cat, "restart", "empty", "--dry-run")
	if code != ExitUsage || len(errLines) == 0 || !strings.Contains(errLines[0], "nothing to run") {
		t.Fatalf("empty: %d %v", code, errLines)
	}

	code, _, errLines = runCatalog(t, root, cat, "restart", "--dry-run", "--kill-unowned", "web")
	if code != ExitUsage || len(errLines) == 0 || !strings.Contains(errLines[0], "--kill-unowned") {
		t.Fatalf("kill on restart: %d %v", code, errLines)
	}
}

func TestNeedsConfirmBeforeAnyDaemon(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	root := t.TempDir()
	cat := testCatalog([]catalog.ServiceDefinition{verifiedService("api")})

	code, outLines, errLines := runCatalog(t, root, cat, "start", "api", "--kill-unowned", "--json")
	assertJSONError(t, "kill", code, ExitNeedsConfirm, "needs-confirm", outLines, errLines)
	if !strings.Contains(outLines[0], `"hint":"pass --yes"`) {
		t.Fatalf("hint = %s", outLines[0])
	}

	code, outLines, errLines = runCatalog(t, root, cat, "manager", "stop", "--json")
	assertJSONError(t, "manager stop", code, ExitNeedsConfirm, "needs-confirm", outLines, errLines)

	code, outLines, errLines = runCatalog(t, root, cat, "manager", "stop")
	if code != ExitNeedsConfirm || len(outLines) != 0 || len(errLines) != 1 || strings.HasPrefix(errLines[0], "hearth:") {
		t.Fatalf("text stop: %d out=%v err=%v", code, outLines, errLines)
	}
	if !strings.Contains(errLines[0], "stopping the daemon") {
		t.Fatalf("stderr = %v", errLines)
	}

	code, outLines, errLines = runCatalog(t, root, cat, "manager", "stop", "--json", "--yes")
	assertJSONError(t, "manager stop yes", code, ExitUnavailable, "unavailable", outLines, errLines)

	code, outLines, errLines = runSharedCLI(t, root, "remove", "postgres@16.4", "--json")
	assertJSONError(t, "remove", code, ExitNeedsConfirm, "needs-confirm", outLines, errLines)

	code, outLines, errLines = runSharedCLI(t, root, "remove", "postgres@16.4", "--json", "--yes")
	assertJSONError(t, "remove yes", code, ExitUnavailable, "unavailable", outLines, errLines)

	code, outLines, errLines = runSharedCLI(t, root, "detach", "postgres@16.4", "--stop-if-unused", "--json")
	assertJSONError(t, "detach", code, ExitNeedsConfirm, "needs-confirm", outLines, errLines)
	if !strings.Contains(outLines[0], "stopping a shared instance") {
		t.Fatalf("detach message = %s", outLines[0])
	}

	code, outLines, errLines = runSharedCLI(t, root, "detach", "postgres@16.4", "--stop-if-unused", "--yes")
	if code != 0 || len(outLines) != 0 || len(errLines) != 0 {
		t.Fatalf("detach yes with no smp: %d out=%v err=%v", code, outLines, errLines)
	}

	code, outLines, errLines = runCatalog(t, root, cat, "logs", "--json")
	assertJSONError(t, "logs", code, ExitUsage, "usage", outLines, errLines)

	code, outLines, errLines = runCatalog(t, root, cat, "nope", "--json")
	assertJSONError(t, "unknown", code, ExitUsage, "usage", outLines, errLines)

	code, outLines, errLines = runCatalog(t, root, cat, "status", "--json")
	assertJSONError(t, "status", code, ExitUnavailable, "unavailable", outLines, errLines)

	code, outLines, errLines = runCatalog(t, root, cat, "version", "nope", "--json")
	assertJSONError(t, "version", code, ExitUsage, "usage", outLines, errLines)
}

func TestSharedInstancesSortByNameThenVersion(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	root := t.TempDir()

	reg, err := shared.LoadRegistry(fileio.New(false), shared.SharedRoot())
	if err != nil {
		t.Fatal(err)
	}
	if err := reg.Update(func(data *shared.SharedRegistryData) error {
		for _, instance := range []shared.SharedInstance{
			instanceRow("zed", "1", 3),
			instanceRow("alpha", "2", 2),
			instanceRow("alpha", "1", 1),
			instanceRow("minio", "RELEASE.9", 5),
			instanceRow("minio", "RELEASE.10", 4),
		} {
			data.Instances[instance.ID()] = instance
		}
		return nil
	}); err != nil {
		t.Fatal(err)
	}

	code, outLines, errLines := runSharedCLI(t, root, "installed", "--json")
	if code != 0 {
		t.Fatalf("installed: %d %v", code, errLines)
	}
	want := []string{"alpha", "1", "alpha", "2", "minio", "RELEASE.10", "minio", "RELEASE.9", "zed", "1"}
	if got := instancePairs(t, strings.Join(outLines, "\n")); strings.Join(got, ",") != strings.Join(want, ",") {
		t.Fatalf("installed order = %v", got)
	}
	code, text, errLines := runSharedCLI(t, root, "installed")
	if code != 0 {
		t.Fatalf("installed text: %d %v", code, errLines)
	}
	wantText := []string{
		"alpha@1  port 1  installed  (0 project(s) attached)",
		"alpha@2  port 2  installed  (0 project(s) attached)",
		"minio@RELEASE.10  port 4  installed  (0 project(s) attached)",
		"minio@RELEASE.9  port 5  installed  (0 project(s) attached)",
		"zed@1  port 3  installed  (0 project(s) attached)",
	}
	if strings.Join(text, "\n") != strings.Join(wantText, "\n") {
		t.Fatalf("text =\n%s", strings.Join(text, "\n"))
	}

	restore := SetSharedInstanceLoader(func(context.Context) (map[string]any, error) {
		return map[string]any{"instances": []any{
			statusRow("zed@1", "zed", "1", 3),
			statusRow("alpha@2", "alpha", "2", 2),
			statusRow("alpha@1", "", "", 1),
		}}, nil
	})
	t.Cleanup(restore)
	code, outLines, errLines = runSharedCLI(t, root, "status", "--json")
	if code != 0 {
		t.Fatalf("status: %d %v", code, errLines)
	}
	ids := instanceIDs(t, strings.Join(outLines, "\n"))
	if strings.Join(ids, ",") != "alpha@1,alpha@2,zed@1" {
		t.Fatalf("status order = %v", ids)
	}
	code, text, errLines = runSharedCLI(t, root, "status")
	if code != 0 || len(text) != 3 || !strings.HasPrefix(text[0], "alpha@1") || !strings.HasPrefix(text[1], "alpha@2") || !strings.HasPrefix(text[2], "zed@1") {
		t.Fatalf("status text: %d %v %v", code, text, errLines)
	}
}

func TestWorkspacesList(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("HEARTH_WORKSPACE_FILE", "")
	root := t.TempDir()

	code, outLines, errLines := runCLI(t, root, "workspaces", "list", "--json")
	if code != 0 || len(errLines) != 0 || len(outLines) != 1 || outLines[0] != `{"workspaces":[]}` {
		t.Fatalf("missing: %d out=%v err=%v", code, outLines, errLines)
	}
	code, outLines, errLines = runCLI(t, root, "workspaces", "list")
	if code != 0 || len(outLines) != 0 || len(errLines) != 0 {
		t.Fatalf("missing text: %d out=%v err=%v", code, outLines, errLines)
	}

	path := filepath.Join(home, "Library", "Application Support", "HearthApp", "workspaces.json")
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(" \n"), 0o644); err != nil {
		t.Fatal(err)
	}
	code, outLines, errLines = runCLI(t, root, "workspaces", "list", "--json")
	if code != 0 || outLines[0] != `{"workspaces":[]}` {
		t.Fatalf("blank: %d %v %v", code, outLines, errLines)
	}

	body := `[{"id":"w1","path":"/tmp/app","trusted":true,"addedAt":"2026-01-01T00:00:00Z"},{"id":"w2","path":"/tmp/other"}]`
	if err := os.WriteFile(path, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
	code, outLines, errLines = runCLI(t, root, "workspaces", "list", "--json")
	if code != 0 || len(errLines) != 0 {
		t.Fatalf("home file: %d %v", code, errLines)
	}
	if !strings.Contains(outLines[0], `"id":"w1"`) || !strings.Contains(outLines[0], `"trusted":false`) {
		t.Fatalf("json = %s", outLines[0])
	}
	code, text, errLines := runCLI(t, root, "workspaces", "list")
	if code != 0 || len(text) != 2 || text[0] != "w1  /tmp/app  trusted  2026-01-01T00:00:00Z" || text[1] != "w2  /tmp/other  untrusted  " {
		t.Fatalf("text: %d %v %v", code, text, errLines)
	}

	override := filepath.Join(t.TempDir(), "workspaces.json")
	t.Setenv("HEARTH_WORKSPACE_FILE", override)
	if err := os.WriteFile(override, []byte(`[{"id":"only","path":"/override","trusted":false,"addedAt":""}]`), 0o644); err != nil {
		t.Fatal(err)
	}
	code, outLines, errLines = runCLI(t, root, "workspaces", "list", "--json")
	if code != 0 || !strings.Contains(outLines[0], `"path":"/override"`) {
		t.Fatalf("override: %d %v %v", code, outLines, errLines)
	}

	for _, bad := range []string{`{`, `{"id":"x"}`, `[{"id":"x"}]`, `[{"path":"/p"}]`, `[1]`} {
		if err := os.WriteFile(override, []byte(bad), 0o644); err != nil {
			t.Fatal(err)
		}
		code, outLines, errLines = runCLI(t, root, "workspaces", "list", "--json")
		assertJSONError(t, bad, code, ExitProtocol, "protocol", outLines, errLines)
		code, outLines, errLines = runCLI(t, root, "workspaces", "list")
		if code != ExitProtocol || len(outLines) != 0 || len(errLines) != 1 || !strings.HasPrefix(errLines[0], "hearth:") {
			t.Fatalf("text %s: %d out=%v err=%v", bad, code, outLines, errLines)
		}
	}
}

func TestHelpExitsZero(t *testing.T) {
	root := t.TempDir()
	for _, path := range schema.Paths() {
		args := append(strings.Fields(path), "--help")
		code, outLines, errLines := runCLI(t, root, args...)
		if code != 0 || len(errLines) != 0 || len(outLines) == 0 {
			t.Fatalf("%v help: %d out=%v err=%v", args, code, outLines, errLines)
		}
		code, outLines, errLines = runCLI(t, root, append(strings.Fields(path), "-h")...)
		if code != 0 || len(outLines) == 0 {
			t.Fatalf("%s -h: %d", path, code)
		}
	}
	if _, ok := HelpFor([]string{"status", "help"}); ok {
		t.Fatal("status help is a target")
	}
	if _, ok := HelpFor([]string{"shared", "attach", "nginx@1", "--", "--help"}); ok {
		t.Fatal("--help after -- is passthrough")
	}
	if text, ok := HelpFor([]string{"help"}); !ok || !strings.Contains(text, "workspaces list") {
		t.Fatal("bare help")
	}
}

func runCatalog(t *testing.T, root string, cat catalog.ServiceCatalog, args ...string) (int, []string, []string) {
	t.Helper()
	options := &Options{
		Catalog: cat,
		SpawnDaemon: func(string) {
			panic("spawn_daemon should not be called")
		},
	}
	var outLines, errLines []string
	code := Main(context.Background(), options, root, args, &Io{
		Out: func(s string) { outLines = append(outLines, s) },
		Err: func(s string) { errLines = append(errLines, s) },
	})
	return code, outLines, errLines
}

func assertJSONError(t *testing.T, name string, code, wantExit int, wantCode string, outLines, errLines []string) {
	t.Helper()
	if code != wantExit || len(errLines) != 0 || len(outLines) != 1 {
		t.Fatalf("%s: %d out=%v err=%v", name, code, outLines, errLines)
	}
	decoded := decodeObject(t, outLines[0])
	errObj, _ := decoded["error"].(map[string]any)
	if errObj["code"] != wantCode || errObj["exit"] != float64(wantExit) {
		t.Fatalf("%s error = %#v", name, errObj)
	}
	if _, ok := errObj["message"].(string); !ok || errObj["message"] == "" {
		t.Fatalf("%s message = %#v", name, errObj["message"])
	}
}

func instanceRow(name, version string, port uint16) shared.SharedInstance {
	return shared.SharedInstance{
		Name: name, Version: version, Port: port, InstallState: shared.InstallInstalled,
		Recipe: shared.SharedRecipe{
			Artifacts: map[string]shared.SharedArtifact{},
			Run:       catalog.CommandSpec{Argv: []string{name}},
			Readiness: shared.RecipeReadiness{PrimaryTcp: true},
		},
		Attachments: map[string]shared.SharedAttachment{},
	}
}

func statusRow(id, name, version string, port int) map[string]any {
	row := map[string]any{
		"id":           id,
		"port":         port,
		"installState": "installed",
		"state":        map[string]any{"actualState": "ready"},
	}
	if name != "" || version != "" {
		row["name"] = name
		row["version"] = version
	}
	return row
}

func instancePairs(t *testing.T, body string) []string {
	t.Helper()
	var doc struct {
		Instances []struct {
			Name    string `json:"name"`
			Version string `json:"version"`
		} `json:"instances"`
	}
	if err := json.Unmarshal([]byte(body), &doc); err != nil {
		t.Fatal(err)
	}
	out := make([]string, 0, len(doc.Instances)*2)
	for _, instance := range doc.Instances {
		out = append(out, instance.Name, instance.Version)
	}
	return out
}

func instanceIDs(t *testing.T, body string) []string {
	t.Helper()
	decoded := decodeObject(t, body)
	raw, _ := decoded["instances"].([]any)
	ids := make([]string, 0, len(raw))
	for _, entry := range raw {
		instance, _ := entry.(map[string]any)
		id, _ := instance["id"].(string)
		ids = append(ids, id)
	}
	return ids
}
