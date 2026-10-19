package supervisor

import (
	"strings"
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

func TestStopDisabledServicesStopsTheTrackedTree(t *testing.T) {
	api := argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})
	worker := argvVerified("worker", catalog.ReadinessSpec{Kind: "tcp", Port: port(8081)})
	h := buildHarness(catalog.ServiceCatalog{
		Services:           []catalog.ServiceDefinition{api, worker},
		Groups:             map[string][]string{},
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
	})
	h.probes.setTCPReady(8080, true)
	h.probes.setTCPReady(8081, true)
	if err := h.supervisor.Start("api", nil); err != nil {
		t.Fatal(err)
	}
	if err := h.supervisor.Start("worker", nil); err != nil {
		t.Fatal(err)
	}
	workerPid := h.host.stateOf("worker").Identity.PidValue()
	child := h.process.forkIntoOwnGroup(workerPid)
	apiPid := h.host.stateOf("api").Identity.PidValue()
	for i := range h.host.cat.Services {
		if h.host.cat.Services[i].ID == "worker" {
			h.host.cat.Services[i].Disabled = true
		}
	}

	h.supervisor.StopDisabledServices()

	stopped := h.host.stateOf("worker")
	if stopped.ActualState != state.ActualStopped || stopped.DesiredState != state.DesiredStopped {
		t.Fatalf("disabled service state %+v", stopped)
	}
	if h.process.isAlive(workerPid) || h.process.isAlive(child) {
		t.Fatalf("disabled processes still alive leader=%v child=%v", h.process.isAlive(workerPid), h.process.isAlive(child))
	}
	if h.host.stateOf("api").ActualState != state.ActualReady || !h.process.isAlive(apiPid) {
		t.Fatal("enabled service was stopped")
	}
}

func TestStopDisabledServicesReapsAnUntrackedMatchingProcess(t *testing.T) {
	worker := argvVerified("worker", catalog.ReadinessSpec{Kind: "process"})
	worker.Disabled = true
	h := buildHarness(oneServiceCatalog(worker))
	fingerprint := NormalizeCommandFingerprint(worker.Profiles.Run.Command)
	h.process.insertAlive(4242, fingerprint, "Mon Jan  1 00:00:00 2024")
	h.process.insertAlive(4243, "some-other-program", "Mon Jan  1 00:00:01 2024")

	h.supervisor.StopDisabledServices()

	if h.process.isAlive(4242) {
		t.Fatal("matching process of a disabled service was left running")
	}
	if !h.process.isAlive(4243) {
		t.Fatal("a process with a different command was signalled")
	}
}

func TestStopDisabledServicesReapsTheExecdProgramInItsDirectory(t *testing.T) {
	shell := "cd -- 'portal/metadata' && exec 'build/install/portal.metadata/bin/portal.metadata'"
	service := shellService("metadata", ".", shell, 1166)
	service.Disabled = true
	h := buildHarness(oneServiceCatalog(service))
	program, ok := execProgram(shell)
	if !ok {
		t.Fatal("shell has no exec program")
	}
	h.process.insertAlive(5151, NormalizeObservedCommandFingerprint(program), "Mon Jan  1 00:00:00 2024")
	h.process.setCwd(5151, "portal/metadata")
	h.process.insertAlive(5152, NormalizeObservedCommandFingerprint(program), "Mon Jan  1 00:00:01 2024")
	h.process.setCwd(5152, "/other/project/portal/metadata")

	h.supervisor.StopDisabledServices()

	if h.process.isAlive(5151) {
		t.Fatal("exec'd program of a disabled service was left running")
	}
	if !h.process.isAlive(5152) {
		t.Fatal("the same program in another directory was signalled")
	}
}

func TestStopDisabledServicesSkipsTheReapWhenProcessesCannotBeListed(t *testing.T) {
	worker := argvVerified("worker", catalog.ReadinessSpec{Kind: "process"})
	worker.Disabled = true
	h := buildHarness(oneServiceCatalog(worker))
	fingerprint := NormalizeCommandFingerprint(worker.Profiles.Run.Command)
	h.process.insertAlive(4242, fingerprint, "Mon Jan  1 00:00:00 2024")
	h.process.setMatchesUnknown()

	h.supervisor.StopDisabledServices()

	if !h.process.isAlive(4242) {
		t.Fatal("signalled a disabled service's process without a process list")
	}
}

func TestStartOfADisabledServiceIsRejected(t *testing.T) {
	service := argvVerified("api", catalog.ReadinessSpec{Kind: "tcp", Port: port(8080)})
	service.Disabled = true
	h := buildHarness(oneServiceCatalog(service))
	h.probes.setTCPReady(8080, true)
	err := h.supervisor.Start("api", nil)
	if err == nil || !strings.Contains(err.Error(), "service api is disabled") {
		t.Fatalf("err %v", err)
	}
	if st := h.host.stateOf("api"); st != nil && st.ActualState != state.ActualStopped {
		t.Fatalf("disabled service was started: %+v", st)
	}
}

func TestSyncExternalServicesDoesNotAdoptADisabledService(t *testing.T) {
	service := argvVerified("cache", catalog.ReadinessSpec{Kind: "tcp", Port: port(6379)})
	own := catalog.OwnershipExternal
	service.Ownership = &own
	service.Disabled = true
	h := buildHarness(oneServiceCatalog(service))
	h.probes.setTCPReady(6379, true)

	h.supervisor.SyncExternalServices()

	if st := h.host.stateOf("cache"); st != nil {
		t.Fatalf("disabled external service was adopted: %+v", st)
	}
}

func TestReconcileStopsADisabledServiceAndDoesNotReadoptIt(t *testing.T) {
	worker := argvVerified("worker", catalog.ReadinessSpec{Kind: "tcp", Port: port(8081)})
	h := buildHarness(oneServiceCatalog(worker))
	h.probes.setTCPReady(8081, true)
	if err := h.supervisor.Start("worker", nil); err != nil {
		t.Fatal(err)
	}
	h.host.cat.Services[0].Disabled = true

	h.supervisor.Reconcile()

	st := h.host.stateOf("worker")
	if st.ActualState != state.ActualStopped || st.DesiredState != state.DesiredStopped {
		t.Fatalf("reconcile left a disabled service running: %+v", st)
	}
}
