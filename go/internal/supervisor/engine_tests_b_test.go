// Ported from rust/crates/hearth-core/src/supervisor/engine/tests.rs (batch B).
package supervisor

import (
	"strings"
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

func TestANeverReadyServiceStaysRunningUnreadyAndKeepsProbing(t *testing.T) {
	h := buildHarnessWithTimeout(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})), 30)
	// tcp never reports ready — setTCPReady is never called for 8080.
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualRunningUnready || st.Readiness != state.ReadinessNotReady {
		t.Fatalf("state %+v", st)
	}
	if st.Error != nil && strings.Contains(*st.Error, "timed out") {
		t.Fatalf("unexpected timeout error: %v", *st.Error)
	}
	pid := st.Identity.PidValue()
	if !h.process.isAlive(pid) {
		t.Fatal("process should be alive")
	}
	if len(h.process.signals()) != 0 {
		t.Fatal("a failing probe must not signal the process")
	}
	// The loop is still the start's token: flipping the probe reaches ready without a restart.
	h.probes.setTCPReady(8080, true)
	waitUntilActual(t, h.host, "api", state.ActualReady)
	if !h.process.isAlive(pid) {
		t.Fatal("process should still be alive")
	}
}

func TestStopCancelsTheContinuousProbeLoop(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	if h.host.stateOf("api").ActualState != state.ActualRunningUnready {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
	if err := h.supervisor.Stop("api", nil); err != nil {
		t.Fatal(err)
	}
	h.probes.setTCPReady(8080, true)
	for range 40 {
		gosched()
	}
	if h.host.stateOf("api").ActualState != state.ActualStopped {
		t.Fatalf("a stopped service must not become ready from a leftover probe: %s", h.host.stateOf("api").ActualState)
	}
}

func TestTCPPortConflictRefusesToStart(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setPortInUse(8080, true)
	err := h.supervisor.Start("api", nil)
	if err == nil || !strings.Contains(err.Error(), "externally owned") {
		t.Fatalf("err %v", err)
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualExternallyOwned {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Error == nil || !strings.Contains(*st.Error, "Port 8080 is held") {
		t.Fatalf("error %v", st.Error)
	}
	if len(h.process.signals()) != 0 {
		t.Fatal("must never have spawned anything")
	}
}

// The refusal error must name the squatter — `pid (command)`.
func TestTCPPortConflictNamesTheHolderInTheError(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setPortHolder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main")
	err := h.supervisor.Start("api", nil)
	if err == nil || !strings.Contains(err.Error(), "externally owned") {
		t.Fatalf("err %v", err)
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualExternallyOwned {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Error == nil || *st.Error != "Port 8080 is held by pid 41234 (node dist/main)" {
		t.Fatalf("error %v", st.Error)
	}
	if len(h.process.pidSignals()) != 0 {
		t.Fatal("without kill_unowned nothing may be signalled")
	}
}

// `killUnowned` is the client's echo of the user's "yes".
func TestStartWithKillUnownedTerminatesTheHolderAndContinues(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setPortHolder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main")
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.StartWithOptions("api", nil, StartOptions{KillUnowned: true}); err != nil {
		t.Fatal(err)
	}
	sigs := h.process.pidSignals()
	if len(sigs) != 1 || sigs[0].pid != 41_234 || sigs[0].signal != SignalTerm {
		t.Fatalf("pid signals %v", sigs)
	}
	if h.process.isAlive(41_234) {
		t.Fatal("the squatter must be dead")
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualReady {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Identity == nil {
		t.Fatal("the service itself must have spawned after the port freed")
	}
}

// A squatter that ignores SIGTERM is escalated to SIGKILL on a second pass.
func TestReclaimEscalatesToSigkillForASigtermImmuneHolder(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setPortHolder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main")
	h.probes.setTCPReady(8080, true)
	h.process.markTermImmune(41_234)
	if err := h.supervisor.StartWithOptions("api", nil, StartOptions{KillUnowned: true}); err != nil {
		t.Fatal(err)
	}
	sigs := h.process.pidSignals()
	if len(sigs) != 2 || sigs[0].pid != 41_234 || sigs[0].signal != SignalTerm || sigs[1].signal != SignalKill {
		t.Fatalf("pid signals %v", sigs)
	}
	if h.host.stateOf("api").ActualState != state.ActualReady {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
}

// If the holder still owns the port after SIGKILL the start must fail loudly.
func TestReclaimFailsLoudlyWhenTheHolderSurvivesSigkill(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setPortHolder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main")
	h.process.markUnkillable(41_234)
	err := h.supervisor.StartWithOptions("api", nil, StartOptions{KillUnowned: true})
	if err == nil || !strings.Contains(err.Error(), "still held") {
		t.Fatalf("err %v", err)
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualExternallyOwned {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Identity != nil {
		t.Fatal("the service must never have spawned")
	}
}

// A pid that no longer presents the resolved lstart is not ours to kill.
func TestReclaimNeverKillsAHolderWhoseStartIdentityNoLongerMatches(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setPortHolder(8080, 41_234, "original lstart", "node dist/main")
	// The squatter died and its pid was recycled by something else before the signal landed.
	h.process.reusePid(41_234, "recycled lstart", "node dist/main")
	err := h.supervisor.StartWithOptions("api", nil, StartOptions{KillUnowned: true})
	if err == nil || !strings.Contains(err.Error(), "still held") {
		t.Fatalf("err %v", err)
	}
	for _, s := range h.process.pidSignals() {
		if s.pid != 41_234 {
			t.Fatalf("unexpected pid signal %v", s)
		}
	}
	if !h.process.isAlive(41_234) {
		t.Fatal("a recycled pid must survive reclaim")
	}
}

func TestReclaimTerminatesEveryHolderOfThePort(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setPortHolder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node a")
	h.probes.setPortHolder(8080, 41_235, "Fake Jan  1 00:00:02 2024", "node b")
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.StartWithOptions("api", nil, StartOptions{KillUnowned: true}); err != nil {
		t.Fatal(err)
	}
	sigs := h.process.pidSignals()
	if len(sigs) != 2 || sigs[0].pid != 41_234 || sigs[1].pid != 41_235 {
		t.Fatalf("pid signals %v", sigs)
	}
	if h.host.stateOf("api").ActualState != state.ActualReady {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
}

// `port_holders` returning `None` means the adapter cannot resolve who holds the port.
func TestReclaimFailsClosedWhenTheProbeCannotResolveHolders(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setPortInUse(8080, true)
	h.probes.setPortHoldersUnsupported()
	err := h.supervisor.StartWithOptions("api", nil, StartOptions{KillUnowned: true})
	if err == nil || !strings.Contains(err.Error(), "still held by an unowned process") {
		t.Fatalf("err %v", err)
	}
	if len(h.process.pidSignals()) != 0 {
		t.Fatal("an unresolvable holder must never be signalled")
	}
	if h.host.stateOf("api").ActualState != state.ActualExternallyOwned {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
}

// Restart always uses default options — it must never signal a squatter.
func TestRestartNeverReclaimsAnOccupiedPort(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setPortHolder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main")
	err := h.supervisor.Restart("api", nil)
	if err == nil || !strings.Contains(err.Error(), "externally owned") {
		t.Fatalf("err %v", err)
	}
	if len(h.process.pidSignals()) != 0 {
		t.Fatal("no pid signals expected")
	}
	if h.host.stateOf("api").ActualState != state.ActualExternallyOwned {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
}

func TestShutdownReapsAPersistedIdentityInANonActiveState(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	cmd := argvCommandForTest("api")
	fingerprint := NormalizeCommandFingerprint(&cmd)
	pid := int64(900_001)
	pgid := int64(900_001)
	start := "Fake Jan  1 00:00:01 2024"
	identity := &state.ProcessIdentity{
		ManagerInstanceID: h.host.InstanceID(), ServiceID: "api", Generation: 1,
		StartedAt: "2024-01-01T00:00:00.000Z", CommandFingerprint: fingerprint,
		Pid: &pid, Pgid: &pgid, StartIdentity: &start,
	}
	h.process.insertAlive(900_001, fingerprint, start)
	errText := "Port 8080 is held by an unowned process"
	h.host.seed(state.ServiceLifecycleState{
		ServiceID:    "api",
		DesiredState: state.DesiredStopped,
		ActualState:  state.ActualExternallyOwned,
		Readiness:    state.ReadinessFailed,
		Generation:   1,
		Identity:     identity,
		CreatedAt:    "2024-01-01T00:00:00.000Z",
		UpdatedAt:    "2024-01-01T00:00:00.000Z",
		Error:        &errText,
	})
	h.supervisor.Shutdown()
	saw := false
	for _, s := range h.process.signals() {
		if s.pid == 900_001 && s.signal == SignalTerm {
			saw = true
		}
	}
	if !saw {
		t.Fatalf("shutdown's second pass must reap a persisted identity even in a non-active state: %v", h.process.signals())
	}
}
