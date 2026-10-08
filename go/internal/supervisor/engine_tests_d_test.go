// Ported from rust/crates/hearth-core/src/supervisor/engine/tests.rs (batch D).
package supervisor

import (
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

func harnessWithProbes(cat catalog.ServiceCatalog, probes ProbeAdapter, timeoutMs int64) (*ProcessSupervisor, *fakeHost) {
	host := newFakeHost(cat)
	process := newFakeProcAdapter()
	sup := NewProcessSupervisor(host, SupervisorOptions{
		Process:            process,
		RunBuild:           newFakeRunBuild(),
		Probes:             probes,
		Preparation:        noPreparation{},
		Clock:              newFakeClock(),
		ReadinessTimeoutMs: timeoutMs,
		ReadinessBackoffMs: 5,
		TerminationGraceMs: 50,
		IsClosing:          func() bool { return false },
	})
	return sup, host
}

func TestArtifactServiceInstallsBeforeStarting(t *testing.T) {
	installer := newFakeArtifactInstaller()
	sup, host := artifactHarness(artifactService("db"), installer)
	if err := sup.Start("db", nil); err != nil {
		t.Fatal(err)
	}
	calls := installer.callList()
	if len(calls) != 1 || calls[0] != "db" {
		t.Fatalf("installer calls %v", calls)
	}
	// `process` readiness ends the start at running-unready — the proof we want is that the
	// install ran before spawn and nothing failed.
	if host.stateOf("db").ActualState != state.ActualRunningUnready {
		t.Fatalf("state %s", host.stateOf("db").ActualState)
	}
}

func TestArtifactInstallFailureFailsTheStart(t *testing.T) {
	installer := newFakeArtifactInstaller()
	installer.setResult(NewError("sha256 mismatch"))
	sup, host := artifactHarness(artifactService("db"), installer)
	if err := sup.Start("db", nil); err == nil {
		t.Fatal("expected a failure")
	}
	st := host.stateOf("db")
	if st.ActualState != state.ActualFailed {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Error == nil || *st.Error != "Install failed: sha256 mismatch" {
		t.Fatalf("error %v", st.Error)
	}
}

func TestArtifactWithoutAnInstallerFailsClearly(t *testing.T) {
	sup, host := artifactHarness(artifactService("db"), nil)
	if err := sup.Start("db", nil); err == nil {
		t.Fatal("expected a failure")
	}
	st := host.stateOf("db")
	if st.ActualState != state.ActualFailed {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Error == nil || !strings.Contains(*st.Error, "not supported") {
		t.Fatalf("error %v", st.Error)
	}
}

func TestPreparationCommandRunsBeforeStartAndSucceedsWhenTheProbeReportsReady(t *testing.T) {
	inner := newFakeProbeAdapter()
	inner.setTCPReady(8080, true)
	yes := true
	probes := newCommandCapableProbeAdapter(inner, &yes, 0)
	sup, host := harnessWithProbes(preparationCatalog(), probes, 200)
	if err := sup.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	if host.stateOf("api").ActualState != state.ActualReady {
		t.Fatalf("state %s", host.stateOf("api").ActualState)
	}
}

func TestPreparationCommandFailureFailsTheStartNotJustReadiness(t *testing.T) {
	inner := newFakeProbeAdapter()
	no := false
	probes := newCommandCapableProbeAdapter(inner, &no, 0)
	sup, host := harnessWithProbes(preparationCatalog(), probes, 200)
	err := sup.Start("api", nil)
	if err == nil || err.Error() != "Preparation failed for api" {
		t.Fatalf("err %v", err)
	}
	st := host.stateOf("api")
	if st.ActualState != state.ActualFailed {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Error == nil || *st.Error != "Preparation failed" {
		t.Fatalf("error %v", st.Error)
	}
}

func TestPreparationCommandWithNoCommandProbeConfiguredFailsTheStartImmediately(t *testing.T) {
	// Unlike command readiness (which stays `running-unready` and keeps probing), a preparation
	// command with no way to run it can never succeed by waiting longer.
	h := buildHarnessWithTimeout(preparationCatalog(), 20)
	err := h.supervisor.Start("api", nil)
	if err == nil || err.Error() != "Preparation failed for api" {
		t.Fatalf("err %v", err)
	}
}

func TestPreparationCommandSerializationByKeyRunsOneAtATime(t *testing.T) {
	serviceWithPrep := func(id, key string) catalog.ServiceDefinition {
		k := key
		return withPreparationCommand(
			argvVerified(id, catalog.ReadinessSpec{Kind: "process"}),
			catalog.PreparationCommand{
				Command:          catalog.CommandSpec{Argv: []string{"prepare"}},
				SerializationKey: &k,
			},
		)
	}
	cat := catalog.ServiceCatalog{
		Services:           []catalog.ServiceDefinition{serviceWithPrep("a", "shared"), serviceWithPrep("b", "shared")},
		Groups:             map[string][]string{},
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
	}
	yes := true
	probes := newCommandCapableProbeAdapter(nil, &yes, 5)
	sup, _ := harnessWithProbes(cat, probes, 200)
	var wg sync.WaitGroup
	var errA, errB error
	wg.Add(2)
	go func() { defer wg.Done(); errA = sup.Start("a", nil) }()
	go func() { defer wg.Done(); errB = sup.Start("b", nil) }()
	wg.Wait()
	if errA != nil || errB != nil {
		t.Fatalf("errs %v %v", errA, errB)
	}
	probes.mu.Lock()
	timeline := append([]time.Time{}, probes.timeline...)
	probes.mu.Unlock()
	if len(timeline) != 2 {
		t.Fatalf("timeline %v", timeline)
	}
	gap := timeline[1].Sub(timeline[0])
	if gap < 4*time.Millisecond {
		t.Fatalf("preparation commands sharing a serializationKey must not overlap, gap was %v", gap)
	}
}

func TestSyncExternalServicesAdoptsOnReadyAndReleasesOnNotReady(t *testing.T) {
	service := argvVerified("cache", catalog.ReadinessSpec{Kind: "tcp", Port: port(6379)})
	own := catalog.OwnershipExternal
	service.Ownership = &own
	h := buildHarness(oneServiceCatalog(service))
	h.probes.setTCPReady(6379, true)
	h.supervisor.SyncExternalServices()
	st := h.host.stateOf("cache")
	if st.ActualState != state.ActualReady {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.ReadinessDetail == nil || *st.ReadinessDetail != "adopted from external state" {
		t.Fatalf("detail %v", st.ReadinessDetail)
	}
	if st.Identity != nil {
		t.Fatal("external services never get a ProcessIdentity")
	}
	h.probes.setTCPReady(6379, false)
	h.supervisor.SyncExternalServices()
	if h.host.stateOf("cache").ActualState != state.ActualStopped {
		t.Fatalf("state %s", h.host.stateOf("cache").ActualState)
	}
}
