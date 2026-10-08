// Ported from rust/crates/hearth-core/src/supervisor/engine/tests.rs (batch I).
package supervisor

import (
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

// Group restart of viclass `core` / `editors` used to stop here: the rows are `externally-owned`,
// identity-less, and the catalog has no `stop` command. Restart must start the service when the
// port is free.
func TestRestartStartsAnExternallyOwnedServiceWhenNothingIsListening(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("metadata", catalog.ReadinessSpec{Kind: "tcp", Port: port(1166)})))
	h.probes.setTCPReady(1166, true)
	seedExternallyOwned(h.host, "metadata", state.DesiredRunning, "Port 1166 is held by pid 49268 (/usr/bin/java)")
	if err := h.supervisor.Restart("metadata", nil); err != nil {
		t.Fatal(err)
	}
	st := h.host.stateOf("metadata")
	if st.ActualState != state.ActualReady {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Identity == nil {
		t.Fatal("expected an identity")
	}
	if st.Error != nil {
		t.Fatalf("error %v", *st.Error)
	}
	if len(h.process.signals()) != 0 || len(h.process.pidSignals()) != 0 {
		t.Fatal("a free port has nothing to signal")
	}
	if h.process.nextPidValue() == 900_001 {
		t.Fatal("restart must spawn")
	}
}

func TestRestartStartsAFailedServiceThatHasNoProcess(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("metadata", catalog.ReadinessSpec{Kind: "tcp", Port: port(1166)})))
	h.probes.setTCPReady(1166, true)
	kind := state.ReadinessKindTCP
	errText := "Managed process is no longer alive"
	h.host.seed(state.ServiceLifecycleState{
		ServiceID:     "metadata",
		DesiredState:  state.DesiredRunning,
		ActualState:   state.ActualFailed,
		Readiness:     state.ReadinessFailed,
		Generation:    2,
		ReadinessKind: &kind,
		CreatedAt:     "2024-01-01T00:00:00.000Z",
		UpdatedAt:     "2024-01-01T00:00:00.000Z",
		Error:         &errText,
	})
	if err := h.supervisor.Restart("metadata", nil); err != nil {
		t.Fatal(err)
	}
	if h.host.stateOf("metadata").ActualState != state.ActualReady {
		t.Fatalf("state %s", h.host.stateOf("metadata").ActualState)
	}
}

// The listener is this service. Restart has to replace it, through the owned-process stop,
// before the new process can bind.
func TestRestartReplacesTheServiceHoldingItsOwnPort(t *testing.T) {
	cwd := "viclass/packages/backend/viclass/eb.word"
	h := buildHarness(oneServiceCatalog(shellService("word", cwd, "exec node dist/main", 8012)))
	h.probes.setPortHolder(8012, 4242, "Mon Oct  5 11:20:13 2026", "node dist/main")
	h.process.setCwd(4242, cwd)
	h.probes.setTCPReady(8012, true)
	seedExternallyOwned(h.host, "word", state.DesiredRunning, "Port 8012 is held by pid 4242 (node dist/main)")
	if err := h.supervisor.Restart("word", nil); err != nil {
		t.Fatal(err)
	}
	if h.process.isAlive(4242) {
		t.Fatal("the previous listener must be gone")
	}
	saw := false
	for _, s := range h.process.signals() {
		if s.pid == 4242 && s.signal == SignalTerm {
			saw = true
		}
	}
	if !saw {
		t.Fatalf("signals %v", h.process.signals())
	}
	if len(h.process.pidSignals()) != 0 {
		t.Fatal("replacing our own listener is not killUnowned")
	}
	st := h.host.stateOf("word")
	if st.ActualState != state.ActualReady {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Identity == nil || st.Identity.IsDocker() || st.Identity.PidValue() == 4242 {
		t.Fatalf("expected a new process, got %+v", st.Identity)
	}
}

func TestRestartReplacesAJavaProcessIdentifiedByItsInstallDirectory(t *testing.T) {
	h := buildHarness(oneServiceCatalog(shellService("metadata", ".", "cd -- 'portal/metadata' && exec 'build/install/portal.metadata/bin/portal.metadata'", 1166)))
	command := "/usr/bin/java -classpath /Users/ngosangns/Github/ryan/viclass/portal/metadata/build/install/portal.metadata/lib/app.jar"
	h.probes.setPortHolder(1166, 49268, "Mon Oct  5 11:26:11 2026", command)
	h.probes.setTCPReady(1166, true)
	seedExternallyOwned(h.host, "metadata", state.DesiredRunning, "Port 1166 is held by pid 49268 (/usr/bin/java)")
	if err := h.supervisor.Restart("metadata", nil); err != nil {
		t.Fatal(err)
	}
	if h.process.isAlive(49268) {
		t.Fatal("the java process must be gone")
	}
	saw := false
	for _, s := range h.process.signals() {
		if s.pid == 49268 && s.signal == SignalTerm {
			saw = true
		}
	}
	if !saw {
		t.Fatalf("signals %v", h.process.signals())
	}
	if h.host.stateOf("metadata").ActualState != state.ActualReady {
		t.Fatalf("state %s", h.host.stateOf("metadata").ActualState)
	}
}

// A different program on the port is not ours to signal. Restart fails the same way start does.
func TestRestartLeavesADifferentProgramOnThePort(t *testing.T) {
	h := buildHarness(oneServiceCatalog(shellService("word", "viclass/packages/backend/viclass/eb.word", "exec node dist/main", 8012)))
	h.probes.setPortHolder(8012, 4242, "Mon Oct  5 11:20:13 2026", "python other.py")
	h.process.setCwd(4242, "viclass/packages/backend/viclass/eb.word")
	seedExternallyOwned(h.host, "word", state.DesiredRunning, "Port 8012 is held by pid 4242 (python other.py)")
	err := h.supervisor.Restart("word", nil)
	if err == nil || !contains(err.Error(), "externally owned") {
		t.Fatalf("err %v", err)
	}
	if !h.process.isAlive(4242) {
		t.Fatal("holder should be alive")
	}
	didNotSpawn(t, h.process)
	if h.host.stateOf("word").ActualState != state.ActualExternallyOwned {
		t.Fatalf("state %s", h.host.stateOf("word").ActualState)
	}
}

// A catalog `stop` command is still the way an adopted external unit is restarted.
func TestRestartOfAnExternalUnitStillRunsItsStopCommand(t *testing.T) {
	h := buildHarness(oneServiceCatalog(externalContainerService("cache", "proj-cache", strPtr("docker compose stop cache"))))
	h.probes.setContainerReady("proj-cache", true)
	h.host.seed(adoptedExternalState("cache"))
	if err := h.supervisor.Restart("cache", nil); err != nil {
		t.Fatal(err)
	}
	calls := h.process.stopContainerCalls()
	if len(calls) != 1 || calls[0] != "proj-cache" {
		t.Fatalf("stop container calls %v", calls)
	}
	if h.host.stateOf("cache").ActualState != state.ActualReady {
		t.Fatalf("state %s", h.host.stateOf("cache").ActualState)
	}
}
