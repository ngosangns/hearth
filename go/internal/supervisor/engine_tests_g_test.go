// Ported from rust/crates/hearth-core/src/supervisor/engine/tests.rs (batch G).
package supervisor

import (
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

func startAsync(sup *ProcessSupervisor, id string) <-chan error {
	ch := make(chan error, 1)
	go func() { ch <- sup.Start(id, nil) }()
	return ch
}

func TestAnExitCommandThatReturnsZeroIsSucceededAndNotRestarted(t *testing.T) {
	h := buildHarnessWithTimeout(oneServiceCatalog(argvVerified("fe", catalog.ReadinessSpec{Kind: "exit"})), 30)
	started := h.clock.NowMillis()
	run := startAsync(h.supervisor, "fe")
	pid := waitForPosixPid(t, h.host, "fe")
	if h.host.stateOf("fe").ActualState != state.ActualRunning {
		t.Fatalf("state %s", h.host.stateOf("fe").ActualState)
	}
	for h.clock.NowMillis() < started+40 {
		gosched()
	}
	h.process.killExternally(pid, 0)
	if err := <-run; err != nil {
		t.Fatal(err)
	}
	st := h.host.stateOf("fe")
	if st.ActualState != state.ActualSucceeded || st.DesiredState != state.DesiredStopped {
		t.Fatalf("state %+v", st)
	}
	if st.ExitCode == nil || *st.ExitCode != 0 {
		t.Fatalf("exit code %v", st.ExitCode)
	}
	if st.Identity != nil {
		t.Fatal("identity should be cleared")
	}
	if st.Error != nil {
		t.Fatalf("error %v", *st.Error)
	}
	h.supervisor.Reconcile()
	after := h.host.stateOf("fe")
	if after.ActualState != state.ActualSucceeded {
		t.Fatalf("state %s", after.ActualState)
	}
	if h.process.isAlive(pid) {
		t.Fatal("pid should be dead")
	}
}

func TestAnExitCommandThatReturnsNonzeroIsFailed(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("fe", catalog.ReadinessSpec{Kind: "exit"})))
	run := startAsync(h.supervisor, "fe")
	pid := waitForPosixPid(t, h.host, "fe")
	h.process.killExternally(pid, 1)
	err := <-run
	if err == nil || !contains(err.Error(), "code 1") {
		t.Fatalf("err %v", err)
	}
	st := h.host.stateOf("fe")
	if st.ActualState != state.ActualFailed || st.DesiredState != state.DesiredStopped {
		t.Fatalf("state %+v", st)
	}
	if st.ExitCode == nil || *st.ExitCode != 1 {
		t.Fatalf("exit code %v", st.ExitCode)
	}
	if st.Identity != nil {
		t.Fatal("identity should be cleared")
	}
}

func TestStoppingAnExitCommandMidRunRecordsStopped(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("fe", catalog.ReadinessSpec{Kind: "exit"})))
	run := startAsync(h.supervisor, "fe")
	waitForPosixPid(t, h.host, "fe")
	if err := h.supervisor.Stop("fe", nil); err != nil {
		t.Fatal(err)
	}
	<-run
	st := h.host.stateOf("fe")
	if st.ActualState != state.ActualStopped {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.ActualState == state.ActualSucceeded || st.ActualState == state.ActualFailed {
		t.Fatalf("state %s", st.ActualState)
	}
}

func TestAnExitCommandStaysRunningPastAConfiguredDeadline(t *testing.T) {
	h := buildHarness(oneServiceCatalog(withExitTimeout(argvVerified("fe", catalog.ReadinessSpec{Kind: "exit"}), 40)))
	started := h.clock.NowMillis()
	run := startAsync(h.supervisor, "fe")
	pid := waitForPosixPid(t, h.host, "fe")
	running := h.host.stateOf("fe")
	if running.ActualState != state.ActualRunning {
		t.Fatalf("state %s", running.ActualState)
	}
	if running.ActualState == state.ActualReady || running.Readiness == state.ReadinessReady {
		t.Fatalf("state %+v", running)
	}
	for h.clock.NowMillis() < started+80 {
		gosched()
	}
	still := h.host.stateOf("fe")
	if still.ActualState != state.ActualRunning {
		t.Fatalf("state %s", still.ActualState)
	}
	if still.ActualState == state.ActualFailed {
		t.Fatal("must not fail past the deadline")
	}
	h.process.killExternally(pid, 0)
	if err := <-run; err != nil {
		t.Fatal(err)
	}
	st := h.host.stateOf("fe")
	if st.ActualState != state.ActualSucceeded {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.ActualState == state.ActualReady || st.Readiness == state.ReadinessReady {
		t.Fatalf("state %+v", st)
	}
	if st.ExitCode == nil || *st.ExitCode != 0 {
		t.Fatalf("exit code %v", st.ExitCode)
	}
}

func TestStopOnASucceededExitCommandKeepsSucceeded(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("fe", catalog.ReadinessSpec{Kind: "exit"})))
	run := startAsync(h.supervisor, "fe")
	pid := waitForPosixPid(t, h.host, "fe")
	h.process.killExternally(pid, 0)
	if err := <-run; err != nil {
		t.Fatal(err)
	}
	if err := h.supervisor.Stop("fe", nil); err != nil {
		t.Fatal(err)
	}
	st := h.host.stateOf("fe")
	if st.ActualState != state.ActualSucceeded {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.ExitCode == nil || *st.ExitCode != 0 {
		t.Fatalf("exit code %v", st.ExitCode)
	}
}

func TestAnAdoptedExitCommandThatDisappearsIsNotSucceeded(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("fe", catalog.ReadinessSpec{Kind: "exit"})))
	startIdentity := "Fake Jan  1 00:00:00 2024"
	h.process.insertAlive(42, "fp", startIdentity)
	pid := int64(42)
	pgid := int64(42)
	kind := state.ReadinessKindExit
	h.host.seed(state.ServiceLifecycleState{
		ServiceID:     "fe",
		DesiredState:  state.DesiredRunning,
		ActualState:   state.ActualRunningUnready,
		Readiness:     state.ReadinessNotReady,
		Generation:    1,
		ReadinessKind: &kind,
		CreatedAt:     "2024-01-01T00:00:00.000Z",
		UpdatedAt:     "2024-01-01T00:00:00.000Z",
		Identity: &state.ProcessIdentity{
			ManagerInstanceID: "instance-1", ServiceID: "fe", Generation: 1,
			StartedAt: "2024-01-01T00:00:00.000Z", CommandFingerprint: "fp",
			Pid: &pid, Pgid: &pgid, StartIdentity: &startIdentity,
		},
	})
	done := make(chan struct{})
	go func() { h.supervisor.Reconcile(); close(done) }()
	for range 20 {
		gosched()
	}
	h.process.killExternally(42, 0)
	<-done
	st := h.host.stateOf("fe")
	if st.ActualState != state.ActualFailed {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.ActualState == state.ActualSucceeded {
		t.Fatal("an adopted exit command that disappears must not be succeeded")
	}
	errText := ""
	if st.Error != nil {
		errText = *st.Error
	}
	if !contains(errText, "exit code unknown") && !contains(errText, "no longer alive") && !contains(errText, "exited with code") {
		t.Fatalf("error %q", errText)
	}
}
