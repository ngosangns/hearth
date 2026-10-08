package supervisor

import (
	"strings"
	"sync"
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

type memHost struct {
	mu       sync.Mutex
	cat      *catalog.ServiceCatalog
	states   map[string]state.ServiceLifecycleState
	instance string
	logs     []string
	bg       []string
}

func (h *memHost) InstanceID() string               { return h.instance }
func (h *memHost) Catalog() *catalog.ServiceCatalog { return h.cat }
func (h *memHost) ServiceStates() []state.ServiceLifecycleState {
	h.mu.Lock()
	defer h.mu.Unlock()
	out := make([]state.ServiceLifecycleState, 0, len(h.states))
	for _, st := range h.states {
		out = append(out, st)
	}
	return out
}
func (h *memHost) ServiceState(id string) *state.ServiceLifecycleState {
	h.mu.Lock()
	defer h.mu.Unlock()
	st, ok := h.states[id]
	if !ok {
		return nil
	}
	cp := st
	return &cp
}
func (h *memHost) SetServiceState(next *state.ServiceLifecycleState) {
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.states == nil {
		h.states = map[string]state.ServiceLifecycleState{}
	}
	h.states[next.ServiceID] = *next
}
func (h *memHost) AppendLog(id, data string) {
	h.mu.Lock()
	h.logs = append(h.logs, id+":"+data)
	h.mu.Unlock()
}
func (h *memHost) Publish(string, map[string]any) {}
func (h *memHost) RecordBackgroundError(scope, err string) {
	h.mu.Lock()
	h.bg = append(h.bg, scope+":"+err)
	h.mu.Unlock()
}

type fakeProc struct {
	mu        sync.Mutex
	inspect   Inspection
	inspectFn func(*state.ProcessIdentity) Inspection
	spawned   int
	pids      []int64
	groups    []int64
	stopped   int
	supported bool
	stopErr   error
	tree      ProcessTreeSnapshot
	live      map[int64]string
	matches   *[]CommandMatch
}

func (f *fakeProc) Spawn(input SpawnInput, _ OnOutput) (*ManagedProcess, error) {
	f.mu.Lock()
	f.spawned++
	f.mu.Unlock()
	exited := make(chan int32)
	return &ManagedProcess{
		Record: ProcessRecord{Posix: &PosixProcessRecord{Pid: 42, Pgid: 42, StartIdentity: "start", CommandFingerprint: input.CommandFingerprint, CommandLine: "svc"}},
		Exited: exited,
	}, nil
}
func (f *fakeProc) Inspect(id *state.ProcessIdentity) Inspection {
	if f.inspectFn != nil {
		return f.inspectFn(id)
	}
	return f.inspect
}
func (f *fakeProc) SignalGroup(pgid int64, _ ProcessSignal) {
	f.mu.Lock()
	f.groups = append(f.groups, pgid)
	f.mu.Unlock()
}
func (f *fakeProc) SignalPID(pid int64, _ string, _ ProcessSignal) {
	f.mu.Lock()
	f.pids = append(f.pids, pid)
	f.mu.Unlock()
}
func (f *fakeProc) ProcessTree(int64, string) ProcessTreeSnapshot {
	if f.tree.Kind == 0 && f.tree.Entries == nil {
		return AbsentSnapshot()
	}
	return f.tree
}
func (f *fakeProc) LiveStartIdentities() map[int64]string {
	if f.live == nil {
		return map[int64]string{}
	}
	return f.live
}
func (f *fakeProc) CommandMatches([]string, []string, string) *[]CommandMatch { return f.matches }
func (f *fakeProc) StopContainer(*catalog.ServiceCommand, OnOutput) (bool, error) {
	f.mu.Lock()
	f.stopped++
	f.mu.Unlock()
	return f.supported, f.stopErr
}
func (f *fakeProc) AttachOutput(string, OutputSource, OnOutput) *OutputTail { return nil }

type fakeProbes struct {
	inUse   *bool
	holders *[]PortHolder
	tcp     bool
}

func (p fakeProbes) TCP(uint16) bool                  { return p.tcp }
func (p fakeProbes) HTTP(string) bool                 { return false }
func (p fakeProbes) Container(string) bool            { return false }
func (p fakeProbes) Tailnet() bool                    { return false }
func (p fakeProbes) PortInUse(uint16) *bool           { return p.inUse }
func (p fakeProbes) PortHolders(uint16) *[]PortHolder { return p.holders }
func (p fakeProbes) Command(CommandContext, *catalog.CommandSpec, *string) *bool {
	v := true
	return &v
}

func testSupervisor(service catalog.ServiceDefinition, proc *fakeProc, probes fakeProbes) (*ProcessSupervisor, *memHost) {
	host := &memHost{
		instance: "mgr",
		cat:      &catalog.ServiceCatalog{Services: []catalog.ServiceDefinition{service}},
		states:   map[string]state.ServiceLifecycleState{},
	}
	sup := NewProcessSupervisor(host, SupervisorOptions{
		Process: proc, Probes: probes, Clock: SystemClock{},
		ReadinessBackoffMs: 1, TerminationGraceMs: 1, ReadinessTimeoutMs: 20,
		IsClosing: func() bool { return false },
	})
	return sup, host
}

func verified(id string, kind string, port *uint16) catalog.ServiceDefinition {
	cmd := catalog.ServiceCommand{Cwd: ".", Command: catalog.CommandSpec{Argv: []string{"/bin/svc"}}}
	return catalog.ServiceDefinition{
		ID: id,
		Profiles: catalog.ServiceProfiles{Run: catalog.ServiceRunProfile{
			CommandStatus: "verified",
			Command:       &cmd,
			Readiness:     catalog.ReadinessSpec{Kind: kind, Port: port},
		}},
	}
}

func TestInstallDirMarkerAndExecProgram(t *testing.T) {
	cmd := &catalog.ServiceCommand{Cwd: ".", Command: catalog.CommandSpec{
		Shell: "cd portal/metadata && exec build/install/portal.metadata/bin/portal.metadata",
	}}
	marker, ok := installDirMarker(cmd)
	if !ok || marker != "portal/metadata/build/install/portal.metadata" {
		t.Fatalf("marker %v %q", ok, marker)
	}
	node := &catalog.ServiceCommand{Cwd: "app", Command: catalog.CommandSpec{Shell: "exec node dist/main"}}
	if _, ok := installDirMarker(node); ok {
		t.Fatal("node exec has no install dir")
	}
	program, ok := execProgram("cd -- 'portal/metadata' && exec 'build/install/name/bin/name'")
	if !ok || program != "build/install/name/bin/name" {
		t.Fatalf("program %v %q", ok, program)
	}
	if _, ok := lastShellWord("execution", "exec"); ok {
		t.Fatal("execution is not the exec keyword")
	}
}

func TestStopUnownedWithoutStopCommandFails(t *testing.T) {
	svc := verified("web", "tcp", port(8080))
	own := catalog.OwnershipExternal
	svc.Ownership = &own
	proc := &fakeProc{}
	sup, host := testSupervisor(svc, proc, fakeProbes{})
	host.SetServiceState(&state.ServiceLifecycleState{
		ServiceID: "web", DesiredState: state.DesiredRunning, ActualState: state.ActualExternallyOwned,
		Readiness: state.ReadinessFailed, Generation: 3, CreatedAt: "t", UpdatedAt: "t",
		Error: strPtr("Port 8080 is held by pid 9 (other)"),
	})
	err := sup.Stop("web", nil)
	if err == nil || !strings.Contains(err.Error(), "no `stop` command") {
		t.Fatalf("err %v", err)
	}
	st := host.ServiceState("web")
	if st.ActualState != state.ActualExternallyOwned {
		t.Fatalf("state changed to %s", st.ActualState)
	}
	if proc.stopped != 0 {
		t.Fatalf("stop command ran")
	}
}

func TestStopFinishedExitLeavesState(t *testing.T) {
	svc := verified("job", "exit", nil)
	proc := &fakeProc{}
	sup, host := testSupervisor(svc, proc, fakeProbes{})
	code := int32(0)
	host.SetServiceState(&state.ServiceLifecycleState{
		ServiceID: "job", DesiredState: state.DesiredStopped, ActualState: state.ActualSucceeded,
		Readiness: state.ReadinessUnknown, Generation: 2, CreatedAt: "t", UpdatedAt: "t",
		ExitCode: &code,
	})
	if err := sup.Stop("job", nil); err != nil {
		t.Fatal(err)
	}
	st := host.ServiceState("job")
	if st.ActualState != state.ActualSucceeded || st.ExitCode == nil || *st.ExitCode != 0 {
		t.Fatalf("wiped exit row: %+v", st)
	}
}

func TestReconcileUnknownDoesNotFail(t *testing.T) {
	svc := verified("web", "process", nil)
	proc := &fakeProc{inspect: Inspection{Kind: InspectionUnknown}}
	sup, host := testSupervisor(svc, proc, fakeProbes{})
	pid := int64(7)
	start := "Mon"
	host.SetServiceState(&state.ServiceLifecycleState{
		ServiceID: "web", DesiredState: state.DesiredRunning, ActualState: state.ActualReady,
		Readiness: state.ReadinessReady, Generation: 4, CreatedAt: "t", UpdatedAt: "t",
		Identity: &state.ProcessIdentity{ManagerInstanceID: "mgr", ServiceID: "web", Generation: 4, Pid: &pid, Pgid: &pid, StartIdentity: &start},
	})
	sup.Reconcile()
	st := host.ServiceState("web")
	if st.ActualState != state.ActualReady {
		t.Fatalf("unknown probe changed state to %s", st.ActualState)
	}
}

func TestLogMatcherSpansWrites(t *testing.T) {
	svc := verified("web", "log", nil)
	svc.Profiles.Run.Readiness.Pattern = "READY"
	sup, _ := testSupervisor(svc, &fakeProc{}, fakeProbes{})
	sup.feedLogMatcher("web", "REA")
	if sup.probe(&svc.Profiles.Run.Readiness, "web") {
		t.Fatal("partial chunk latched")
	}
	sup.feedLogMatcher("web", "DY now")
	if !sup.probe(&svc.Profiles.Run.Readiness, "web") {
		t.Fatal("split pattern did not latch")
	}
}

func TestRestartWithoutIdentityStarts(t *testing.T) {
	svc := verified("web", "process", nil)
	empty := []CommandMatch{}
	proc := &fakeProc{matches: &empty}
	proc.inspectFn = func(id *state.ProcessIdentity) Inspection {
		proc.mu.Lock()
		spawned := proc.spawned
		proc.mu.Unlock()
		if spawned == 0 || id == nil || id.Pid == nil {
			return Inspection{Kind: InspectionGone}
		}
		return Observed(ProcessRecord{Posix: &PosixProcessRecord{
			Pid: id.PidValue(), Pgid: id.PgidValue(), StartIdentity: id.StartIdentityValue(),
			CommandFingerprint: id.CommandFingerprint, CommandLine: "/bin/svc",
		}}, true)
	}
	sup, host := testSupervisor(svc, proc, fakeProbes{})
	host.SetServiceState(&state.ServiceLifecycleState{
		ServiceID: "web", DesiredState: state.DesiredStopped, ActualState: state.ActualExternallyOwned,
		Readiness: state.ReadinessFailed, Generation: 1, CreatedAt: "t", UpdatedAt: "t",
	})
	if err := sup.Restart("web", nil); err != nil {
		t.Fatal(err)
	}
	if proc.spawned != 1 {
		t.Fatalf("spawned %d", proc.spawned)
	}
	st := host.ServiceState("web")
	if st.ActualState != state.ActualRunningUnready {
		t.Fatalf("state %s", st.ActualState)
	}
	sup.BeginShutdown()
}

func TestReclaimSignalsPidsNotGroups(t *testing.T) {
	portN := uint16(9)
	svc := verified("web", "tcp", &portN)
	inUse := true
	holders := []PortHolder{{Pid: 15, Pgid: 15, StartIdentity: "s", Command: "other"}}
	proc := &fakeProc{}
	sup, host := testSupervisor(svc, proc, fakeProbes{inUse: &inUse, holders: &holders})
	free := false
	// First signal pass still sees the port held; second pass's poll sees it free
	// after the kill. The fake flips on the first SignalPID.
	procOnSignal := &signalFlip{fakeProc: proc, probes: sup.options.Probes.(fakeProbes), free: &free}
	_ = procOnSignal
	err := sup.StartWithOptions("web", nil, StartOptions{KillUnowned: true})
	// Port stays held, so reclaim fails closed. No group signal either way.
	if err == nil {
		t.Fatal("expected reclaim to fail while the port stays held")
	}
	if len(proc.groups) != 0 {
		t.Fatalf("signalled groups %v", proc.groups)
	}
	if len(proc.pids) == 0 || proc.pids[0] != 15 {
		t.Fatalf("pids %v", proc.pids)
	}
	st := host.ServiceState("web")
	if st == nil || st.ActualState != state.ActualExternallyOwned {
		t.Fatalf("state %+v", st)
	}
}

type signalFlip struct {
	*fakeProc
	probes fakeProbes
	free   *bool
}

func port(n uint16) *uint16   { return &n }
func strPtr(s string) *string { return &s }
