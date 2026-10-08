// Shared helpers for the engine tests — the Go port of the helper functions in
// rust/crates/hearth-core/src/supervisor/engine/tests.rs.
package supervisor

import (
	"sync"
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

func withExitTimeout(service catalog.ServiceDefinition, timeoutMs uint64) catalog.ServiceDefinition {
	if service.Profiles.Run.Command != nil {
		t := timeoutMs
		service.Profiles.Run.ReadinessTimeoutMs = &t
	}
	return service
}

func shellService(id, cwd, shell string, port uint16) catalog.ServiceDefinition {
	service := argvVerified(id, catalog.ReadinessSpec{Kind: "tcp", Port: &port})
	if service.Profiles.Run.Command != nil {
		exec := true
		service.Profiles.Run.Command.Cwd = cwd
		service.Profiles.Run.Command.Command = catalog.CommandSpec{Shell: shell, Exec: &exec}
	}
	return service
}

func shellCommand(cwd, shell string) catalog.ServiceCommand {
	exec := true
	return catalog.ServiceCommand{
		Command: catalog.CommandSpec{Shell: shell, Exec: &exec},
		Cwd:     cwd,
	}
}

func seedExternallyOwned(host *fakeHost, serviceID string, desired state.DesiredServiceState, errText string) {
	kind := state.ReadinessKindTCP
	host.seed(state.ServiceLifecycleState{
		ServiceID:     serviceID,
		DesiredState:  desired,
		ActualState:   state.ActualExternallyOwned,
		Readiness:     state.ReadinessFailed,
		Generation:    2,
		ReadinessKind: &kind,
		CreatedAt:     "2024-01-01T00:00:00.000Z",
		UpdatedAt:     "2024-01-01T00:00:00.000Z",
		Error:         &errText,
	})
}

func posixPid(t *testing.T, h *harness, serviceID string) int64 {
	t.Helper()
	st := h.host.stateOf(serviceID)
	if st == nil || st.Identity == nil || st.Identity.IsDocker() {
		t.Fatalf("expected posix identity for %s, got %+v", serviceID, st)
	}
	return st.Identity.PidValue()
}

func withRestart(service catalog.ServiceDefinition, on string, max *uint32, delayMs *uint64) catalog.ServiceDefinition {
	trigger := catalog.RestartNever
	switch on {
	case "always":
		trigger = catalog.RestartAlways
	case "on-failure":
		trigger = catalog.RestartOnFailure
	}
	service.Restart = &catalog.ServiceRestartPolicy{On: trigger, MaxRestarts: max, DelayMs: delayMs}
	return service
}

func redisService() catalog.ServiceDefinition {
	return withArgv(
		argvVerified("redis", catalog.ReadinessSpec{Kind: "tcp", Port: port(6380)}),
		[]string{"/opt/redis/bin/redis-server", "/data/redis@8/redis.conf"},
	)
}

func externalCommandService(id string) catalog.ServiceDefinition {
	service := argvVerified(id, catalog.ReadinessSpec{
		Kind:    "command",
		Command: &catalog.CommandSpec{Argv: []string{"probe"}},
	})
	own := catalog.OwnershipExternal
	service.Ownership = &own
	return service
}

func argvCommandForTest(id string) catalog.ServiceCommand {
	return catalog.ServiceCommand{
		Command: catalog.CommandSpec{Argv: []string{id}},
		Cwd:     ".",
	}
}

// oneShotSupervisor builds a supervisor whose probe adapter can answer `command` readiness.
func oneShotSupervisor(service catalog.ServiceDefinition, probe *bool) (*fakeHost, *fakeProcAdapter, *ProcessSupervisor) {
	host := newFakeHost(oneServiceCatalog(service))
	process := newFakeProcAdapter()
	probes := newCommandCapableProbeAdapter(nil, probe, 0)
	sup := NewProcessSupervisor(host, SupervisorOptions{
		Process:            process,
		RunBuild:           newFakeRunBuild(),
		Probes:             probes,
		Preparation:        noPreparation{},
		Clock:              newFakeClock(),
		ReadinessTimeoutMs: 60_000,
		ReadinessBackoffMs: 5,
		TerminationGraceMs: 50,
		IsClosing:          func() bool { return false },
	})
	return host, process, sup
}

func newCommandCapableProbeAdapter(inner *fakeProbeAdapter, result *bool, delayMs int64) *commandCapableProbeAdapter {
	return &commandCapableProbeAdapter{inner: inner, result: result, delayMs: delayMs}
}

// --- Fake artifact installer ---

type fakeArtifactInstaller struct {
	mu     sync.Mutex
	calls  []string
	result error
}

func newFakeArtifactInstaller() *fakeArtifactInstaller { return &fakeArtifactInstaller{} }

func (f *fakeArtifactInstaller) Install(service *catalog.ServiceDefinition, _ OnOutput) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.calls = append(f.calls, service.ID)
	return f.result
}

func (f *fakeArtifactInstaller) setResult(err error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.result = err
}

func (f *fakeArtifactInstaller) callList() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]string{}, f.calls...)
}

func artifactService(id string) catalog.ServiceDefinition {
	service := argvVerified(id, catalog.ReadinessSpec{Kind: "process"})
	url := "file:///tmp/x.tgz"
	sha := "0000000000000000000000000000000000000000000000000000000000000000"
	installDir := "/tmp/x"
	dataDir := "/tmp/x-data"
	service.Artifact = &catalog.ServiceArtifact{
		Version:    "1.0",
		URL:        &url,
		Sha256:     &sha,
		InstallDir: &installDir,
		DataDir:    &dataDir,
	}
	return service
}

func artifactHarness(service catalog.ServiceDefinition, installer ArtifactInstaller) (*ProcessSupervisor, *fakeHost) {
	host := newFakeHost(oneServiceCatalog(service))
	process := newFakeProcAdapter()
	probes := newFakeProbeAdapter()
	sup := NewProcessSupervisor(host, SupervisorOptions{
		Process:            process,
		RunBuild:           newFakeRunBuild(),
		Probes:             probes,
		Preparation:        noPreparation{},
		ArtifactInstaller:  installer,
		Clock:              newFakeClock(),
		ReadinessTimeoutMs: 200,
		ReadinessBackoffMs: 5,
		TerminationGraceMs: 50,
		IsClosing:          func() bool { return false },
	})
	return sup, host
}

func preparationCatalog() catalog.ServiceCatalog {
	service := withPreparationCommand(
		argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)}),
		catalog.PreparationCommand{
			Command: catalog.CommandSpec{Argv: []string{"prepare"}},
			Cwd:     strPtr("infra"),
		},
	)
	return oneServiceCatalog(service)
}
