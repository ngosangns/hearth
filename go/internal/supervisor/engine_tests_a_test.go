// Ported from rust/crates/hearth-core/src/supervisor/engine/tests.rs (batch A).
package supervisor

import (
	"strings"
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

func apiPid(t *testing.T, h *harness) int64 {
	t.Helper()
	st := h.host.stateOf("api")
	if st == nil || st.Identity == nil || st.Identity.IsDocker() {
		t.Fatalf("expected a posix identity, got %+v", st)
	}
	return st.Identity.PidValue()
}

// externalContainerService is an `ownership: external` container unit, shaped like a real one:
// a `docker compose up -d` run command, a container readiness probe, and — optionally — the
// `stop:` command that is the only way this manager can stop it.
func externalContainerService(id, container string, stop *string) catalog.ServiceDefinition {
	service := argvVerified(id, catalog.ReadinessSpec{Kind: "container"})
	own := catalog.OwnershipExternal
	service.Ownership = &own
	var stopCmd *catalog.CommandSpec
	if stop != nil {
		stopCmd = &catalog.CommandSpec{Shell: *stop}
	}
	cmd := catalog.ServiceCommand{
		Command:           catalog.CommandSpec{Shell: "docker compose up -d " + container},
		Cwd:               ".",
		ContainerName:     &container,
		DockerStopCommand: stopCmd,
	}
	service.Profiles.Run = catalog.ServiceRunProfile{
		CommandStatus: "verified",
		Command:       &cmd,
		Readiness:     catalog.ReadinessSpec{Kind: "container"},
	}
	return service
}

// adoptedExternalState is the state `sync_external_services` leaves behind for an adopted
// external unit: ready, no process identity, and a readiness detail saying where that readiness
// came from.
func adoptedExternalState(serviceID string) state.ServiceLifecycleState {
	detail := "adopted from external state"
	return state.ServiceLifecycleState{
		ServiceID:       serviceID,
		DesiredState:    state.DesiredRunning,
		ActualState:     state.ActualReady,
		Readiness:       state.ReadinessReady,
		Generation:      1,
		ReadinessDetail: &detail,
		CreatedAt:       "2024-01-01T00:00:00.000Z",
		UpdatedAt:       "2024-01-01T00:00:00.000Z",
	}
}

func TestStartsAStoppedServiceToReadyWithTCPReadiness(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualReady || st.Readiness != state.ReadinessReady {
		t.Fatalf("state %+v", st)
	}
	if st.Identity == nil {
		t.Fatal("expected an identity")
	}
}

func TestProcessReadinessReportsRunningUnreadyAsTheTerminalState(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "process"})))
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualRunningUnready {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.ReadinessDetail == nil || *st.ReadinessDetail != "process-liveness-only" {
		t.Fatalf("detail %v", st.ReadinessDetail)
	}
}

func TestRestartStopsThenStarts(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	first := apiPid(t, h)
	if err := h.supervisor.Restart("api", nil); err != nil {
		t.Fatal(err)
	}
	second := apiPid(t, h)
	if first == second {
		t.Fatal("restart should spawn a fresh process")
	}
	found := false
	for _, s := range h.process.signals() {
		if s.pid == first && s.signal == SignalTerm {
			found = true
		}
	}
	if !found {
		t.Fatalf("expected a SIGTERM to the first leader group: %v", h.process.signals())
	}
}

// A second process with the same command is not the tracked one. Restart kills it and the child
// that moved into its own process group, then starts a fresh process.
func TestRestartKillsADuplicateProcessAndItsTree(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	fingerprint := h.host.stateOf("api").Identity.CommandFingerprint
	duplicate := int64(424_242)
	h.process.insertAlive(duplicate, fingerprint, "Dup Jan  1 00:00:01 2024")
	child := h.process.forkIntoOwnGroup(duplicate)
	if err := h.supervisor.Restart("api", nil); err != nil {
		t.Fatal(err)
	}
	if h.process.isAlive(duplicate) {
		t.Fatal("duplicate survived")
	}
	if h.process.isAlive(child) {
		t.Fatal("child survived")
	}
	sawDup, sawChild := false, false
	for _, s := range h.process.pidSignals() {
		if s.pid == duplicate && s.signal == SignalTerm {
			sawDup = true
		}
		if s.pid == child && s.signal == SignalTerm {
			sawChild = true
		}
	}
	if !sawDup || !sawChild {
		t.Fatalf("signals %v", h.process.pidSignals())
	}
	if h.host.stateOf("api").ActualState != state.ActualReady {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
}

func TestRestartFailsWhenADuplicateCannotBeKilled(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	fingerprint := h.host.stateOf("api").Identity.CommandFingerprint
	duplicate := int64(424_242)
	h.process.insertAlive(duplicate, fingerprint, "Dup Jan  1 00:00:01 2024")
	h.process.markUnkillable(duplicate)
	err := h.supervisor.Restart("api", nil)
	if err == nil || !strings.Contains(err.Error(), "duplicate") {
		t.Fatalf("err %v", err)
	}
	if !h.process.isAlive(duplicate) {
		t.Fatal("duplicate should survive")
	}
}

func TestRestartFailsWhenDuplicateProcessesCannotBeListed(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.process.setMatchesUnknown()
	err := h.supervisor.Restart("api", nil)
	if err == nil || !strings.Contains(err.Error(), "could not list processes") {
		t.Fatalf("err %v", err)
	}
}

// The whole-tree stop rule: `air` runs the real server in its own process group, so a stop that
// signals only the tracked pgid leaves it alive holding the port.
func TestStopSignalsAChildThatMovedIntoItsOwnProcessGroup(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	leader := apiPid(t, h)
	child := h.process.forkIntoOwnGroup(leader)
	if err := h.supervisor.Stop("api", nil); err != nil {
		t.Fatal(err)
	}
	sawLeader, sawChild := false, false
	for _, s := range h.process.signals() {
		if s.pid == leader && s.signal == SignalTerm {
			sawLeader = true
		}
		if s.pid == child && s.signal == SignalTerm {
			sawChild = true
		}
	}
	if !sawLeader {
		t.Fatalf("leader group not signalled: %v", h.process.signals())
	}
	if !sawChild {
		t.Fatalf("the secondary group must be signalled: %v", h.process.signals())
	}
	if h.process.isAlive(child) {
		t.Fatal("the child in its own process group must not survive the stop")
	}
	if h.host.stateOf("api").ActualState != state.ActualStopped {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
}

func TestContinuousProbeDropsReadyThenRecoversWithoutStatus(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	if h.host.stateOf("api").ActualState != state.ActualReady {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
	h.probes.setTCPReady(8080, false)
	waitUntilActual(t, h.host, "api", state.ActualRunningUnready)
	// `status` no longer probes. The continuous loop owns this edge.
	if err := h.supervisor.Status("api"); err != nil {
		t.Fatal(err)
	}
	if h.host.stateOf("api").ActualState != state.ActualRunningUnready {
		t.Fatalf("status changed state to %s", h.host.stateOf("api").ActualState)
	}
	h.probes.setTCPReady(8080, true)
	waitUntilActual(t, h.host, "api", state.ActualReady)
	st := h.host.stateOf("api")
	if st.Readiness != state.ReadinessReady {
		t.Fatalf("readiness %s", st.Readiness)
	}
	if len(h.process.signals()) != 0 {
		t.Fatal("a failed probe must not kill the process")
	}
}

// A probe that keeps passing must not call `set_service_state`. That write is what publishes
// `service.lifecycle` and persists the row.
func TestContinuousProbeDoesNotWriteStateWhileTheProbeKeepsPassing(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	if h.host.stateOf("api").ActualState != state.ActualReady {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
	writes := h.host.stateWritesCount()
	started := h.clock.NowMillis()
	deadline := nowPlus2s()
	for h.clock.NowMillis() < started+50 {
		if afterDeadline(deadline) {
			t.Fatalf("the continuous probe did not advance the clock (last %d, writes %d)", h.clock.NowMillis(), h.host.stateWritesCount())
		}
		gosched()
	}
	if h.host.stateWritesCount() != writes {
		t.Fatalf("a passing probe must not rewrite an already-ready service (writes %d -> %d)", writes, h.host.stateWritesCount())
	}
	if h.host.stateOf("api").ActualState != state.ActualReady {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
}

func TestTransitionsLeaveTheSingleLifecyclePublishToTheHost(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	published := 0
	for _, e := range h.host.publishedEvents() {
		if e.eventType == "service.lifecycle" {
			published++
		}
	}
	if published != 0 {
		t.Fatalf("`Host::set_service_state` owns the service.lifecycle event, got %d", published)
	}
}

func TestStopOnAnAlreadyStoppedServiceIsANoop(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	if err := h.supervisor.Stop("api", nil); err != nil {
		t.Fatal(err)
	}
	var found *state.ServiceLifecycleState
	for _, s := range h.host.ServiceStates() {
		if s.ServiceID == "api" {
			cp := s
			found = &cp
		}
	}
	if found == nil || found.ActualState != state.ActualStopped {
		t.Fatalf("state %+v", found)
	}
	if h.host.stateOf("api") != nil {
		t.Fatal("a true no-op must never write to persisted state")
	}
	if len(h.process.signals()) != 0 {
		t.Fatal("no signals expected")
	}
}

// An adopted `ownership: external` unit carries no process identity, so the catalog's `stop:`
// command is the only thing that can stop it.
func TestStopOnAnAdoptedExternalServiceRunsItsStopCommand(t *testing.T) {
	h := buildHarness(oneServiceCatalog(externalContainerService("cache", "proj-cache", strPtr("docker compose stop cache"))))
	h.host.seed(adoptedExternalState("cache"))
	if err := h.supervisor.Stop("cache", nil); err != nil {
		t.Fatal(err)
	}
	calls := h.process.stopContainerCalls()
	if len(calls) != 1 || calls[0] != "proj-cache" {
		t.Fatalf("stop container calls %v", calls)
	}
	st := h.host.stateOf("cache")
	if st.ActualState != state.ActualStopped || st.DesiredState != state.DesiredStopped {
		t.Fatalf("state %+v", st)
	}
	if st.Error != nil {
		t.Fatalf("a successful stop must not leave the old error behind: %v", *st.Error)
	}
}

// A service whose port is held by a process this manager does not own has nothing to stop and no
// `stop:` command to run. Stop must say so.
func TestStopOnAnExternallyOwnedServiceWithoutAStopCommandFailsLoudly(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setPortInUse(8080, true)
	startErr := h.supervisor.Start("api", nil)
	if startErr == nil || !strings.Contains(startErr.Error(), "externally owned") {
		t.Fatalf("start err %v", startErr)
	}
	err := h.supervisor.Stop("api", nil)
	if err == nil {
		t.Fatal("expected a refusal")
	}
	for _, want := range []string{"cannot be stopped", "Port 8080 is held by an unowned process", "no `stop` command"} {
		if !strings.Contains(err.Error(), want) {
			t.Fatalf("err %q missing %q", err.Error(), want)
		}
	}
	if h.host.stateOf("api").ActualState != state.ActualExternallyOwned {
		t.Fatalf("a refused stop must not pretend the service is stopped: %s", h.host.stateOf("api").ActualState)
	}
}

// An external unit with no `stop:` command cannot be stopped by this manager either.
func TestStopOnAnExternalServiceWithoutAStopCommandFailsLoudly(t *testing.T) {
	h := buildHarness(oneServiceCatalog(externalContainerService("cache", "proj-cache", nil)))
	h.host.seed(adoptedExternalState("cache"))
	err := h.supervisor.Stop("cache", nil)
	if err == nil {
		t.Fatal("expected a refusal")
	}
	for _, want := range []string{"cannot be stopped", "owned externally"} {
		if !strings.Contains(err.Error(), want) {
			t.Fatalf("err %q missing %q", err.Error(), want)
		}
	}
	if len(h.process.stopContainerCalls()) != 0 {
		t.Fatal("nothing may be run when the catalog declares no stop command")
	}
	if h.host.stateOf("cache").ActualState != state.ActualReady {
		t.Fatalf("state %s", h.host.stateOf("cache").ActualState)
	}
}

// A stop must never report success for a process it cannot touch: the pid is alive but its start
// time differs, so it was reused.
func TestStopRefusesAnOrphanedPidWhoseStartTimeChanged(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	pid := apiPid(t, h)
	h.process.reusePid(pid, "Other Jan  2 00:00:00 2024", "not-the-same-program-anymore")
	if err := h.supervisor.Status("api"); err != nil {
		t.Fatal(err)
	}
	if h.host.stateOf("api").ActualState != state.ActualOrphaned {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
	err := h.supervisor.Stop("api", nil)
	if err == nil || !strings.Contains(err.Error(), "cannot be stopped") || !strings.Contains(err.Error(), "pid "+itoa(pid)) {
		t.Fatalf("err %v", err)
	}
	if len(h.process.signals()) != 0 {
		t.Fatal("a reused pid must never be signalled")
	}
	if !h.process.isAlive(pid) {
		t.Fatal("pid should still be alive")
	}
	if h.host.stateOf("api").ActualState != state.ActualOrphaned {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
}

// Same start time, same command line changed: restart's stop phase refuses it too.
func TestRestartRefusesAnOrphanedPidWhoseStartTimeChanged(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	pid := apiPid(t, h)
	h.process.reusePid(pid, "Other Jan  2 00:00:00 2024", "someone-else")
	err := h.supervisor.Restart("api", nil)
	if err == nil || !strings.Contains(err.Error(), "cannot be stopped") {
		t.Fatalf("err %v", err)
	}
	if len(h.process.signals()) != 0 {
		t.Fatal("no signals expected")
	}
	if !h.process.isAlive(pid) {
		t.Fatal("pid should still be alive")
	}
	if apiPid(t, h) != pid {
		t.Fatal("nothing new was spawned")
	}
}

// `sh -c 'setup; exec server'`: the pid, group and start time stay, the command line becomes the
// server's, and the row turns `orphaned`. That process is still this service, so stop kills it
// and every group in its tree.
func TestStopKillsAnOrphanedServiceWhosePidAndStartTimeStillMatch(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	pid := apiPid(t, h)
	worker := h.process.forkIntoOwnGroup(pid)
	h.process.rewriteCommandLine(pid, "/usr/bin/server --port 8080")
	if err := h.supervisor.Status("api"); err != nil {
		t.Fatal(err)
	}
	if h.host.stateOf("api").ActualState != state.ActualOrphaned {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
	if err := h.supervisor.Stop("api", nil); err != nil {
		t.Fatal(err)
	}
	if h.process.isAlive(pid) {
		t.Fatal("the exec'd leader must be stopped")
	}
	if h.process.isAlive(worker) {
		t.Fatal("a child in its own group must be stopped with it")
	}
	saw := false
	for _, s := range h.process.signals() {
		if s.pid == pid && s.signal == SignalTerm {
			saw = true
		}
	}
	if !saw {
		t.Fatalf("signals %v", h.process.signals())
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualStopped || st.DesiredState != state.DesiredStopped {
		t.Fatalf("state %+v", st)
	}
	if st.Error != nil {
		t.Fatalf("error %v", *st.Error)
	}
}

// A verified orphan that ignores SIGTERM gets the same SIGKILL as an owned one.
func TestStopEscalatesToSigkillOnAVerifiedOrphan(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	pid := apiPid(t, h)
	h.process.mutateFingerprint(pid, "exec-target")
	h.process.markTermImmune(pid)
	if err := h.supervisor.Status("api"); err != nil {
		t.Fatal(err)
	}
	if err := h.supervisor.Stop("api", nil); err != nil {
		t.Fatal(err)
	}
	sawTerm, sawKill := false, false
	for _, s := range h.process.signals() {
		if s.pid == pid && s.signal == SignalTerm {
			sawTerm = true
		}
		if s.pid == pid && s.signal == SignalKill {
			sawKill = true
		}
	}
	if !sawTerm || !sawKill {
		t.Fatalf("signals %v", h.process.signals())
	}
	if h.process.isAlive(pid) {
		t.Fatal("pid should be dead")
	}
}

// Restart's stop phase replaces a verified orphan instead of failing on it.
func TestRestartReplacesAVerifiedOrphan(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	old := apiPid(t, h)
	h.process.mutateFingerprint(old, "exec-target")
	if err := h.supervisor.Status("api"); err != nil {
		t.Fatal(err)
	}
	if err := h.supervisor.Restart("api", nil); err != nil {
		t.Fatal(err)
	}
	if h.process.isAlive(old) {
		t.Fatal("old process should be dead")
	}
	newPid := apiPid(t, h)
	if newPid == old {
		t.Fatal("expected a fresh pid")
	}
	if !h.process.isAlive(newPid) {
		t.Fatal("new process should be alive")
	}
	if h.host.stateOf("api").ActualState != state.ActualReady {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
}

// `stop-services` shutdown stops a verified orphan too.
func TestShutdownStopsAVerifiedOrphan(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	pid := apiPid(t, h)
	h.process.mutateFingerprint(pid, "exec-target")
	if err := h.supervisor.Status("api"); err != nil {
		t.Fatal(err)
	}
	h.supervisor.Shutdown()
	if h.process.isAlive(pid) {
		t.Fatal("the orphan should be reaped")
	}
}

// ...and still leaves a reused pid alone.
func TestShutdownLeavesAReusedOrphanPidAlone(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	pid := apiPid(t, h)
	h.process.reusePid(pid, "Other Jan  2 00:00:00 2024", "someone-else")
	if err := h.supervisor.Status("api"); err != nil {
		t.Fatal(err)
	}
	h.supervisor.Shutdown()
	if !h.process.isAlive(pid) {
		t.Fatal("a reused pid must be left alone")
	}
	if len(h.process.signals()) != 0 {
		t.Fatal("no signals expected")
	}
}
