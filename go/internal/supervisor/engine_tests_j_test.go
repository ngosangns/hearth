// Ported from rust/crates/hearth-core/src/supervisor/engine/tests.rs (batch J).
package supervisor

import (
	"testing"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

func TestALeaderExitReapsAChildItLeftInItsOwnGroup(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	leader := posixPid(t, h, "api")
	child := h.process.forkInGroup(leader)
	time.Sleep(350 * time.Millisecond)
	h.process.killExternally(leader, 1)
	waitDead(t, h.process, child)
	if h.process.isAlive(child) {
		t.Fatal("a child left in the dead leader's group must be reaped")
	}
	saw := false
	for _, s := range h.process.signals() {
		if s.pid == leader && s.signal == SignalTerm {
			saw = true
		}
	}
	if !saw {
		t.Fatalf("the group is signalled while a member of it is alive: %v", h.process.signals())
	}
	waitUntilActual(t, h.host, "api", state.ActualFailed)
}

// `(trap ” TERM; cmd &)`: `cmd` is reparented to launchd but stays in the service's group, and
// it ignores SIGTERM.
func TestStopSigkillsADoubleForkedGroupMemberThatIgnoresSigterm(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	leader := posixPid(t, h, "api")
	child := h.process.forkInGroup(leader)
	h.process.reparentToLaunchd(child)
	h.process.markTermImmune(child)
	if err := h.supervisor.Stop("api", nil); err != nil {
		t.Fatal(err)
	}
	if h.process.isAlive(leader) {
		t.Fatal("leader should be dead")
	}
	if h.process.isAlive(child) {
		t.Fatalf("the double-forked member must get SIGKILL: %v", h.process.signals())
	}
	saw := false
	for _, s := range h.process.signals() {
		if s.pid == leader && s.signal == SignalKill {
			saw = true
		}
	}
	if !saw {
		t.Fatalf("expected SIGKILL to the leader group: %v", h.process.signals())
	}
}

// A descendant that moved into its own group and was then orphaned (`setsid` plus a double fork)
// has no ppid path and no group in common with the leader. The sampler saw it while it was still
// a child, so the stop signals it too.
func TestStopSignalsAnEscapedDescendantTheSamplerSaw(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	leader := posixPid(t, h, "api")
	escaped := h.process.forkIntoOwnGroup(leader)
	time.Sleep(350 * time.Millisecond)
	h.process.reparentToLaunchd(escaped)
	// A few more samples after it left the tree: it must stay remembered while alive.
	time.Sleep(450 * time.Millisecond)
	if err := h.supervisor.Stop("api", nil); err != nil {
		t.Fatal(err)
	}
	if h.process.isAlive(escaped) {
		t.Fatalf("the escaped descendant must be stopped with its service: %v", h.process.signals())
	}
	saw := false
	for _, s := range h.process.signals() {
		if s.pid == escaped && s.signal == SignalTerm {
			saw = true
		}
	}
	if !saw {
		t.Fatalf("expected SIGTERM to the escaped group: %v", h.process.signals())
	}
}

// A leave-services shutdown keeps the services but must stop the followers it spawned for them.
func TestDetachAllOutputStopsEveryLogFollower(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	if h.process.tailWasStopped("api") {
		t.Fatal("tail should not be stopped yet")
	}
	h.supervisor.DetachAllOutput()
	if !h.process.tailWasStopped("api") {
		t.Fatal("tail should be stopped")
	}
	if !h.process.isAlive(posixPid(t, h, "api")) {
		t.Fatal("the service itself keeps running")
	}
}

func TestLogReadinessLatchesReadyOnceThePatternAppears(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "log", Pattern: "listening on"})))
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	waitUntilActual(t, h.host, "api", state.ActualRunningUnready)
	h.process.emitOutput("api", "booting…\n")
	gosched()
	if h.host.stateOf("api").ActualState != state.ActualRunningUnready {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
	h.process.emitOutput("api", "listening on :8080\n")
	waitUntilActual(t, h.host, "api", state.ActualReady)
}

// The ready marker can be split across two writes — the matcher sees the joined stream.
func TestLogReadinessMatchesAPatternSplitAcrossWrites(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "log", Pattern: "ready to accept"})))
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	waitUntilActual(t, h.host, "api", state.ActualRunningUnready)
	h.process.emitOutput("api", "ready to ")
	h.process.emitOutput("api", "accept connections\n")
	waitUntilActual(t, h.host, "api", state.ActualReady)
}

// A fresh spawn clears the previous process's latch — the new process must print the marker
// itself before it is ready.
func TestARespawnMustEmitTheLogPatternAgain(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "log", Pattern: "ready"})))
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	h.process.emitOutput("api", "ready\n")
	waitUntilActual(t, h.host, "api", state.ActualReady)
	if err := h.supervisor.Restart("api", nil); err != nil {
		t.Fatal(err)
	}
	waitUntilActual(t, h.host, "api", state.ActualRunningUnready)
	if h.host.stateOf("api").ActualState != state.ActualRunningUnready {
		t.Fatal("a cleared latch must not satisfy the new process's readiness")
	}
	h.process.emitOutput("api", "ready\n")
	waitUntilActual(t, h.host, "api", state.ActualReady)
}
