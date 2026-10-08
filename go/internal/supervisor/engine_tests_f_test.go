// Ported from rust/crates/hearth-core/src/supervisor/engine/tests.rs (batch F).
package supervisor

import (
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

func TestARewrittenProcessTitleStaysTheSameService(t *testing.T) {
	h := buildHarness(oneServiceCatalog(redisService()))
	h.probes.setTCPReady(6380, true)
	if err := h.supervisor.Start("redis", nil); err != nil {
		t.Fatal(err)
	}
	pid := posixPid(t, h, "redis")
	h.process.rewriteCommandLine(pid, "/opt/redis/bin/redis-server 127.0.0.1:43886")
	h.supervisor.Reconcile()
	st := h.host.stateOf("redis")
	if st.ActualState == state.ActualOrphaned {
		t.Fatal("a title rewrite must not orphan the service")
	}
	if st.Error != nil && contains(*st.Error, "no longer matches") {
		t.Fatalf("error %v", *st.Error)
	}
}

func TestADifferentExecutableOnTheSamePidIsNotOwned(t *testing.T) {
	h := buildHarness(oneServiceCatalog(redisService()))
	h.probes.setTCPReady(6380, true)
	if err := h.supervisor.Start("redis", nil); err != nil {
		t.Fatal(err)
	}
	pid := posixPid(t, h, "redis")
	h.process.rewriteCommandLine(pid, "/other/bin/nginx 127.0.0.1:80")
	h.supervisor.Reconcile()
	if h.host.stateOf("redis").ActualState != state.ActualOrphaned {
		t.Fatalf("state %s", h.host.stateOf("redis").ActualState)
	}
}

func TestStartAdoptsATitleRewrittenCopyInsteadOfSpawningAnother(t *testing.T) {
	h := buildHarness(oneServiceCatalog(redisService()))
	h.probes.setTCPReady(6380, true)
	h.process.insertAlive(4242, "stale", "Fake Jan  1 00:00:42 2024")
	h.process.rewriteCommandLine(4242, "/opt/redis/bin/redis-server 127.0.0.1:43886")
	if err := h.supervisor.Start("redis", nil); err != nil {
		t.Fatal(err)
	}
	st := h.host.stateOf("redis")
	if st.ActualState != state.ActualReady {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Identity == nil || st.Identity.IsDocker() || st.Identity.PidValue() != 4242 {
		t.Fatalf("expected the adopted pid 4242, got %+v", st.Identity)
	}
	h.process.mu.Lock()
	alive := len(h.process.state.alive)
	h.process.mu.Unlock()
	if alive != 1 || !h.process.isAlive(4242) {
		t.Fatalf("a second redis-server must not be spawned beside the one already bound (alive=%d)", alive)
	}
}

func TestRestartReapsATitleRewrittenDuplicateOfTheSameExecutable(t *testing.T) {
	h := buildHarness(oneServiceCatalog(redisService()))
	h.probes.setTCPReady(6380, true)
	if err := h.supervisor.Start("redis", nil); err != nil {
		t.Fatal(err)
	}
	h.process.insertAlive(4242, "stale", "Fake Jan  1 00:00:42 2024")
	h.process.rewriteCommandLine(4242, "/opt/redis/bin/redis-server 127.0.0.1:43886")
	if err := h.supervisor.Restart("redis", nil); err != nil {
		t.Fatal(err)
	}
	saw := false
	for _, s := range h.process.pidSignals() {
		if s.pid == 4242 && s.signal == SignalTerm {
			saw = true
		}
	}
	if !saw {
		t.Fatalf("rewritten redis must be signalled: %v", h.process.pidSignals())
	}
}

func TestASharedInterpreterIsNotADuplicateOfEveryService(t *testing.T) {
	server := withArgv(argvVerified("server", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)}), []string{"/usr/bin/node", "server.js"})
	worker := withArgv(argvVerified("worker", catalog.ReadinessSpec{Kind: "tcp", Port: port(8081)}), []string{"/usr/bin/node", "worker.js"})
	cat := oneServiceCatalog(server)
	cat.Services = append(cat.Services, worker)
	h := buildHarness(cat)
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("server", nil); err != nil {
		t.Fatal(err)
	}
	h.process.insertAlive(4242, "stale", "Fake Jan  1 00:00:42 2024")
	h.process.rewriteCommandLine(4242, "/usr/bin/node worker.js")
	if err := h.supervisor.Restart("server", nil); err != nil {
		t.Fatal(err)
	}
	if !h.process.isAlive(4242) {
		t.Fatal("another node script in the same directory must survive")
	}
	for _, s := range h.process.pidSignals() {
		if s.pid == 4242 {
			t.Fatalf("unexpected signal to 4242: %v", h.process.pidSignals())
		}
	}
}

func TestAOneShotThatExitsNonzeroFailsWithoutWaitingOutReadiness(t *testing.T) {
	no := false
	host, process, sup := oneShotSupervisor(externalCommandService("redis"), &no)
	process.setExitOnSpawn(1)
	err := sup.Start("redis", nil)
	if err == nil || !contains(err.Error(), "exited with code 1") {
		t.Fatalf("err %v", err)
	}
	if contains(err.Error(), "Readiness timed out") {
		t.Fatalf("err %v", err)
	}
	if host.stateOf("redis").ActualState != state.ActualFailed {
		t.Fatalf("state %s", host.stateOf("redis").ActualState)
	}
}

func TestAOneShotThatExitsZeroDoesNotKeepThePackBudget(t *testing.T) {
	no := false
	_, process, sup := oneShotSupervisor(externalCommandService("redis"), &no)
	process.setExitOnSpawn(0)
	err := sup.Start("redis", nil)
	if err == nil || !contains(err.Error(), "not ready") {
		t.Fatalf("err %v", err)
	}
	if contains(err.Error(), "Readiness timed out") {
		t.Fatalf("err %v", err)
	}
}

func TestAOneShotThatExitsZeroIsReadyWhenTheProbePasses(t *testing.T) {
	yes := true
	host, process, sup := oneShotSupervisor(externalCommandService("redis"), &yes)
	process.setExitOnSpawn(0)
	if err := sup.Start("redis", nil); err != nil {
		t.Fatal(err)
	}
	if host.stateOf("redis").ActualState != state.ActualReady {
		t.Fatalf("state %s", host.stateOf("redis").ActualState)
	}
}

func TestAnInterpreterIsNotTreatedAsThisService(t *testing.T) {
	server := withArgv(argvVerified("server", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)}), []string{"/usr/bin/node", "server.js"})
	h := buildHarness(oneServiceCatalog(server))
	h.probes.setTCPReady(8080, true)
	h.process.insertAlive(4242, "stale", "Fake Jan  1 00:00:42 2024")
	h.process.rewriteCommandLine(4242, "/usr/bin/node worker.js")
	if err := h.supervisor.Start("server", nil); err != nil {
		t.Fatal(err)
	}
	pid := posixPid(t, h, "server")
	if pid == 4242 {
		t.Fatal("a node process must not be adopted")
	}
	if !h.process.isAlive(4242) {
		t.Fatal("4242 should be alive")
	}
	if err := h.supervisor.Restart("server", nil); err != nil {
		t.Fatal(err)
	}
	if !h.process.isAlive(4242) {
		t.Fatal("another node script must survive a restart of the only node service")
	}
	for _, s := range h.process.pidSignals() {
		if s.pid == 4242 {
			t.Fatalf("unexpected signal to 4242: %v", h.process.pidSignals())
		}
	}
}

func TestASharedServerBinaryIsNotADuplicateOfAnotherService(t *testing.T) {
	cache := withArgv(argvVerified("cache", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)}), []string{"/opt/cache/bin/server", "cache.conf"})
	queue := withArgv(argvVerified("queue", catalog.ReadinessSpec{Kind: "tcp", Port: port(8081)}), []string{"/opt/cache/bin/server", "queue.conf"})
	cat := oneServiceCatalog(cache)
	cat.Services = append(cat.Services, queue)
	h := buildHarness(cat)
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("cache", nil); err != nil {
		t.Fatal(err)
	}
	h.process.insertAlive(4242, "stale", "Fake Jan  1 00:00:42 2024")
	h.process.rewriteCommandLine(4242, "/opt/cache/bin/server 127.0.0.1:9")
	if err := h.supervisor.Restart("cache", nil); err != nil {
		t.Fatal(err)
	}
	if !h.process.isAlive(4242) {
		t.Fatal("the other service's copy of the same binary must survive")
	}
	for _, s := range h.process.pidSignals() {
		if s.pid == 4242 {
			t.Fatalf("unexpected signal to 4242: %v", h.process.pidSignals())
		}
	}
}
