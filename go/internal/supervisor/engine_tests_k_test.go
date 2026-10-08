// Ported from rust/crates/hearth-core/src/supervisor/engine/tests.rs (batch K).
package supervisor

import (
	"testing"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

func TestRestartOnFailureRespawnsAfterAnUnexpectedNonzeroExit(t *testing.T) {
	h := buildHarness(oneServiceCatalog(withRestart(
		argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)}),
		"on-failure", nil, new(uint64(5)),
	)))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	waitUntilActual(t, h.host, "api", state.ActualReady)
	if h.process.spawnCount("api") != 1 {
		t.Fatalf("spawn count %d", h.process.spawnCount("api"))
	}
	h.process.killExternally(posixPid(t, h, "api"), 1)
	waitUntilActual(t, h.host, "api", state.ActualFailed)
	waitUntilActual(t, h.host, "api", state.ActualReady)
	if h.process.spawnCount("api") != 2 {
		t.Fatalf("the crash must earn exactly one respawn, got %d", h.process.spawnCount("api"))
	}
}

// `on-failure` only pays for a crash — an exit 0 while desired `running` is still unexpected but
// not a failure the policy covers.
func TestRestartOnFailureDoesNotRespawnACleanExit(t *testing.T) {
	h := buildHarness(oneServiceCatalog(withRestart(
		argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)}),
		"on-failure", nil, new(uint64(5)),
	)))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	waitUntilActual(t, h.host, "api", state.ActualReady)
	h.process.killExternally(posixPid(t, h, "api"), 0)
	waitUntilActual(t, h.host, "api", state.ActualFailed)
	time.Sleep(50 * time.Millisecond)
	if h.process.spawnCount("api") != 1 {
		t.Fatalf("spawn count %d", h.process.spawnCount("api"))
	}
}

func TestRestartMaxRestartsBoundsConsecutiveRespawns(t *testing.T) {
	h := buildHarness(oneServiceCatalog(withRestart(
		argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)}),
		"on-failure", new(uint32(1)), new(uint64(5)),
	)))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	waitUntilActual(t, h.host, "api", state.ActualReady)
	h.process.killExternally(posixPid(t, h, "api"), 1)
	waitUntilActual(t, h.host, "api", state.ActualFailed)
	waitUntilActual(t, h.host, "api", state.ActualReady)
	if h.process.spawnCount("api") != 2 {
		t.Fatalf("spawn count %d", h.process.spawnCount("api"))
	}
	// The service reached ready, so the budget reset — this second crash still earns a respawn.
	h.process.killExternally(posixPid(t, h, "api"), 1)
	waitUntilActual(t, h.host, "api", state.ActualFailed)
	waitUntilActual(t, h.host, "api", state.ActualReady)
	if h.process.spawnCount("api") != 3 {
		t.Fatalf("spawn count %d", h.process.spawnCount("api"))
	}
}

// `maxRestarts` bounds crashes that never reach ready: the second consecutive crash stays down.
func TestConsecutiveCrashesExhaustTheRestartBudget(t *testing.T) {
	h := buildHarness(oneServiceCatalog(withRestart(
		argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)}),
		"on-failure", new(uint32(1)), new(uint64(5)),
	)))
	// Never ready — tcp stays false so every spawn dies "unready" and attempts keep accumulating.
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	waitUntilActual(t, h.host, "api", state.ActualRunningUnready)
	h.process.killExternally(posixPid(t, h, "api"), 1)
	waitUntilActual(t, h.host, "api", state.ActualFailed)
	waitUntilActual(t, h.host, "api", state.ActualRunningUnready)
	if h.process.spawnCount("api") != 2 {
		t.Fatalf("spawn count %d", h.process.spawnCount("api"))
	}
	h.process.killExternally(posixPid(t, h, "api"), 1)
	waitUntilActual(t, h.host, "api", state.ActualFailed)
	time.Sleep(50 * time.Millisecond)
	if h.process.spawnCount("api") != 2 {
		t.Fatalf("a third spawn must not happen past maxRestarts, got %d", h.process.spawnCount("api"))
	}
	if h.host.stateOf("api").ActualState != state.ActualFailed {
		t.Fatalf("state %s", h.host.stateOf("api").ActualState)
	}
}

// A manual `restart` during the respawn delay makes the pending auto-respawn a no-op — the
// service must never get two live copies.
func TestAManualRestartDuringTheDelayPreventsADoubleSpawn(t *testing.T) {
	h := buildHarness(oneServiceCatalog(withRestart(
		argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)}),
		"on-failure", nil, new(uint64(300)),
	)))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	waitUntilActual(t, h.host, "api", state.ActualReady)
	h.process.killExternally(posixPid(t, h, "api"), 1)
	waitUntilActual(t, h.host, "api", state.ActualFailed)
	if err := h.supervisor.Restart("api", nil); err != nil {
		t.Fatal(err)
	}
	waitUntilActual(t, h.host, "api", state.ActualReady)
	// The 300ms auto-respawn window lapses with the service already back — it must not fire.
	time.Sleep(400 * time.Millisecond)
	if h.process.spawnCount("api") != 2 {
		t.Fatalf("the queued auto-respawn must see the service alive and skip, got %d", h.process.spawnCount("api"))
	}
	if !h.process.isAlive(posixPid(t, h, "api")) {
		t.Fatal("service should be alive")
	}
}
