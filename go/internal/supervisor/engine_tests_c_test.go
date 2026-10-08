// Ported from rust/crates/hearth-core/src/supervisor/engine/tests.rs (batch C).
package supervisor

import (
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/shared"
	"github.com/ngosangns/hearth/go/internal/state"
)

func TestStopServicesShutdownStopsExternalServicesWithAStopCommand(t *testing.T) {
	cat := catalog.ServiceCatalog{
		Services: []catalog.ServiceDefinition{
			externalContainerService("jitsi", "jitsi-web", strPtr("docker compose stop web")),
			externalContainerService("observed", "observed-db", nil),
			externalContainerService("idle", "idle-db", strPtr("docker compose stop idle")),
		},
		Groups:             map[string][]string{},
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
	}
	h := buildHarness(cat)
	h.host.seed(adoptedExternalState("jitsi"))
	h.host.seed(adoptedExternalState("observed"))
	h.supervisor.Shutdown()
	cmds := h.process.stopCommands()
	if len(cmds) != 1 || cmds[0] != "docker compose stop web" {
		t.Fatalf("stop commands %v", cmds)
	}
	if h.host.stateOf("jitsi").ActualState != state.ActualStopped {
		t.Fatalf("jitsi state %s", h.host.stateOf("jitsi").ActualState)
	}
	if h.host.stateOf("observed").ActualState != state.ActualReady {
		t.Fatalf("an external service with no stop command is only observed: %s", h.host.stateOf("observed").ActualState)
	}
}

// A `shared:` entry's stop is `hearth shared detach <id>`; `manager stop` adds
// `--stop-if-unused`. A plain `hearth stop` of the same service only detaches.
func TestStopServicesShutdownDetachesSharedEntriesWithStopIfUnused(t *testing.T) {
	service := shared.ProjectServiceEntry("postgres", "postgres@16.4", "/opt/hearth", nil, nil, nil)
	h := buildHarness(oneServiceCatalog(service))
	h.host.seed(adoptedExternalState("postgres"))
	if err := h.supervisor.Stop("postgres", nil); err != nil {
		t.Fatal(err)
	}
	h.host.seed(adoptedExternalState("postgres"))
	h.supervisor.Shutdown()
	cmds := h.process.stopCommands()
	want := []string{
		"/opt/hearth shared detach postgres@16.4",
		"/opt/hearth shared detach postgres@16.4 --stop-if-unused",
	}
	if len(cmds) != len(want) || cmds[0] != want[0] || cmds[1] != want[1] {
		t.Fatalf("stop commands %v", cmds)
	}
	if h.host.stateOf("postgres").ActualState != state.ActualStopped {
		t.Fatalf("state %s", h.host.stateOf("postgres").ActualState)
	}
}

func TestOrphansWhenTheObservedFingerprintNoLongerMatches(t *testing.T) {
	h := buildHarness(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})))
	h.probes.setTCPReady(8080, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	pid := apiPid(t, h)
	// Simulate a pid-reuse-like scenario: the OS-observed process at this pid now has a
	// different command fingerprint, so `observed_matches` must fail and the service must be
	// marked orphaned rather than treated as still-ours.
	h.process.mutateFingerprint(pid, "not-the-same-program-anymore")
	if err := h.supervisor.Status("api"); err != nil {
		t.Fatal(err)
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualOrphaned {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Error == nil || *st.Error != "Process ownership identity no longer matches" {
		t.Fatalf("error %v", st.Error)
	}
}

func TestBuildFailureShortCircuitsSpawn(t *testing.T) {
	service := argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})
	service.Profiles.Build = &catalog.ServiceBuildProfile{
		Command: catalog.ServiceCommand{Command: catalog.CommandSpec{Shell: "build-that-fails"}, Cwd: "."},
	}
	h := buildHarness(oneServiceCatalog(service))
	h.runBuild.failFor("build-that-fails")
	h.probes.setTCPReady(8080, true)
	err := h.supervisor.Start("api", nil)
	if err == nil || err.Error() != "Build failed" {
		t.Fatalf("err %v", err)
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualFailed {
		t.Fatalf("state %s", st.ActualState)
	}
	if len(h.process.signals()) != 0 {
		t.Fatal("spawn must never be attempted after a build failure")
	}
}

// A held port fails the start before the build runs, not after it.
func TestAHeldPortFailsTheStartBeforeTheBuildRuns(t *testing.T) {
	service := argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})
	service.Profiles.Build = &catalog.ServiceBuildProfile{
		Command: catalog.ServiceCommand{Command: catalog.CommandSpec{Shell: "slow-build"}, Cwd: "."},
	}
	h := buildHarness(oneServiceCatalog(service))
	h.probes.setPortInUse(8080, true)
	err := h.supervisor.Start("api", nil)
	if err == nil || !strings.Contains(err.Error(), "externally owned") || !strings.Contains(err.Error(), "held by") {
		t.Fatalf("err %v", err)
	}
	if len(h.runBuild.timeline()) != 0 {
		t.Fatal("the build must not run while the port is held")
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualExternallyOwned {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Error == nil || !strings.Contains(*st.Error, "Port 8080 is held by") {
		t.Fatalf("error %v", st.Error)
	}
}

func TestBuildSerializationByKeyRunsOneAtATime(t *testing.T) {
	serviceWithBuild := func(id, key string) catalog.ServiceDefinition {
		service := argvVerified(id, catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})
		k := key
		service.Profiles.Build = &catalog.ServiceBuildProfile{
			Command:          catalog.ServiceCommand{Command: catalog.CommandSpec{Shell: "build-" + id}, Cwd: "."},
			SerializationKey: &k,
		}
		return service
	}
	cat := catalog.ServiceCatalog{
		Services:           []catalog.ServiceDefinition{serviceWithBuild("a", "shared"), serviceWithBuild("b", "shared")},
		Groups:             map[string][]string{},
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
	}
	h := buildHarness(cat)
	h.probes.setTCPReady(8080, true)
	var wg sync.WaitGroup
	var errA, errB error
	wg.Add(2)
	go func() { defer wg.Done(); errA = h.supervisor.Start("a", nil) }()
	go func() { defer wg.Done(); errB = h.supervisor.Start("b", nil) }()
	wg.Wait()
	if errA != nil || errB != nil {
		t.Fatalf("errs %v %v", errA, errB)
	}
	timeline := h.runBuild.timeline()
	if len(timeline) != 2 {
		t.Fatalf("timeline %v", timeline)
	}
	gap := timeline[1].at.Sub(timeline[0].at)
	if gap < 4*time.Millisecond {
		t.Fatalf("builds sharing a serializationKey must not overlap, gap was %v", gap)
	}
}

func TestCommandReadinessWithNoAdapterStaysRunningUnready(t *testing.T) {
	readiness := catalog.ReadinessSpec{Kind: "command", Command: &catalog.CommandSpec{Argv: []string{"check"}}}
	h := buildHarnessWithTimeout(oneServiceCatalog(argvVerified("api", readiness)), 20)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	st := h.host.stateOf("api")
	if st.ActualState != state.ActualRunningUnready {
		t.Fatalf("state %s", st.ActualState)
	}
	if st.Error != nil && strings.Contains(*st.Error, "timed out") {
		t.Fatalf("unexpected timeout error: %v", *st.Error)
	}
}

func TestCommandReadinessSucceedsWhenTheAdapterReportsReady(t *testing.T) {
	host := newFakeHost(oneServiceCatalog(argvVerified("api", catalog.ReadinessSpec{
		Kind:    "command",
		Command: &catalog.CommandSpec{Argv: []string{"check"}},
	})))
	process := newFakeProcAdapter()
	inner := newFakeProbeAdapter()
	yes := true
	probes := newCommandCapableProbeAdapter(inner, &yes, 0)
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
	if host.stateOf("api").ActualState != state.ActualReady {
		t.Fatalf("state %s", host.stateOf("api").ActualState)
	}
}
