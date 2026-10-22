package cli

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/shared"
)

func runSharedCLI(t *testing.T, root string, args ...string) (int, []string, []string) {
	t.Helper()
	var outLines, errLines []string
	code := RunShared(context.Background(), root, args, &Io{
		Out: func(s string) { outLines = append(outLines, s) },
		Err: func(s string) { errLines = append(errLines, s) },
	}, func(string) { t.Fatal("shared command spawned smp") })
	return code, outLines, errLines
}

func connectionFixture(projectID, root string) map[string]any {
	return map[string]any{
		"instances": []any{map[string]any{
			"id": "postgres@16.4",
			"recipe": map[string]any{
				"env": map[string]any{
					"MINIO_ROOT_PASSWORD": "fake-root-pass-123",
					"MINIO_ROOT_USER":     "hearth",
				},
				"run": map[string]any{"argv": []any{
					"postgres",
					"MINIO_ROOT_PASSWORD=fake-root-pass-123",
					"--password", "fake-root-pass-123",
					"--user", "hearth",
				}},
			},
			"attachments": []any{map[string]any{
				"projectId":   projectID,
				"projectRoot": root,
				"provisioned": true,
				"connection": map[string]any{
					"url": "postgres://app:fake-secret-123@127.0.0.1:5432/db",
					"env": map[string]any{
						"DATABASE_URL":          "postgres://app:fake-secret-123@127.0.0.1:5432/db",
						"PASSWORD":              "fake-secret-123",
						"AWS_SECRET_ACCESS_KEY": "fake-secret-123",
						"MINIO_ROOT_PASSWORD":   "fake-root-pass-123",
						"MINIO_ROOT_USER":       "hearth",
						"S3_BUCKET":             "bucket",
					},
					"provisionArgs": map[string]any{
						"MINIO_ROOT_PASSWORD": "fake-root-pass-123",
						"region":              "local",
					},
				},
			}},
		}},
	}
}

func TestSharedConnectionMasksUnlessReveal(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	root := t.TempDir()
	restore := SetSharedInstanceLoader(func(context.Context) (map[string]any, error) {
		return connectionFixture(shared.ProjectID(root), root), nil
	})
	t.Cleanup(restore)

	code, outLines, errLines := runSharedCLI(t, root, "connection", "postgres", "--json")
	if code != 0 {
		t.Fatalf("masked: %d %v %v", code, outLines, errLines)
	}
	if len(outLines) != 1 || !strings.HasPrefix(outLines[0], `{"service":`) {
		t.Fatalf("stdout = %v", outLines)
	}
	decoded := decodeObject(t, outLines[0])
	if decoded["service"] != "postgres@16.4" || decoded["projectId"] != shared.ProjectID(root) {
		t.Fatalf("document = %#v", decoded)
	}
	conn := decoded["connection"].(map[string]any)
	if conn["url"] != "postgres://app:••••••@127.0.0.1:5432/db" {
		t.Fatalf("url = %#v", conn["url"])
	}
	env := conn["env"].(map[string]any)
	if env["PASSWORD"] != "••••••" || env["S3_BUCKET"] != "bucket" || env["MINIO_ROOT_USER"] != "hearth" {
		t.Fatalf("env = %#v", env)
	}
	if env["AWS_SECRET_ACCESS_KEY"] != "••••••" || env["MINIO_ROOT_PASSWORD"] != "••••••" {
		t.Fatalf("env = %#v", env)
	}
	if env["DATABASE_URL"] != "postgres://app:••••••@127.0.0.1:5432/db" {
		t.Fatalf("DATABASE_URL = %#v", env["DATABASE_URL"])
	}
	provision := conn["provisionArgs"].(map[string]any)
	if provision["MINIO_ROOT_PASSWORD"] != "••••••" || provision["region"] != "local" {
		t.Fatalf("provisionArgs = %#v", provision)
	}
	if strings.Contains(outLines[0], "fake-secret-123") || strings.Contains(outLines[0], "fake-root-pass-123") {
		t.Fatal("json leaked a secret")
	}

	code, outLines, errLines = runSharedCLI(t, root, "connection", "postgres")
	if code != 0 {
		t.Fatalf("text: %d %v", code, errLines)
	}
	text := strings.Join(outLines, "\n")
	if strings.Contains(text, "fake-secret-123") || strings.Contains(text, "fake-root-pass-123") {
		t.Fatalf("default text leaked a secret: %q", text)
	}
	if !strings.Contains(text, "MINIO_ROOT_USER=hearth") || !strings.Contains(text, "postgres://app:••••••@127.0.0.1:5432/db") {
		t.Fatalf("default text = %q", text)
	}

	code, outLines, errLines = runSharedCLI(t, root, "connection", "postgres@16.4", "--reveal")
	if code != 0 {
		t.Fatalf("reveal: %d %v", code, errLines)
	}
	text = strings.Join(outLines, "\n")
	if !strings.Contains(text, "fake-secret-123") || !strings.Contains(text, "PASSWORD=fake-secret-123") || !strings.Contains(text, "MINIO_ROOT_PASSWORD=fake-root-pass-123") {
		t.Fatalf("reveal text = %q", text)
	}
	if outLines[0] != "postgres://app:fake-secret-123@127.0.0.1:5432/db" {
		t.Fatalf("url line = %q", outLines[0])
	}
}

func TestSharedConnectionTypedErrors(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	root := t.TempDir()
	restore := SetSharedInstanceLoader(func(context.Context) (map[string]any, error) {
		return connectionFixture("someone-else", root), nil
	})
	t.Cleanup(restore)

	code, outLines, errLines := runSharedCLI(t, root, "connection", "postgres", "--json")
	if code != ExitUnavailable {
		t.Fatalf("not attached: %d %v %v", code, outLines, errLines)
	}
	if len(errLines) != 0 {
		t.Fatalf("stderr = %v", errLines)
	}
	decoded := decodeObject(t, outLines[0])
	errObj := decoded["error"].(map[string]any)
	if errObj["code"] != "unavailable" || errObj["exit"] != 3.0 {
		t.Fatalf("error = %#v", errObj)
	}
	if errObj["hint"] != "start the service first" {
		t.Fatalf("hint = %#v", errObj["hint"])
	}

	code, _, errLines = runSharedCLI(t, root, "connection", "--help")
	if code != 0 || len(errLines) != 0 {
		t.Fatalf("help: %d %v", code, errLines)
	}

	restoreDown := SetSharedInstanceLoader(func(context.Context) (map[string]any, error) {
		return nil, failErrHint(ExitUnavailable, "smp is not running", "run: hearth shared ensure")
	})
	t.Cleanup(restoreDown)
	code, outLines, errLines = runSharedCLI(t, root, "status")
	if code != ExitUnavailable || len(outLines) != 0 || len(errLines) != 1 || errLines[0] != "smp is not running" {
		t.Fatalf("status down: %d out=%v err=%v", code, outLines, errLines)
	}
}

func TestSharedInstalledMasksSecretsEverywhere(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	root := t.TempDir()
	reg, err := shared.LoadRegistry(fileio.New(false), shared.SharedRoot())
	if err != nil {
		t.Fatal(err)
	}
	url := "http://user:fake-secret-123@127.0.0.1:9000"
	if err := reg.Update(func(data *shared.SharedRegistryData) error {
		data.Instances["minio@1"] = shared.SharedInstance{
			Name: "minio", Version: "1", Port: 43100, InstallState: shared.InstallInstalled,
			Recipe: shared.SharedRecipe{
				Artifacts: map[string]shared.SharedArtifact{},
				Run:       catalog.CommandSpec{Argv: []string{"minio", "MINIO_ROOT_PASSWORD=fake-root-pass-123", "--password", "fake-root-pass-123", "--user", "hearth"}},
				Readiness: shared.RecipeReadiness{PrimaryTcp: true},
				Provision: []catalog.CommandSpec{{
					Argv: []string{"hearth-provision", "AWS_SECRET_ACCESS_KEY=fake-secret-123", "--secret", "fake-secret-123"},
				}},
				Connection: &shared.SharedConnection{
					URL: &url,
					Env: map[string]string{"AWS_SECRET_ACCESS_KEY": "fake-secret-123", "S3_BUCKET": "photos"},
				},
				Env: map[string]string{"MINIO_ROOT_PASSWORD": "fake-root-pass-123", "MINIO_ROOT_USER": "hearth"},
			},
			Attachments: map[string]shared.SharedAttachment{
				"p1": {
					ProjectRoot: root,
					Provisioned: true,
					AttachedAt:  "2026-01-01T00:00:00Z",
					Connection:  []byte(`{"url":"http://user:fake-secret-123@127.0.0.1:9000","env":{"AWS_SECRET_ACCESS_KEY":"fake-secret-123","S3_BUCKET":"photos"},"provisionArgs":{"MINIO_ROOT_PASSWORD":"fake-root-pass-123"}}`),
				},
			},
		}
		return nil
	}); err != nil {
		t.Fatal(err)
	}

	code, outLines, errLines := runSharedCLI(t, root, "installed", "--json", "--reveal")
	if code != 0 {
		t.Fatalf("reveal: %d %v", code, errLines)
	}
	assertInstalledReal(t, strings.Join(outLines, "\n"))

	code, outLines, errLines = runSharedCLI(t, root, "installed", "--json")
	if code != 0 {
		t.Fatalf("installed: %d %v", code, errLines)
	}
	masked := strings.Join(outLines, "\n")
	if strings.Contains(masked, "fake-secret-123") || strings.Contains(masked, "fake-root-pass-123") {
		t.Fatal("default installed json leaked a secret")
	}

	code, outLines, errLines = runSharedCLI(t, root, "installed", "--json", "--mask")
	if code != 0 {
		t.Fatalf("mask: %d %v", code, errLines)
	}
	if strings.Join(outLines, "\n") != masked {
		t.Fatal("--mask changed the default installed json")
	}
	if strings.Contains(masked, "fake-secret-123") || strings.Contains(masked, "fake-root-pass-123") {
		t.Fatal("--mask json leaked a secret")
	}
	decoded := decodeObject(t, masked)
	instance := decoded["instances"].([]any)[0].(map[string]any)
	recipe := instance["recipe"].(map[string]any)
	recipeEnv := recipe["env"].(map[string]any)
	if recipeEnv["MINIO_ROOT_PASSWORD"] != "••••••" || recipeEnv["MINIO_ROOT_USER"] != "hearth" {
		t.Fatalf("recipe.env = %#v", recipeEnv)
	}
	runArgv := recipe["run"].(map[string]any)["argv"].([]any)
	if runArgv[1] != "MINIO_ROOT_PASSWORD=••••••" || runArgv[3] != "••••••" || runArgv[5] != "hearth" {
		t.Fatalf("run argv = %#v", runArgv)
	}
	provisionArgv := recipe["provision"].([]any)[0].(map[string]any)["argv"].([]any)
	if provisionArgv[1] != "AWS_SECRET_ACCESS_KEY=••••••" || provisionArgv[3] != "••••••" {
		t.Fatalf("provision argv = %#v", provisionArgv)
	}
	recipeConn := recipe["connection"].(map[string]any)
	if recipeConn["env"].(map[string]any)["AWS_SECRET_ACCESS_KEY"] != "••••••" || recipeConn["env"].(map[string]any)["S3_BUCKET"] != "photos" {
		t.Fatalf("recipe.connection = %#v", recipeConn)
	}
	attachment := instance["attachments"].(map[string]any)["p1"].(map[string]any)
	attachmentConn := attachment["connection"].(map[string]any)
	if attachmentConn["url"] != "http://user:••••••@127.0.0.1:9000" {
		t.Fatalf("attachment = %#v", attachmentConn)
	}
	if attachmentConn["provisionArgs"].(map[string]any)["MINIO_ROOT_PASSWORD"] != "••••••" {
		t.Fatalf("attachment provisionArgs = %#v", attachmentConn["provisionArgs"])
	}

	code, text, errLines := runSharedCLI(t, root, "installed")
	if code != 0 {
		t.Fatalf("text: %d %v", code, errLines)
	}
	wantText := "minio@1  port 43100  installed  (1 project(s) attached)"
	if len(text) != 1 || text[0] != wantText {
		t.Fatalf("text = %#v", text)
	}
	for _, args := range [][]string{{"installed", "--mask"}, {"installed", "--reveal"}} {
		code, got, errLines := runSharedCLI(t, root, args...)
		if code != 0 || len(got) != 1 || got[0] != wantText {
			t.Fatalf("%v text: %d %#v %v", args, code, got, errLines)
		}
		if strings.Contains(got[0], "fake-secret-123") || strings.Contains(got[0], "fake-root-pass-123") {
			t.Fatalf("%v text leaked a secret: %q", args, got[0])
		}
	}

	code, outLines, errLines = runSharedCLI(t, root, "installed", "--json", "--mask", "--reveal")
	if code != ExitUsage || len(errLines) != 0 || len(outLines) != 1 || !strings.Contains(outLines[0], "cannot be used together") {
		t.Fatalf("mask+reveal: %d out=%v err=%v", code, outLines, errLines)
	}
	if decodeObject(t, outLines[0])["error"].(map[string]any)["code"] != "usage" {
		t.Fatalf("mask+reveal json = %v", outLines)
	}
	code, outLines, errLines = runSharedCLI(t, root, "status", "--reveal", "--mask")
	if code != ExitUsage || len(outLines) != 0 || len(errLines) != 1 || strings.HasPrefix(errLines[0], "hearth:") {
		t.Fatalf("status mask+reveal: %d out=%v err=%v", code, outLines, errLines)
	}

	code, _, errLines = runSharedCLI(t, root, "list", "--reveal")
	if code != ExitUsage || len(errLines) == 0 || strings.HasPrefix(errLines[0], "hearth:") {
		t.Fatalf("reveal on list: %d %v", code, errLines)
	}
	code, _, errLines = runSharedCLI(t, root, "list", "--mask")
	if code != ExitUsage || len(errLines) == 0 || !strings.Contains(errLines[0], "--mask") {
		t.Fatalf("mask on list: %d %v", code, errLines)
	}
	code, _, errLines = runSharedCLI(t, root, "connection", "minio", "--mask")
	if code != ExitUsage || len(errLines) == 0 || !strings.Contains(errLines[0], "--mask") {
		t.Fatalf("mask on connection: %d %v", code, errLines)
	}
}

func assertInstalledReal(t *testing.T, body string) {
	t.Helper()
	decoded := decodeObject(t, body)
	instance := decoded["instances"].([]any)[0].(map[string]any)
	recipe := instance["recipe"].(map[string]any)
	recipeEnv := recipe["env"].(map[string]any)
	if recipeEnv["MINIO_ROOT_PASSWORD"] != "fake-root-pass-123" || recipeEnv["MINIO_ROOT_USER"] != "hearth" {
		t.Fatalf("recipe.env = %#v", recipeEnv)
	}
	recipeConn := recipe["connection"].(map[string]any)
	if recipeConn["env"].(map[string]any)["AWS_SECRET_ACCESS_KEY"] != "fake-secret-123" {
		t.Fatalf("recipe.connection = %#v", recipeConn)
	}
	attachment := instance["attachments"].(map[string]any)["p1"].(map[string]any)
	if attachment["connection"].(map[string]any)["url"] != "http://user:fake-secret-123@127.0.0.1:9000" {
		t.Fatalf("attachment = %#v", attachment["connection"])
	}
	if !strings.Contains(body, "fake-root-pass-123") || !strings.Contains(body, "fake-secret-123") {
		t.Fatal("installed json hid a secret")
	}
}

func TestSharedStatusStaysUnmaskedUnlessMask(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	root := t.TempDir()
	restore := SetSharedInstanceLoader(func(context.Context) (map[string]any, error) {
		body := connectionFixture(shared.ProjectID(root), root)
		instance := body["instances"].([]any)[0].(map[string]any)
		instance["port"] = 5432
		instance["installState"] = "installed"
		instance["state"] = map[string]any{"actualState": "ready"}
		return body, nil
	})
	t.Cleanup(restore)

	code, outLines, errLines := runSharedCLI(t, root, "status", "--json", "--reveal")
	if code != 0 {
		t.Fatalf("reveal: %d %v", code, errLines)
	}
	body := strings.Join(outLines, "\n")
	if !strings.Contains(body, "fake-secret-123") || !strings.Contains(body, "fake-root-pass-123") {
		t.Fatal("--reveal status json hid a secret")
	}
	decoded := decodeObject(t, body)
	instance := decoded["instances"].([]any)[0].(map[string]any)
	recipeEnv := instance["recipe"].(map[string]any)["env"].(map[string]any)
	if recipeEnv["MINIO_ROOT_PASSWORD"] != "fake-root-pass-123" || recipeEnv["MINIO_ROOT_USER"] != "hearth" {
		t.Fatalf("recipe.env = %#v", recipeEnv)
	}
	attachment := instance["attachments"].([]any)[0].(map[string]any)
	conn := attachment["connection"].(map[string]any)
	if conn["url"] != "postgres://app:fake-secret-123@127.0.0.1:5432/db" {
		t.Fatalf("url = %#v", conn["url"])
	}
	env := conn["env"].(map[string]any)
	if env["PASSWORD"] != "fake-secret-123" || env["AWS_SECRET_ACCESS_KEY"] != "fake-secret-123" || env["S3_BUCKET"] != "bucket" {
		t.Fatalf("env = %#v", env)
	}

	code, outLines, errLines = runSharedCLI(t, root, "status", "--json")
	if code != 0 {
		t.Fatalf("status: %d %v", code, errLines)
	}
	masked := strings.Join(outLines, "\n")
	if strings.Contains(masked, "fake-secret-123") || strings.Contains(masked, "fake-root-pass-123") {
		t.Fatal("default status json leaked a secret")
	}

	code, outLines, errLines = runSharedCLI(t, root, "status", "--json", "--mask")
	if code != 0 || strings.Join(outLines, "\n") != masked {
		t.Fatalf("mask changed default status json: %d %v", code, errLines)
	}
	if strings.Contains(masked, "fake-secret-123") || strings.Contains(masked, "fake-root-pass-123") {
		t.Fatal("--mask left a secret")
	}
	decoded = decodeObject(t, masked)
	instance = decoded["instances"].([]any)[0].(map[string]any)
	recipeEnv = instance["recipe"].(map[string]any)["env"].(map[string]any)
	if recipeEnv["MINIO_ROOT_PASSWORD"] != "••••••" || recipeEnv["MINIO_ROOT_USER"] != "hearth" {
		t.Fatalf("masked recipe.env = %#v", recipeEnv)
	}
	runArgv := instance["recipe"].(map[string]any)["run"].(map[string]any)["argv"].([]any)
	if runArgv[1] != "MINIO_ROOT_PASSWORD=••••••" || runArgv[3] != "••••••" || runArgv[5] != "hearth" {
		t.Fatalf("masked argv = %#v", runArgv)
	}
	attachment = instance["attachments"].([]any)[0].(map[string]any)
	conn = attachment["connection"].(map[string]any)
	if conn["url"] != "postgres://app:••••••@127.0.0.1:5432/db" {
		t.Fatalf("masked url = %#v", conn["url"])
	}
	env = conn["env"].(map[string]any)
	if env["PASSWORD"] != "••••••" || env["AWS_SECRET_ACCESS_KEY"] != "••••••" || env["DATABASE_URL"] != "postgres://app:••••••@127.0.0.1:5432/db" || env["S3_BUCKET"] != "bucket" {
		t.Fatalf("masked env = %#v", env)
	}
	if conn["provisionArgs"].(map[string]any)["MINIO_ROOT_PASSWORD"] != "••••••" {
		t.Fatalf("masked provisionArgs = %#v", conn["provisionArgs"])
	}

	wantText := "postgres@16.4  ready  port 5432  installed"
	for _, args := range [][]string{{"status"}, {"status", "--mask"}, {"status", "--reveal"}} {
		code, got, errLines := runSharedCLI(t, root, args...)
		if code != 0 || len(got) != 1 || got[0] != wantText {
			t.Fatalf("%v text: %d %#v %v", args, code, got, errLines)
		}
		if strings.Contains(got[0], "fake-secret-123") || strings.Contains(got[0], "fake-root-pass-123") {
			t.Fatalf("%v text leaked a secret: %q", args, got[0])
		}
	}
}

func TestSkillCheckDriftAndMissing(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)
	root := t.TempDir()
	code, _, errLines := runCLI(t, root, "skill", "check", "--json")
	if code != ExitUnavailable {
		t.Fatalf("missing: %d %v", code, errLines)
	}

	code, _, errLines = runCLI(t, root, "skill", "install")
	if code != 0 {
		t.Fatalf("install: %d %v", code, errLines)
	}
	code, outLines, errLines := runCLI(t, root, "skill", "check", "--json")
	if code != 0 {
		t.Fatalf("sync: %d %v %v", code, outLines, errLines)
	}
	if !strings.Contains(outLines[0], `"inSync":true`) || !strings.Contains(outLines[0], `"missing":[]`) {
		t.Fatalf("sync json = %s", outLines[0])
	}

	dest := filepath.Join(home, ".agents", "skills", "hearth")
	if err := os.WriteFile(filepath.Join(dest, ".DS_Store"), []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	code, outLines, errLines = runCLI(t, root, "skill", "check", "--json")
	if code != ExitConflict {
		t.Fatalf("drift: %d %v %v", code, outLines, errLines)
	}
	decoded := decodeObject(t, outLines[0])
	if decoded["inSync"] != false {
		t.Fatalf("drift json = %#v", decoded)
	}
	extra := decoded["extra"].([]any)
	if len(extra) != 1 || extra[0] != ".DS_Store" {
		t.Fatalf("extra = %#v", extra)
	}

	code, outLines, _ = runCLI(t, root, "version", "--help")
	if code != 0 || len(outLines) == 0 || !strings.Contains(outLines[0], "hearth version") {
		t.Fatalf("version help: %d %v", code, outLines)
	}
	code, outLines, _ = runCLI(t, root, "schema", "--help")
	if code != 0 || !strings.Contains(strings.Join(outLines, "\n"), "hearth schema") {
		t.Fatalf("schema help: %d %v", code, outLines)
	}
	code, outLines, _ = runCLI(t, root, "skill", "check", "--help")
	if code != 0 || !strings.Contains(strings.Join(outLines, "\n"), "skill check") {
		t.Fatalf("skill check help: %d %v", code, outLines)
	}
	code, outLines, errLines = runCLI(t, root, "version", "nope", "--json")
	if code != ExitUsage {
		t.Fatalf("version extra: %d %v %v", code, outLines, errLines)
	}
	if len(errLines) != 0 || decodeObject(t, outLines[0])["error"].(map[string]any)["code"] != "usage" {
		t.Fatalf("version error out=%v err=%v", outLines, errLines)
	}
}

func TestSharedConnectionScript(t *testing.T) {
	root := t.TempDir()
	projectID := shared.ProjectID(root)
	newBin := filepath.Join(t.TempDir(), "hearth")
	writeExe(t, newBin, "#!/bin/bash\nif [[ \"$*\" == *\"--help\"* ]]; then exit 0; fi\necho \"NEW $*\"\n")
	t.Setenv("HEARTH_BIN", newBin)
	t.Setenv("HEARTH_ROOT", root)
	out, errLines, code := runScript(t, "postgres", "--json")
	if code != 0 {
		t.Fatalf("new binary: %d %s %s", code, out, errLines)
	}
	if !strings.Contains(out, "shared connection postgres --json") || strings.Contains(out, "--reveal") {
		t.Fatalf("stdout = %q", out)
	}
	out, _, code = runScript(t, "postgres", "--reveal")
	if code != 0 || !strings.Contains(out, "--reveal") {
		t.Fatalf("reveal passthrough: %d %q", code, out)
	}

	oldBin := filepath.Join(t.TempDir(), "hearth-old")
	status := `{"instances":[{"id":"postgres@16.4","recipe":{"env":{"MINIO_ROOT_PASSWORD":"fake-root-pass-123","MINIO_ROOT_USER":"hearth"}},"attachments":[{"projectId":"` + projectID + `","provisioned":true,"connection":{"url":"postgres://app:fake-secret-123@127.0.0.1:5432/db","env":{"PASSWORD":"fake-secret-123","AWS_SECRET_ACCESS_KEY":"fake-secret-123","MINIO_ROOT_USER":"hearth","S3_BUCKET":"bucket"},"provisionArgs":{"MINIO_ROOT_PASSWORD":"fake-root-pass-123"},"argv":["MINIO_ROOT_PASSWORD=fake-root-pass-123","--password","fake-root-pass-123","--user","hearth"]}}]}]}`
	writeExe(t, oldBin, "#!/bin/bash\nif [[ \"$*\" == *\"connection --help\"* ]]; then exit 2; fi\nif [[ \"$*\" == *\"status --json\"* ]]; then printf '%s\\n' '"+status+"'; exit 0; fi\necho \"unexpected $*\" >&2; exit 9\n")
	t.Setenv("HEARTH_BIN", oldBin)
	out, errLines, code = runScript(t, "postgres", "--json")
	if code != 0 {
		t.Fatalf("fallback: %d %s %s", code, out, errLines)
	}
	decoded := decodeObject(t, strings.TrimSpace(out))
	conn := decoded["connection"].(map[string]any)
	env := conn["env"].(map[string]any)
	if env["PASSWORD"] != "••••••" || env["AWS_SECRET_ACCESS_KEY"] != "••••••" || env["MINIO_ROOT_USER"] != "hearth" || env["S3_BUCKET"] != "bucket" {
		t.Fatalf("fallback env = %#v", env)
	}
	if conn["url"] != "postgres://app:••••••@127.0.0.1:5432/db" {
		t.Fatalf("fallback url = %#v", conn["url"])
	}
	if conn["provisionArgs"].(map[string]any)["MINIO_ROOT_PASSWORD"] != "••••••" {
		t.Fatalf("fallback provisionArgs = %#v", conn["provisionArgs"])
	}
	argv := conn["argv"].([]any)
	if argv[0] != "MINIO_ROOT_PASSWORD=••••••" || argv[2] != "••••••" || argv[4] != "hearth" {
		t.Fatalf("fallback argv = %#v", argv)
	}
	if strings.Contains(out, "fake-secret-123") || strings.Contains(out, "fake-root-pass-123") {
		t.Fatal("fallback leaked a secret")
	}
	out, _, code = runScript(t, "postgres")
	if code != 0 || strings.Contains(out, "fake-secret-123") || strings.Contains(out, "fake-root-pass-123") {
		t.Fatalf("fallback text: %d %q", code, out)
	}
	if !strings.Contains(out, "postgres://app:••••••@127.0.0.1:5432/db") || !strings.Contains(out, "MINIO_ROOT_USER=hearth") {
		t.Fatalf("fallback text = %q", out)
	}
	out, _, code = runScript(t, "postgres", "--json", "--reveal")
	if code != 0 || !strings.Contains(out, "fake-secret-123") || !strings.Contains(out, "fake-root-pass-123") {
		t.Fatalf("fallback reveal: %d %q", code, out)
	}
	_, errLines, code = runScript(t, "missing", "--json")
	if code != 3 {
		t.Fatalf("missing instance: %d %s", code, errLines)
	}
}

func TestSharedStatusScriptMasksByDefault(t *testing.T) {
	root := t.TempDir()
	bin := filepath.Join(t.TempDir(), "hearth")
	writeExe(t, bin, "#!/bin/bash\necho \"$*\"\n")
	t.Setenv("HEARTH_BIN", bin)
	t.Setenv("HEARTH_ROOT", root)

	out, errLines, code := runStatusScript(t, "--json")
	if code != 0 {
		t.Fatalf("default: %d %s %s", code, out, errLines)
	}
	if !strings.Contains(out, "shared status --json") || strings.Contains(out, "--mask") || strings.Contains(out, "--reveal") {
		t.Fatalf("stdout = %q", out)
	}

	out, _, code = runStatusScript(t, "--json", "--reveal")
	if code != 0 || !strings.Contains(out, "shared status --json --reveal") || strings.Contains(out, "--mask") {
		t.Fatalf("reveal: %d %q", code, out)
	}

	out, _, code = runStatusScript(t)
	if code != 0 || !strings.Contains(out, "shared status") || strings.Contains(out, "--mask") || strings.Contains(out, "--reveal") {
		t.Fatalf("text: %d %q", code, out)
	}

	_, errLines, code = runStatusScript(t, "--mask", "--reveal")
	if code != 2 || !strings.Contains(errLines, "cannot be used together") {
		t.Fatalf("both: %d %s", code, errLines)
	}
}

func TestStopDaemonScriptPassesYes(t *testing.T) {
	root := t.TempDir()
	bin := filepath.Join(t.TempDir(), "hearth")
	writeExe(t, bin, "#!/bin/bash\necho \"$*\"\n")
	t.Setenv("HEARTH_BIN", bin)
	t.Setenv("HEARTH_ROOT", root)

	out, errLines, code := runSkillScript(t, "stop-daemon.sh")
	if code != 0 || !strings.Contains(out, "manager stop --yes") || strings.Count(out, "--yes") != 1 {
		t.Fatalf("default: %d %q %s", code, out, errLines)
	}
	out, errLines, code = runSkillScript(t, "stop-daemon.sh", "--yes", "--json")
	if code != 0 || strings.Count(out, "--yes") != 1 || !strings.Contains(out, "--json") {
		t.Fatalf("duplicate: %d %q %s", code, out, errLines)
	}
}

func runSkillScript(t *testing.T, name string, args ...string) (string, string, int) {
	t.Helper()
	cmd := exec.Command("bash", append([]string{"skill/scripts/" + name}, args...)...)
	var stdout, stderr strings.Builder
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	err := cmd.Run()
	code := 0
	if err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) {
			code = exitErr.ExitCode()
		} else {
			t.Fatal(err)
		}
	}
	return stdout.String(), stderr.String(), code
}

func writeExe(t *testing.T, path, body string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(body), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o755); err != nil {
		t.Fatal(err)
	}
}

func runScript(t *testing.T, args ...string) (string, string, int) {
	t.Helper()
	cmd := exec.Command("bash", append([]string{"skill/scripts/shared-connection.sh"}, args...)...)
	var stdout, stderr strings.Builder
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	err := cmd.Run()
	code := 0
	if err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) {
			code = exitErr.ExitCode()
		} else {
			t.Fatal(err)
		}
	}
	return stdout.String(), stderr.String(), code
}

func runStatusScript(t *testing.T, args ...string) (string, string, int) {
	t.Helper()
	cmd := exec.Command("bash", append([]string{"skill/scripts/shared-status.sh"}, args...)...)
	var stdout, stderr strings.Builder
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	err := cmd.Run()
	code := 0
	if err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) {
			code = exitErr.ExitCode()
		} else {
			t.Fatal(err)
		}
	}
	return stdout.String(), stderr.String(), code
}

func decodeObject(t *testing.T, text string) map[string]any {
	t.Helper()
	var decoded map[string]any
	if err := json.Unmarshal([]byte(text), &decoded); err != nil {
		t.Fatalf("json: %v\n%s", err, text)
	}
	return decoded
}
