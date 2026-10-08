// Ported from rust/crates/hearth-core/src/supervisor/engine/tests.rs (batch H).
package supervisor

import (
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

func TestAnInstallScriptMarkerIsTheDirectoryJavaStillShows(t *testing.T) {
	java := shellCommand(".", "cd -- 'portal/metadata' && exec 'build/install/portal.metadata/bin/portal.metadata'")
	if marker, ok := installDirMarker(&java); !ok || marker != "portal/metadata/build/install/portal.metadata" {
		t.Fatalf("marker %v %q", ok, marker)
	}
	if got := processCwd(&java); got != "portal/metadata" {
		t.Fatalf("cwd %q", got)
	}
	node := shellCommand("viclass/packages/backend/viclass/eb.word", "exec node dist/main")
	if program, ok := execProgram("exec node dist/main"); !ok || program != "node dist/main" {
		t.Fatalf("program %v %q", ok, program)
	}
	if _, ok := installDirMarker(&node); ok {
		t.Fatal("node exec has no install dir")
	}
	if got := processCwd(&node); got != "viclass/packages/backend/viclass/eb.word" {
		t.Fatalf("cwd %q", got)
	}
	binServer := shellCommand(".", "exec ./bin/server")
	if _, ok := installDirMarker(&binServer); ok {
		t.Fatal("./bin/server has no install dir")
	}
}

func TestReconcileClearsAStalePortHeldRowWhenThePortIsFree(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("metadata", catalog.ReadinessSpec{Kind: "tcp", Port: port(1166)})))
	seedExternallyOwned(h.host, "metadata", state.DesiredRunning, "Port 1166 is held by pid 49268 (/usr/bin/java)")
	h.supervisor.Reconcile()
	st := h.host.stateOf("metadata")
	if st.ActualState != state.ActualFailed || st.DesiredState != state.DesiredRunning {
		t.Fatalf("state %+v", st)
	}
	if st.Error == nil || *st.Error != "Managed process is no longer alive" {
		t.Fatalf("error %v", st.Error)
	}
	if st.Identity != nil {
		t.Fatal("identity should be cleared")
	}
	didNotSpawn(t, h.process)
}

func TestReconcileClearsAStalePortHeldRowWhenTheServiceShouldBeStopped(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("metadata", catalog.ReadinessSpec{Kind: "tcp", Port: port(1166)})))
	seedExternallyOwned(h.host, "metadata", state.DesiredStopped, "Port 1166 is held by pid 49268 (/usr/bin/java)")
	h.supervisor.Reconcile()
	st := h.host.stateOf("metadata")
	if st.ActualState != state.ActualStopped {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Error != nil {
		t.Fatalf("error %v", *st.Error)
	}
	if st.Identity != nil {
		t.Fatal("identity should be cleared")
	}
	didNotSpawn(t, h.process)
}

func TestReconcileAdoptsTheExecTargetHoldingTheServicePort(t *testing.T) {
	cwd := "viclass/packages/backend/viclass/eb.word"
	h := buildHarness(oneServiceCatalog(shellService("word", cwd, "exec node dist/main", 8012)))
	h.probes.setPortHolder(8012, 4242, "Mon Oct  5 11:20:13 2026", "node dist/main")
	h.process.setCwd(4242, cwd)
	h.probes.setTCPReady(8012, true)
	seedExternallyOwned(h.host, "word", state.DesiredRunning, "Port 8012 is held by pid 4242 (node dist/main)")
	h.supervisor.Reconcile()
	st := h.host.stateOf("word")
	if st.ActualState != state.ActualReady {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Identity == nil || st.Identity.IsDocker() || st.Identity.PidValue() != 4242 {
		t.Fatalf("expected the holder pid, got %+v", st.Identity)
	}
	if !h.process.isAlive(4242) {
		t.Fatal("holder should be alive")
	}
	didNotSpawn(t, h.process)
}

func TestReconcileLeavesAnExecTargetInAnotherDirectory(t *testing.T) {
	h := buildHarness(oneServiceCatalog(shellService("word", "viclass/packages/backend/viclass/eb.word", "exec node dist/main", 8012)))
	h.probes.setPortHolder(8012, 4242, "Mon Oct  5 11:20:13 2026", "node dist/main")
	h.process.setCwd(4242, "/tmp")
	seedExternallyOwned(h.host, "word", state.DesiredRunning, "Port 8012 is held by pid 4242 (node dist/main)")
	h.supervisor.Reconcile()
	st := h.host.stateOf("word")
	if st.ActualState != state.ActualExternallyOwned {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Identity != nil {
		t.Fatal("identity should be nil")
	}
	if !h.process.isAlive(4242) {
		t.Fatal("holder should be alive")
	}
	didNotSpawn(t, h.process)
}

func TestReconcileAdoptsAJavaProcessFromItsInstallDirectory(t *testing.T) {
	h := buildHarness(oneServiceCatalog(shellService("metadata", ".", "cd -- 'portal/metadata' && exec 'build/install/portal.metadata/bin/portal.metadata'", 1166)))
	command := "/usr/bin/java -classpath /Users/ngosangns/Github/ryan/viclass/portal/metadata/build/install/portal.metadata/lib/app.jar"
	h.probes.setPortHolder(1166, 49268, "Mon Oct  5 11:26:11 2026", command)
	h.probes.setTCPReady(1166, true)
	seedExternallyOwned(h.host, "metadata", state.DesiredRunning, "Port 1166 is held by pid 49268 (/usr/bin/java)")
	h.supervisor.Reconcile()
	st := h.host.stateOf("metadata")
	if st.ActualState != state.ActualReady {
		t.Fatalf("state %+v", st)
	}
	if st.Identity == nil || st.Identity.IsDocker() || st.Identity.PidValue() != 49268 {
		t.Fatalf("expected the holder pid, got %+v", st.Identity)
	}
	if !h.process.isAlive(49268) {
		t.Fatal("holder should be alive")
	}
	didNotSpawn(t, h.process)
}

func TestReconcileLeavesADifferentProgramOnThePort(t *testing.T) {
	h := buildHarness(oneServiceCatalog(shellService("word", "viclass/packages/backend/viclass/eb.word", "exec node dist/main", 8012)))
	h.probes.setPortHolder(8012, 4242, "Mon Oct  5 11:20:13 2026", "python other.py")
	h.process.setCwd(4242, "viclass/packages/backend/viclass/eb.word")
	seedExternallyOwned(h.host, "word", state.DesiredRunning, "Port 8012 is held by pid 4242 (python other.py)")
	h.supervisor.Reconcile()
	st := h.host.stateOf("word")
	if st.ActualState != state.ActualExternallyOwned {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Error == nil || *st.Error != "Port 8012 is held by pid 4242 (python other.py)" {
		t.Fatalf("error %v", st.Error)
	}
	if !h.process.isAlive(4242) {
		t.Fatal("holder should be alive")
	}
	didNotSpawn(t, h.process)
}

func TestReconcileDoesNotAdoptPidOne(t *testing.T) {
	cwd := "viclass/packages/backend/viclass/eb.word"
	h := buildHarness(oneServiceCatalog(shellService("word", cwd, "exec node dist/main", 8012)))
	h.probes.setPortHolder(8012, 1, "Mon Oct  5 11:20:13 2026", "node dist/main")
	h.process.setCwd(1, cwd)
	seedExternallyOwned(h.host, "word", state.DesiredRunning, "Port 8012 is held by pid 1 (node dist/main)")
	h.supervisor.Reconcile()
	st := h.host.stateOf("word")
	if st.ActualState != state.ActualExternallyOwned {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Identity != nil {
		t.Fatal("identity should be nil")
	}
	didNotSpawn(t, h.process)
}

func TestReconcileLeavesAnExternallyOwnedRowWhenThePortProbeFails(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("metadata", catalog.ReadinessSpec{Kind: "tcp", Port: port(1166)})))
	h.probes.setPortProbeUnknown()
	seedExternallyOwned(h.host, "metadata", state.DesiredRunning, "Port 1166 is held by pid 49268 (/usr/bin/java)")
	h.supervisor.Reconcile()
	st := h.host.stateOf("metadata")
	if st.ActualState != state.ActualExternallyOwned {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Error == nil || *st.Error != "Port 1166 is held by pid 49268 (/usr/bin/java)" {
		t.Fatalf("error %v", st.Error)
	}
	didNotSpawn(t, h.process)
}
