package cli

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

// testCatalog builds a catalog whose runtime directory is relative to the
// project root, so the lock search lands under the root the test passes.
func testCatalog(services []catalog.ServiceDefinition) catalog.ServiceCatalog {
	runtimeDirectory := ".hearth/runtime-v1"
	return catalog.ServiceCatalog{
		Services:           services,
		Groups:             map[string][]string{},
		RuntimeDirectory:   &runtimeDirectory,
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
		PrivateFileGuard:   new(false),
	}
}

func verifiedService(id string) catalog.ServiceDefinition {
	return catalog.ServiceDefinition{
		ID: id,
		Profiles: catalog.ServiceProfiles{
			Run: catalog.ServiceRunProfile{CommandStatus: "verified"},
		},
	}
}

func disabledService(id string) catalog.ServiceDefinition {
	service := verifiedService(id)
	service.Disabled = true
	return service
}

func unresolvedService(id string) catalog.ServiceDefinition {
	return catalog.ServiceDefinition{
		ID: id,
		Profiles: catalog.ServiceProfiles{
			Run: catalog.ServiceRunProfile{CommandStatus: "unresolved"},
		},
	}
}

// runCLI drives Main the way the binary does, capturing out/err lines.
func runCLI(t *testing.T, root string, extraArgs ...string) (int, []string, []string) {
	t.Helper()
	options := &Options{
		Catalog:     testCatalog(nil),
		SpawnDaemon: func(string) { panic("spawn_daemon should not be called") },
	}
	var outLines, errLines []string
	io := &Io{
		Out: func(s string) { outLines = append(outLines, s) },
		Err: func(s string) { errLines = append(errLines, s) },
	}
	code := Main(context.Background(), options, root, extraArgs, io)
	return code, outLines, errLines
}

// ---------------------------------------------------------------------------------------------
// parse_command_flags
// ---------------------------------------------------------------------------------------------

func TestParsesPositionalsAndKnownFlags(t *testing.T) {
	flags, err := ParseCommandFlags([]string{"start", "api", "--wait", "--json"}, []Flag{FlagWait, FlagJSON})
	if err != nil {
		t.Fatal(err)
	}
	if len(flags.Positionals) != 2 || flags.Positionals[0] != "start" || flags.Positionals[1] != "api" {
		t.Fatalf("unexpected positionals: %v", flags.Positionals)
	}
	if !flags.Wait || !flags.JSON || flags.Follow {
		t.Fatalf("unexpected flags: %+v", flags)
	}
}

func TestRejectsAnUnsupportedFlag(t *testing.T) {
	_, err := ParseCommandFlags([]string{"--wait"}, []Flag{FlagJSON})
	if err == nil {
		t.Fatal("expected an error")
	}
	if exitCodeOf(err) != ExitUsage {
		t.Fatalf("expected exit %d, got %d", ExitUsage, exitCodeOf(err))
	}
	if !strings.Contains(err.Error(), "unsupported flag") {
		t.Fatalf("unexpected message: %v", err)
	}
}

func TestRejectsADuplicateFlag(t *testing.T) {
	_, err := ParseCommandFlags([]string{"--json", "--json"}, []Flag{FlagJSON})
	if err == nil || !strings.Contains(err.Error(), "duplicate flag") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestRejectsAnUnknownFlag(t *testing.T) {
	_, err := ParseCommandFlags([]string{"--bogus"}, []Flag{FlagJSON})
	if err == nil || !strings.Contains(err.Error(), "unknown flag") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestTailRequiresAPositiveInteger(t *testing.T) {
	if _, err := ParseCommandFlags([]string{"--tail", "0"}, []Flag{FlagTail}); err == nil ||
		!strings.Contains(err.Error(), "--tail must be a positive integer") {
		t.Fatalf("unexpected error for 0: %v", err)
	}
	if _, err := ParseCommandFlags([]string{"--tail", "abc"}, []Flag{FlagTail}); err == nil ||
		!strings.Contains(err.Error(), "--tail must be a positive integer") {
		t.Fatalf("unexpected error for abc: %v", err)
	}
	flags, err := ParseCommandFlags([]string{"--tail", "50"}, []Flag{FlagTail})
	if err != nil || flags.Tail == nil || *flags.Tail != 50 {
		t.Fatalf("unexpected tail: %+v (%v)", flags, err)
	}
}

func TestParseRootSplitsALeadingRootFlagAndRejectsAMissingPath(t *testing.T) {
	root, rest, err := ParseRoot([]string{"--root", "/tmp/project", "status"})
	if err != nil || root != "/tmp/project" || len(rest) != 1 || rest[0] != "status" {
		t.Fatalf("unexpected parse: %q %v %v", root, rest, err)
	}
	cwd, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	root, rest, err = ParseRoot([]string{"status"})
	if err != nil || root != cwd || len(rest) != 1 || rest[0] != "status" {
		t.Fatalf("unexpected parse: %q %v %v", root, rest, err)
	}
	if _, _, err := ParseRoot([]string{"--root"}); err == nil || exitCodeOf(err) != ExitUsage {
		t.Fatalf("expected a usage error, got %v", err)
	}
}

// ---------------------------------------------------------------------------------------------
// runnable_targets
// ---------------------------------------------------------------------------------------------

func TestRunnableTargetsRejectsADisabledDirectTarget(t *testing.T) {
	cat := testCatalog([]catalog.ServiceDefinition{disabledService("api")})
	_, err := RunnableTargets(&cat, new("api"))
	if err == nil || !strings.Contains(err.Error(), "service api is disabled") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestRunnableTargetsSkipsDisabledGroupMembers(t *testing.T) {
	cat := testCatalog([]catalog.ServiceDefinition{verifiedService("api"), disabledService("worker")})
	cat.Groups["all"] = []string{"api", "worker"}
	selected, err := RunnableTargets(&cat, new("all"))
	if err != nil {
		t.Fatal(err)
	}
	if len(selected) != 1 || selected[0] != "api" {
		t.Fatalf("unexpected selection: %v", selected)
	}
}

func TestRunnableTargetsRejectsAnAllDisabledSelection(t *testing.T) {
	cat := testCatalog([]catalog.ServiceDefinition{disabledService("api"), disabledService("worker")})
	_, err := RunnableTargets(&cat, nil)
	if err == nil || !strings.Contains(err.Error(), "nothing to run: every selected service is disabled") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestRunnableTargetsRejectsAnUnresolvedProfile(t *testing.T) {
	cat := testCatalog([]catalog.ServiceDefinition{unresolvedService("wip")})
	_, err := RunnableTargets(&cat, new("wip"))
	if err == nil || exitCodeOf(err) != ExitFailed || !strings.Contains(err.Error(), "unsupported service: wip") {
		t.Fatalf("unexpected error: %v", err)
	}
}

func TestRunnableTargetsSelectsEveryServiceWithoutATarget(t *testing.T) {
	cat := testCatalog([]catalog.ServiceDefinition{verifiedService("api"), verifiedService("web")})
	selected, err := RunnableTargets(&cat, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(selected) != 2 || selected[0] != "api" || selected[1] != "web" {
		t.Fatalf("unexpected selection: %v", selected)
	}
}

// ---------------------------------------------------------------------------------------------
// status / logs / dispatch
// ---------------------------------------------------------------------------------------------

func TestTextStateNeverHidesAFailureAsStopped(t *testing.T) {
	lifecycle := func(actual state.ActualServiceState) *state.ServiceLifecycleState {
		return &state.ServiceLifecycleState{
			ServiceID:    "svc",
			DesiredState: state.DesiredRunning,
			ActualState:  actual,
			Readiness:    state.ReadinessUnknown,
			Generation:   1,
			CreatedAt:    "t",
			UpdatedAt:    "t",
		}
	}
	expected := []struct {
		actual  state.ActualServiceState
		printed string
	}{
		{state.ActualStopped, "stopped"},
		{state.ActualQueuedStart, "queued-start"},
		{state.ActualPreparing, "running"},
		{state.ActualStarting, "running"},
		{state.ActualRunning, "running"},
		{state.ActualRunningUnready, "running"},
		{state.ActualReady, "ready"},
		{state.ActualSucceeded, "succeeded"},
		{state.ActualStopping, "stopping"},
		{state.ActualFailed, "failed"},
		{state.ActualOrphaned, "orphaned"},
		{state.ActualExternallyOwned, "externally-owned"},
	}
	if len(expected) != len(state.AllActualStates) {
		t.Fatalf("a new state needs an explicit CLI rendering: %d vs %d", len(expected), len(state.AllActualStates))
	}
	for _, tc := range expected {
		if got := textState(lifecycle(tc.actual)); got != tc.printed {
			t.Fatalf("%s: got %q, want %q", tc.actual, got, tc.printed)
		}
	}
	if got := textState(nil); got != "stopped" {
		t.Fatalf("nil: got %q", got)
	}
}

func TestStatusShowsTheRealStateAndAPidOnlyForALiveProcess(t *testing.T) {
	lifecycle := func(actual state.ActualServiceState) *state.ServiceLifecycleState {
		return &state.ServiceLifecycleState{
			ServiceID:    "svc",
			DesiredState: state.DesiredRunning,
			ActualState:  actual,
			Readiness:    state.ReadinessUnknown,
			Generation:   1,
			Identity: &state.ProcessIdentity{
				ManagerInstanceID:  "i",
				ServiceID:          "svc",
				Generation:         1,
				Pid:                new(int64(4242)),
				Pgid:               new(int64(4242)),
				StartedAt:          "t",
				StartIdentity:      new("s"),
				CommandFingerprint: "f",
			},
			CreatedAt: "t",
			UpdatedAt: "t",
		}
	}
	for _, actual := range state.AllActualStates {
		if got := actualStateName(lifecycle(actual)); got != string(actual) {
			t.Fatalf("%s: got %q", actual, got)
		}
	}
	if got := actualStateName(nil); got != "stopped" {
		t.Fatalf("nil: got %q", got)
	}
	expected := []struct {
		actual state.ActualServiceState
		pid    *int64
	}{
		{state.ActualStopped, nil},
		{state.ActualQueuedStart, nil},
		{state.ActualPreparing, nil},
		{state.ActualFailed, nil},
		{state.ActualSucceeded, nil},
		{state.ActualExternallyOwned, nil},
		{state.ActualStarting, new(int64(4242))},
		{state.ActualRunningUnready, new(int64(4242))},
		{state.ActualReady, new(int64(4242))},
		{state.ActualStopping, new(int64(4242))},
		{state.ActualOrphaned, new(int64(4242))},
	}
	for _, tc := range expected {
		got := statusPid(lifecycle(tc.actual))
		if tc.pid == nil {
			if got != nil {
				t.Fatalf("%s: expected no pid, got %d", tc.actual, *got)
			}
			continue
		}
		if got == nil || *got != *tc.pid {
			t.Fatalf("%s: expected pid %d, got %v", tc.actual, *tc.pid, got)
		}
	}
}

func TestKillUnownedOnANonStartCommandIsAUsageError(t *testing.T) {
	code, _, errLines := runCLI(t, t.TempDir(), "stop", "api", "--kill-unowned")
	if code != ExitUsage {
		t.Fatalf("expected exit %d, got %d", ExitUsage, code)
	}
	if len(errLines) == 0 || !strings.Contains(errLines[0], "--kill-unowned only applies") {
		t.Fatalf("unexpected stderr: %v", errLines)
	}
}

func TestUnknownCommandIsAUsageError(t *testing.T) {
	code, _, errLines := runCLI(t, t.TempDir(), "bogus")
	if code != ExitUsage {
		t.Fatalf("expected exit %d, got %d", ExitUsage, code)
	}
	if len(errLines) == 0 || !strings.Contains(errLines[0], "unknown command") {
		t.Fatalf("unexpected stderr: %v", errLines)
	}
}

func TestStatusWithoutARunningManagerIsUnavailable(t *testing.T) {
	code, _, errLines := runCLI(t, t.TempDir(), "status")
	if code != ExitUnavailable {
		t.Fatalf("expected exit %d, got %d", ExitUnavailable, code)
	}
	if len(errLines) == 0 || !strings.Contains(errLines[0], "unavailable") {
		t.Fatalf("unexpected stderr: %v", errLines)
	}
}

// `hearth logs <typo>` used to print the usage line, as if the syntax were wrong.
func TestLogsForAnUnknownServiceSaysSo(t *testing.T) {
	code, _, errLines := runCLI(t, t.TempDir(), "logs", "nope")
	if code != ExitUsage {
		t.Fatalf("expected exit %d, got %d", ExitUsage, code)
	}
	if len(errLines) == 0 || !strings.Contains(errLines[0], "unknown service: nope") {
		t.Fatalf("unexpected stderr: %v", errLines)
	}
	if strings.Contains(errLines[0], "usage:") {
		t.Fatalf("must not print the usage line: %v", errLines)
	}
	code, _, errLines = runCLI(t, t.TempDir(), "logs")
	if code != ExitUsage {
		t.Fatalf("expected exit %d, got %d", ExitUsage, code)
	}
	if len(errLines) == 0 || !strings.Contains(errLines[0], "usage: hearth logs") {
		t.Fatalf("unexpected stderr: %v", errLines)
	}
}

// ---------------------------------------------------------------------------------------------
// manager stop wait
// ---------------------------------------------------------------------------------------------

func TestManagerStopWaitsForTheDaemonProcessToExitAndFailsIfItNeverDoes(t *testing.T) {
	var checks atomic.Int64
	waited := waitForDaemonExit(context.Background(), 999_999, 5*time.Second, func(int64) bool {
		return checks.Add(1) < 3
	})
	if waited != nil {
		t.Fatalf("expected success, got %v", waited)
	}
	if checks.Load() != 3 {
		t.Fatalf("expected 3 checks, got %d", checks.Load())
	}

	stuck := waitForDaemonExit(context.Background(), 999_999, 300*time.Millisecond, func(int64) bool { return true })
	if stuck == nil || !strings.Contains(stuck.Error(), "did not exit within") {
		t.Fatalf("expected a timeout error, got %v", stuck)
	}

	// Our own pid is an in-process daemon: never waited on, even though it is alive.
	if err := waitForDaemonExit(context.Background(), int64(os.Getpid()), time.Millisecond, func(int64) bool { return true }); err != nil {
		t.Fatalf("expected success for our own pid, got %v", err)
	}
}

// ---------------------------------------------------------------------------------------------
// mcp install / skill install
// ---------------------------------------------------------------------------------------------

func TestMcpInstallCreatesANewConfigFileWithTheResolvedHearthPath(t *testing.T) {
	dir := t.TempDir()
	configPath := filepath.Join(dir, "mcp.json")
	code, outLines, _ := runCLI(t, dir, "mcp", "install", configPath)
	if code != 0 {
		t.Fatalf("expected exit 0, got %d", code)
	}
	if len(outLines) == 0 || !strings.Contains(outLines[0], `installed mcp server "hearth"`) {
		t.Fatalf("unexpected stdout: %v", outLines)
	}
	written := readJSON(t, configPath)
	entry := written["mcpServers"].(map[string]any)["hearth"].(map[string]any)
	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	if entry["command"] != exe {
		t.Fatalf("command: got %v, want %v", entry["command"], exe)
	}
	expectedRoot := canonicalize(dir)
	args := entry["args"].([]any)
	if len(args) != 3 || args[0] != "--root" || args[1] != expectedRoot || args[2] != "mcp" {
		t.Fatalf("unexpected args: %v", args)
	}
}

func TestMcpInstallPreservesOtherFieldsAndOtherServers(t *testing.T) {
	dir := t.TempDir()
	configPath := filepath.Join(dir, "mcp.json")
	initial := `{"mcpServers":{"other-server":{"command":"node","args":["other.mjs"]},"hearth":{"command":"node","args":["old-wrapper.mjs"],"skill":"local-dev","env":{},"notes":["some historical note"]}}}`
	if err := os.WriteFile(configPath, []byte(initial), 0o644); err != nil {
		t.Fatal(err)
	}
	code, _, _ := runCLI(t, dir, "mcp", "install", configPath)
	if code != 0 {
		t.Fatalf("expected exit 0, got %d", code)
	}
	written := readJSON(t, configPath)
	servers := written["mcpServers"].(map[string]any)
	other := servers["other-server"].(map[string]any)
	if other["command"] != "node" {
		t.Fatalf("other-server command changed: %v", other["command"])
	}
	entry := servers["hearth"].(map[string]any)
	if entry["skill"] != "local-dev" {
		t.Fatalf("skill lost: %v", entry["skill"])
	}
	notes := entry["notes"].([]any)
	if len(notes) != 1 || notes[0] != "some historical note" {
		t.Fatalf("notes lost: %v", notes)
	}
	args := entry["args"].([]any)
	if len(args) == 1 && args[0] == "old-wrapper.mjs" {
		t.Fatalf("args were not replaced: %v", args)
	}
}

func TestMcpInstallPreservesTheOriginalKeyOrderOfAHumanMaintainedFile(t *testing.T) {
	dir := t.TempDir()
	configPath := filepath.Join(dir, "mcp.json")
	initial := "{\n  \"mcpServers\": {\n    \"zebra-server\": { \"command\": \"node\", \"args\": [\"z.mjs\"] },\n    \"hearth\": { \"command\": \"node\", \"args\": [\"old-wrapper.mjs\"] },\n    \"apple-server\": { \"command\": \"node\", \"args\": [\"a.mjs\"] }\n  }\n}"
	if err := os.WriteFile(configPath, []byte(initial), 0o644); err != nil {
		t.Fatal(err)
	}
	code, _, _ := runCLI(t, dir, "mcp", "install", configPath)
	if code != 0 {
		t.Fatalf("expected exit 0, got %d", code)
	}
	written, err := os.ReadFile(configPath)
	if err != nil {
		t.Fatal(err)
	}
	text := string(written)
	zebra := strings.Index(text, "zebra-server")
	hearth := strings.Index(text, `"hearth"`)
	apple := strings.Index(text, "apple-server")
	if !(zebra < hearth && hearth < apple) {
		t.Fatalf("key order was not preserved:\n%s", text)
	}
}

func TestMcpInstallSupportsACustomTopLevelKey(t *testing.T) {
	dir := t.TempDir()
	configPath := filepath.Join(dir, "agent-mcp.json")
	initial := `{"servers":{"hearth":{"skill":"local-dev","command":"node","args":["old.mjs"]}}}`
	if err := os.WriteFile(configPath, []byte(initial), 0o644); err != nil {
		t.Fatal(err)
	}
	code, _, _ := runCLI(t, dir, "mcp", "install", "--key", "servers", configPath)
	if code != 0 {
		t.Fatalf("expected exit 0, got %d", code)
	}
	written := readJSON(t, configPath)
	if _, ok := written["mcpServers"]; ok {
		t.Fatal("must not create a stray \"mcpServers\" key when --key targets a different one")
	}
	entry := written["servers"].(map[string]any)["hearth"].(map[string]any)
	if entry["skill"] != "local-dev" {
		t.Fatalf("skill lost: %v", entry["skill"])
	}
	args := entry["args"].([]any)
	if len(args) == 1 && args[0] == "old.mjs" {
		t.Fatalf("args were not replaced: %v", args)
	}
}

func TestMcpInstallSupportsACustomNameAndMultipleFiles(t *testing.T) {
	dir := t.TempDir()
	a := filepath.Join(dir, "a.json")
	b := filepath.Join(dir, "nested", "b.json")
	code, outLines, _ := runCLI(t, dir, "mcp", "install", "--name", "viclass-hearth", a, b)
	if code != 0 {
		t.Fatalf("expected exit 0, got %d", code)
	}
	if len(outLines) != 2 {
		t.Fatalf("expected 2 lines, got %v", outLines)
	}
	for _, path := range []string{a, b} {
		written := readJSON(t, path)
		entry := written["mcpServers"].(map[string]any)["viclass-hearth"].(map[string]any)
		if _, ok := entry["command"].(string); !ok {
			t.Fatalf("%s: missing command: %v", path, entry)
		}
	}
}

func TestMcpInstallRequiresAtLeastOneConfigFile(t *testing.T) {
	code, _, errLines := runCLI(t, t.TempDir(), "mcp", "install")
	if code != ExitUsage {
		t.Fatalf("expected exit %d, got %d", ExitUsage, code)
	}
	if len(errLines) == 0 || !strings.Contains(errLines[0], "usage: hearth mcp install") {
		t.Fatalf("unexpected stderr: %v", errLines)
	}
}

func TestMcpRejectsAnUnknownSubcommand(t *testing.T) {
	code, _, errLines := runCLI(t, t.TempDir(), "mcp", "bogus")
	if code != ExitUsage {
		t.Fatalf("expected exit %d, got %d", ExitUsage, code)
	}
	if len(errLines) == 0 || !strings.Contains(errLines[0], "unknown mcp subcommand") {
		t.Fatalf("unexpected stderr: %v", errLines)
	}
}

func TestSkillInstallWritesTheSkillPackAtARelativeDest(t *testing.T) {
	dir := t.TempDir()
	code, outLines, _ := runCLI(t, dir, "skill", "install", "--dest", ".agents/skills/hearth")
	if code != 0 {
		t.Fatalf("expected exit 0, got %d", code)
	}
	if len(outLines) == 0 || !strings.Contains(outLines[0], "installed hearth skill") {
		t.Fatalf("unexpected stdout: %v", outLines)
	}
	skillDir := filepath.Join(dir, ".agents/skills/hearth")
	written, err := os.ReadFile(filepath.Join(skillDir, "SKILL.md"))
	if err != nil {
		t.Fatal(err)
	}
	if string(written) != skillMarkdown {
		t.Fatal("SKILL.md content mismatch")
	}
	if !strings.Contains(string(written), "scripts/manage.sh") || !strings.Contains(string(written), "`hearth`") {
		t.Fatalf("SKILL.md missing expected content:\n%s", written)
	}
	if strings.Contains(string(written), "hearthd") {
		t.Fatal("SKILL.md must not mention hearthd")
	}
	wrapper := filepath.Join(skillDir, "scripts/hearth.sh")
	info, err := os.Stat(wrapper)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode()&0o111 != 0o111 {
		t.Fatalf("hearth.sh should be executable, mode %v", info.Mode())
	}
	wrapperBody, err := os.ReadFile(wrapper)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(wrapperBody), "HEARTH_BIN") || !strings.Contains(string(wrapperBody), ".local/bin/hearth") {
		t.Fatalf("wrapper missing expected content:\n%s", wrapperBody)
	}
	if strings.Contains(string(wrapperBody), "hearthd") {
		t.Fatal("wrapper must not mention hearthd")
	}
	for _, name := range []string{"status.sh", "shared-connection.sh"} {
		if _, err := os.Stat(filepath.Join(skillDir, "scripts", name)); err != nil {
			t.Fatalf("missing %s: %v", name, err)
		}
	}
}

func TestSkillInstallNormalizesASkillMdDestToItsParentDirectory(t *testing.T) {
	dir := t.TempDir()
	code, outLines, _ := runCLI(t, dir, "skill", "install", "--dest", ".agents/skills/hearth/SKILL.md")
	if code != 0 {
		t.Fatalf("expected exit 0, got %d", code)
	}
	if len(outLines) == 0 || !strings.Contains(outLines[0], "installed hearth skill") {
		t.Fatalf("unexpected stdout: %v", outLines)
	}
	if _, err := os.Stat(filepath.Join(dir, ".agents/skills/hearth/SKILL.md")); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(dir, ".agents/skills/hearth/scripts/hearth.sh")); err != nil {
		t.Fatal(err)
	}
}

func TestSkillInstallRequiresADestFlag(t *testing.T) {
	code, _, errLines := runCLI(t, t.TempDir(), "skill", "install")
	if code != ExitUsage {
		t.Fatalf("expected exit %d, got %d", ExitUsage, code)
	}
	if len(errLines) == 0 || !strings.Contains(errLines[0], "usage: hearth skill install") {
		t.Fatalf("unexpected stderr: %v", errLines)
	}
}

func readJSON(t *testing.T, path string) map[string]any {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var decoded map[string]any
	if err := json.Unmarshal(data, &decoded); err != nil {
		t.Fatalf("%s: %v", path, err)
	}
	return decoded
}
