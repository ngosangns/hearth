// Ported from rust/crates/hearth-core/src/supervisor/engine/tests.rs (batch E).
package supervisor

import (
	"strings"
	"testing"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

func containerService(id, container, shell string) catalog.ServiceDefinition {
	service := argvVerified(id, catalog.ReadinessSpec{Kind: "container"})
	cmd := catalog.ServiceCommand{
		Command:       catalog.CommandSpec{Shell: shell},
		Cwd:           ".",
		ContainerName: &container,
	}
	service.Profiles.Run = catalog.ServiceRunProfile{
		CommandStatus: "verified",
		Command:       &cmd,
		Readiness:     catalog.ReadinessSpec{Kind: "container"},
	}
	return service
}

func TestContainerReadinessIdentityRoundTrips(t *testing.T) {
	service := containerService("db", "proj-db", "docker compose up db")
	h := buildHarness(oneServiceCatalog(service))
	cmd := *service.Profiles.Run.Command
	h.process.registerContainer("proj-db", DockerContainerRecord{
		ContainerName:      "proj-db",
		ContainerID:        "abc123",
		ContainerStartedAt: "2024-01-01T00:00:00.000Z",
		CommandFingerprint: NormalizeCommandFingerprint(&cmd),
	})
	h.probes.setContainerReady("proj-db", true)
	if err := h.supervisor.Start("db", nil); err != nil {
		t.Fatal(err)
	}
	st := h.host.stateOf("db")
	if st.ActualState != state.ActualReady {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Identity == nil || !st.Identity.IsDocker() || st.Identity.ContainerName == nil || *st.Identity.ContainerName != "proj-db" {
		t.Fatalf("identity %+v", st.Identity)
	}
	found := false
	for _, c := range h.process.attachCalls() {
		if c.serviceID == "db" && c.kind == "container" {
			found = true
		}
	}
	if !found {
		t.Fatalf("a container service must get a container log tail: %v", h.process.attachCalls())
	}
}

func TestExternalContainerAdoptionAttachesOneLogTailAndReleasesIt(t *testing.T) {
	service := containerService("cache", "proj-cache", "docker compose up")
	own := catalog.OwnershipExternal
	service.Ownership = &own
	h := buildHarness(oneServiceCatalog(service))
	h.probes.setContainerReady("proj-cache", true)
	h.supervisor.SyncExternalServices()
	if h.host.stateOf("cache").ActualState != state.ActualReady {
		t.Fatalf("state %s", h.host.stateOf("cache").ActualState)
	}
	calls := h.process.attachCalls()
	if len(calls) != 1 || calls[0].serviceID != "cache" || calls[0].kind != "container" {
		t.Fatalf("attach calls %v", calls)
	}
	// `sync_external_services` polls on an interval — an already-attached live tail must not be
	// replaced every tick.
	h.supervisor.SyncExternalServices()
	if len(h.process.attachCalls()) != 1 {
		t.Fatal("a live tail must not be re-attached")
	}
	// A follower that ended on its own (container restarted) is re-attached on the next tick.
	h.process.finishTail("cache")
	h.supervisor.SyncExternalServices()
	if len(h.process.attachCalls()) != 2 {
		t.Fatal("a dead tail must be re-attached")
	}
	h.probes.setContainerReady("proj-cache", false)
	h.supervisor.SyncExternalServices()
	if h.host.stateOf("cache").ActualState != state.ActualStopped {
		t.Fatalf("state %s", h.host.stateOf("cache").ActualState)
	}
	if !h.process.tailWasStopped("cache") {
		t.Fatal("release must stop the log tail")
	}
}

func TestSpawnFailureTransitionsToFailedWithTheAdapterError(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.process.failSpawn("api", "no such file or directory")
	err := h.supervisor.Start("api", nil)
	if err == nil || err.Error() != "no such file or directory" {
		t.Fatalf("err %v", err)
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualFailed {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Error == nil || *st.Error != "no such file or directory" {
		t.Fatalf("error %v", st.Error)
	}
}

// The `shared:` expansion relies on this: an `ownership: external` service whose readiness is a
// `command` probe treats its run command as a one-shot task.
func TestAnExternalCommandReadinessServiceRunsItsCommandAsAOneShotTask(t *testing.T) {
	service := argvVerified("api", catalog.ReadinessSpec{
		Kind:    "command",
		Command: &catalog.CommandSpec{Argv: []string{"attach"}},
	})
	own := catalog.OwnershipExternal
	service.Ownership = &own
	host := newFakeHost(oneServiceCatalog(service))
	process := newFakeProcAdapter()
	yes := true
	probes := newCommandCapableProbeAdapter(nil, &yes, 0)
	sup := NewProcessSupervisor(host, SupervisorOptions{
		Process:            process,
		RunBuild:           newFakeRunBuild(),
		Probes:             probes,
		Preparation:        noPreparation{},
		Clock:              newFakeClock(),
		ReadinessTimeoutMs: 200,
		ReadinessBackoffMs: 5,
		TerminationGraceMs: 50,
		IsClosing:          func() bool { return false },
	})
	if err := sup.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	st := host.stateOf("api")
	if st.ActualState != state.ActualReady {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Identity != nil {
		t.Fatal("a task run must not track a process identity")
	}
	process.mu.Lock()
	alive := len(process.state.alive)
	process.mu.Unlock()
	if alive != 1 {
		t.Fatalf("the attach task must still be spawned once, alive=%d", alive)
	}
}

func TestReconcileMarksAnExternallyKilledProcessAsFailed(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	pid := apiPid(t, h)
	// Simulate the process dying on its own, out from under the supervisor.
	h.process.killExternally(pid, -1)
	h.supervisor.Reconcile()
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualFailed {
		t.Fatalf("state %s", st.ActualState)
	}
	errText := ""
	if st.Error != nil {
		errText = *st.Error
	}
	if !strings.Contains(errText, "no longer alive") && !strings.Contains(errText, "exited with code") {
		t.Fatalf("error %q", errText)
	}
}

func TestStopDoesNotSignalWhenTheProcessTreeCannotBeRead(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	h.process.setTreeUnknown()
	err := h.supervisor.Stop("api", nil)
	if err == nil || (!strings.Contains(err.Error(), "could not be read") && !strings.Contains(err.Error(), "could not be verified")) {
		t.Fatalf("err %v", err)
	}
	if len(h.process.signals()) != 0 {
		t.Fatalf("a failed ps must not signal a pgid: %v", h.process.signals())
	}
	if h.host.stateOf("api").ActualState == state.ActualStopped {
		t.Fatal("state must not be Stopped")
	}
}

func TestALeaderExitReapsAChildInItsOwnProcessGroup(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	leader := apiPid(t, h)
	child := h.process.forkIntoOwnGroup(leader)
	// The sampler records the tree while the leader is alive, then the leader exits on its own.
	time.Sleep(350 * time.Millisecond)
	h.process.killExternally(leader, -1)
	waitDead(t, h.process, child)
	sawChild := false
	sawLeader := false
	for _, s := range h.process.signals() {
		if s.pid == child && s.signal == SignalTerm {
			sawChild = true
		}
		if s.pid == leader {
			sawLeader = true
		}
	}
	if !sawChild {
		t.Fatalf("secondary group must be signalled: %v", h.process.signals())
	}
	if sawLeader {
		t.Fatal("the dead leader pgid must not be signalled")
	}
	waitUntilActual(t, h.host, "api", state.ActualFailed)
}

func TestAFailedOneShotTaskIsStoppedAndTheErrorIsReturned(t *testing.T) {
	service := argvVerified("api", catalog.ReadinessSpec{
		Kind:    "command",
		Command: &catalog.CommandSpec{Argv: []string{"attach"}},
	})
	own := catalog.OwnershipExternal
	service.Ownership = &own
	host := newFakeHost(oneServiceCatalog(service))
	process := newFakeProcAdapter()
	no := false
	probes := newCommandCapableProbeAdapter(nil, &no, 0)
	sup := NewProcessSupervisor(host, SupervisorOptions{
		Process:            process,
		RunBuild:           newFakeRunBuild(),
		Probes:             probes,
		Preparation:        noPreparation{},
		Clock:              newFakeClock(),
		ReadinessTimeoutMs: 30,
		ReadinessBackoffMs: 5,
		TerminationGraceMs: 50,
		IsClosing:          func() bool { return false },
	})
	err := sup.Start("api", nil)
	if err == nil || !strings.Contains(err.Error(), "Readiness timed out") {
		t.Fatalf("err %v", err)
	}
	process.mu.Lock()
	alive := len(process.state.alive)
	process.mu.Unlock()
	if alive != 0 {
		t.Fatal("the one-shot process must be stopped when readiness fails")
	}
	if host.stateOf("api").Identity != nil {
		t.Fatal("a task failure must not persist the trigger process as the service identity")
	}
}
