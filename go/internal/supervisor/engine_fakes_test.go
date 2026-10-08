// Fake adapters for the engine tests — the Go port of the fakes in
// rust/crates/hearth-core/src/supervisor/engine/tests.rs. `fakeProcAdapter`
// keeps a simulated process table (pid, pgid, start identity, parent) so the
// whole-tree stop path — snapshot, leader group, secondary groups — runs
// against it exactly as it runs against `ps` in production.
package supervisor

import (
	"fmt"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/iso8601"
	"github.com/ngosangns/hearth/go/internal/state"
)

// --- Fake clock: sleeps advance a virtual clock instantly, so readiness-timeout tests run in
// milliseconds instead of really waiting. ---

type fakeClock struct {
	mu     sync.Mutex
	millis int64
}

func newFakeClock() *fakeClock { return &fakeClock{millis: 1_700_000_000_000} }

func (c *fakeClock) NowMillis() int64 {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.millis
}

func (c *fakeClock) Now() string { return iso8601.FormatMillis(c.NowMillis()) }

func (c *fakeClock) Sleep(millis int64) {
	if millis < 1 {
		millis = 1
	}
	c.mu.Lock()
	c.millis += millis
	c.mu.Unlock()
	runtime.Gosched()
}

// --- Fake process adapter ---

type signalRec struct {
	pid    int64
	signal ProcessSignal
}

type attachCall struct {
	serviceID string
	kind      string
}

type fakeProcState struct {
	nextPid            int64
	alive              map[int64]PosixProcessRecord
	parents            map[int64]int64
	signals            []signalRec
	pidSigs            []signalRec
	termImmune         map[int64]bool
	unkillable         map[int64]bool
	spawnShouldFail    map[string]string
	containerRecords   map[string]DockerContainerRecord
	exitSenders        map[int64]chan int32
	exitOnSpawn        *int32
	attachOutputCalls  []attachCall
	tailDones          map[string]*atomic.Bool
	tailsStopped       []string
	stopContainerCalls []string
	stopCommands       []string
	treeUnknown        bool
	matchesUnknown     bool
	cwdByPid           map[int64]string
	outputSinks        map[string]OnOutput
	spawnLog           []string
}

func newFakeProcState() *fakeProcState {
	return &fakeProcState{
		nextPid:          900_001,
		alive:            map[int64]PosixProcessRecord{},
		parents:          map[int64]int64{},
		termImmune:       map[int64]bool{},
		unkillable:       map[int64]bool{},
		spawnShouldFail:  map[string]string{},
		containerRecords: map[string]DockerContainerRecord{},
		exitSenders:      map[int64]chan int32{},
		tailDones:        map[string]*atomic.Bool{},
		cwdByPid:         map[int64]string{},
		outputSinks:      map[string]OnOutput{},
	}
}

type fakeProcAdapter struct {
	mu    sync.Mutex
	state *fakeProcState
}

func newFakeProcAdapter() *fakeProcAdapter {
	return &fakeProcAdapter{state: newFakeProcState()}
}

func (f *fakeProcAdapter) attachCalls() []attachCall {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]attachCall{}, f.state.attachOutputCalls...)
}

// finishTail simulates a follower ending on its own — `docker logs --follow` exits when its
// container does, leaving a stale stop handle in the supervisor's map.
func (f *fakeProcAdapter) finishTail(serviceID string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if done := f.state.tailDones[serviceID]; done != nil {
		done.Store(true)
	}
}

func (f *fakeProcAdapter) tailWasStopped(serviceID string) bool {
	f.mu.Lock()
	defer f.mu.Unlock()
	for _, id := range f.state.tailsStopped {
		if id == serviceID {
			return true
		}
	}
	return false
}

func (f *fakeProcAdapter) stopCommands() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]string{}, f.state.stopCommands...)
}

func (f *fakeProcAdapter) stopContainerCalls() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]string{}, f.state.stopContainerCalls...)
}

func (f *fakeProcAdapter) failSpawn(serviceID, message string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.state.spawnShouldFail[serviceID] = message
}

func (f *fakeProcAdapter) registerContainer(name string, record DockerContainerRecord) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.state.containerRecords[name] = record
}

func (f *fakeProcAdapter) signals() []signalRec {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]signalRec{}, f.state.signals...)
}

func (f *fakeProcAdapter) pidSignals() []signalRec {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]signalRec{}, f.state.pidSigs...)
}

func (f *fakeProcAdapter) markTermImmune(pid int64) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.state.termImmune[pid] = true
}

func (f *fakeProcAdapter) markUnkillable(pid int64) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.state.unkillable[pid] = true
}

func (f *fakeProcAdapter) setMatchesUnknown() {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.state.matchesUnknown = true
}

func (f *fakeProcAdapter) setTreeUnknown() {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.state.treeUnknown = true
}

func (f *fakeProcAdapter) setCwd(pid int64, cwd string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.state.cwdByPid[pid] = cwd
}

// setExitOnSpawn: the spawned process has already exited by the time `spawn` returns. The
// channel is filled first, so a one-shot test does not race the virtual readiness clock.
func (f *fakeProcAdapter) setExitOnSpawn(code int32) {
	f.mu.Lock()
	defer f.mu.Unlock()
	c := code
	f.state.exitOnSpawn = &c
}

func (f *fakeProcAdapter) spawnCount(serviceID string) int {
	f.mu.Lock()
	defer f.mu.Unlock()
	n := 0
	for _, id := range f.state.spawnLog {
		if id == serviceID {
			n++
		}
	}
	return n
}

// emitOutput replays one chunk of process output through the sink `spawn` recorded — the fake's
// equivalent of the process printing to stdout.
func (f *fakeProcAdapter) emitOutput(serviceID, data string) {
	f.mu.Lock()
	sink := f.state.outputSinks[serviceID]
	f.mu.Unlock()
	if sink != nil {
		sink(data)
	}
}

// killExternally simulates a process dying on its own (not via a signal from us) — fires its
// exit code and removes it from the alive set, exactly like a real process disappearing.
func (f *fakeProcAdapter) killExternally(pid int64, code int32) {
	f.mu.Lock()
	defer f.mu.Unlock()
	st := f.state
	delete(st.alive, pid)
	if ch, ok := st.exitSenders[pid]; ok {
		delete(st.exitSenders, pid)
		ch <- code
		close(ch)
	}
}

// forkIntoOwnGroup simulates `air` handing its built server its own process group: a live child
// of `parent` whose pgid is its own pid, so signalling only the parent's group leaves it running.
func (f *fakeProcAdapter) forkIntoOwnGroup(parent int64) int64 {
	f.mu.Lock()
	defer f.mu.Unlock()
	st := f.state
	pid := st.nextPid
	st.nextPid++
	st.alive[pid] = PosixProcessRecord{
		Pid: pid, Pgid: pid,
		StartIdentity:      fmt.Sprintf("Child Jan  1 00:00:%02d 2024", pid%60),
		CommandFingerprint: "child",
	}
	st.parents[pid] = parent
	return pid
}

// forkInGroup: a live child of `parent` that stays in `parent`'s process group.
func (f *fakeProcAdapter) forkInGroup(parent int64) int64 {
	f.mu.Lock()
	defer f.mu.Unlock()
	st := f.state
	pgid := parent
	if r, ok := st.alive[parent]; ok {
		pgid = r.Pgid
	}
	pid := st.nextPid
	st.nextPid++
	st.alive[pid] = PosixProcessRecord{
		Pid: pid, Pgid: pgid,
		StartIdentity:      fmt.Sprintf("Child Jan  1 00:00:%02d 2024", pid%60),
		CommandFingerprint: "child",
	}
	st.parents[pid] = parent
	return pid
}

// reparentToLaunchd: the parent exited and launchd adopted `pid` (ppid 1): a double fork.
func (f *fakeProcAdapter) reparentToLaunchd(pid int64) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.state.parents[pid] = 1
}

func (f *fakeProcAdapter) isAlive(pid int64) bool {
	f.mu.Lock()
	defer f.mu.Unlock()
	_, ok := f.state.alive[pid]
	return ok
}

func (f *fakeProcAdapter) nextPidValue() int64 {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.state.nextPid
}

func (f *fakeProcAdapter) insertAlive(pid int64, fingerprint, startIdentity string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.state.alive[pid] = PosixProcessRecord{
		Pid: pid, Pgid: pid,
		StartIdentity:      startIdentity,
		CommandFingerprint: fingerprint,
	}
}

func (f *fakeProcAdapter) mutateFingerprint(pid int64, newFingerprint string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if r, ok := f.state.alive[pid]; ok {
		r.CommandFingerprint = newFingerprint
		f.state.alive[pid] = r
	}
}

// reusePid: the pid was recycled — a different process (another start time, another program)
// now sits at it.
func (f *fakeProcAdapter) reusePid(pid int64, startIdentity, fingerprint string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if r, ok := f.state.alive[pid]; ok {
		r.StartIdentity = startIdentity
		r.CommandFingerprint = fingerprint
		f.state.alive[pid] = r
	}
}

// rewriteCommandLine: `ps` after a server rewrites its title — the fingerprint changes with the
// new line, and the executable path stays in argv0.
func (f *fakeProcAdapter) rewriteCommandLine(pid int64, commandLine string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if r, ok := f.state.alive[pid]; ok {
		r.CommandLine = commandLine
		r.CommandFingerprint = "rewritten:" + commandLine
		f.state.alive[pid] = r
	}
}

func (f *fakeProcAdapter) Spawn(input SpawnInput, onOutput OnOutput) (*ManagedProcess, error) {
	f.mu.Lock()
	st := f.state
	if msg, ok := st.spawnShouldFail[input.ServiceID]; ok {
		f.mu.Unlock()
		return nil, NewError(msg)
	}
	st.outputSinks[input.ServiceID] = onOutput
	st.spawnLog = append(st.spawnLog, input.ServiceID)
	if catalog.IsContainerCommand(&input.Command) {
		name := ""
		if input.Command.ContainerName != nil {
			name = *input.Command.ContainerName
		}
		record, ok := st.containerRecords[name]
		if !ok {
			record = DockerContainerRecord{
				ContainerName:      name,
				ContainerID:        "container-" + name,
				ContainerStartedAt: "2024-01-01T00:00:00.000Z",
				CommandFingerprint: input.CommandFingerprint,
			}
		}
		f.mu.Unlock()
		// A container has no child handle; the tail never resolves on its own. (Rust's fake
		// drops the oneshot sender, which resolves -1; Go's scheduler would race that against
		// the test's assertion, so the channel simply never fires.)
		return &ManagedProcess{Record: ProcessRecord{Docker: &record}, Exited: make(chan int32)}, nil
	}
	pid := st.nextPid
	st.nextPid++
	record := PosixProcessRecord{
		Pid: pid, Pgid: pid,
		StartIdentity:      fmt.Sprintf("Fake Jan  1 00:00:%02d 2024", pid%60),
		CommandFingerprint: input.CommandFingerprint,
		CommandLine:        displayedCommand(&input.Command),
	}
	ch := make(chan int32, 1)
	if st.exitOnSpawn != nil {
		code := *st.exitOnSpawn
		f.mu.Unlock()
		ch <- code
		close(ch)
		return &ManagedProcess{Record: ProcessRecord{Posix: &record}, Exited: ch}, nil
	}
	st.alive[pid] = record
	st.exitSenders[pid] = ch
	f.mu.Unlock()
	return &ManagedProcess{Record: ProcessRecord{Posix: &record}, Exited: ch}, nil
}

func (f *fakeProcAdapter) Inspect(identity *state.ProcessIdentity) Inspection {
	f.mu.Lock()
	defer f.mu.Unlock()
	if identity == nil {
		return Inspection{Kind: InspectionGone}
	}
	if identity.IsDocker() {
		name := ""
		if identity.ContainerName != nil {
			name = *identity.ContainerName
		}
		rec, ok := f.state.containerRecords[name]
		if !ok {
			return Inspection{Kind: InspectionGone}
		}
		return Observed(ProcessRecord{Docker: &rec}, true)
	}
	rec, ok := f.state.alive[identity.PidValue()]
	if !ok {
		return Inspection{Kind: InspectionGone}
	}
	return Observed(ProcessRecord{Posix: &rec}, true)
}

func (f *fakeProcAdapter) SignalGroup(pgid int64, signal ProcessSignal) {
	f.mu.Lock()
	defer f.mu.Unlock()
	st := f.state
	st.signals = append(st.signals, signalRec{pgid, signal})
	// Every member of the group dies — and only the group: a tree member that moved into a
	// group of its own survives, exactly like the real `air` case.
	var members []int64
	for pid, r := range st.alive {
		if r.Pgid != pgid {
			continue
		}
		if signal == SignalTerm && st.termImmune[pid] {
			continue
		}
		members = append(members, pid)
	}
	for _, pid := range members {
		delete(st.alive, pid)
		if ch, ok := st.exitSenders[pid]; ok {
			delete(st.exitSenders, pid)
			code := int32(143)
			if signal == SignalKill {
				code = 137
			}
			ch <- code
			close(ch)
		}
	}
}

func (f *fakeProcAdapter) ProcessTree(leaderPid int64, leaderStartIdentity string) ProcessTreeSnapshot {
	f.mu.Lock()
	defer f.mu.Unlock()
	st := f.state
	if st.treeUnknown {
		return UnknownSnapshot()
	}
	rows := make([]PsTreeRow, 0, len(st.alive))
	for _, r := range st.alive {
		ppid := int64(1)
		if p, ok := st.parents[r.Pid]; ok {
			ppid = p
		}
		rows = append(rows, PsTreeRow{Pid: r.Pid, Ppid: ppid, Pgid: r.Pgid, StartIdentity: r.StartIdentity})
	}
	tree := BuildProcessTree(rows, leaderPid, leaderStartIdentity)
	if len(tree) == 0 {
		return AbsentSnapshot()
	}
	return PresentSnapshot(tree)
}

func (f *fakeProcAdapter) LiveStartIdentities() map[int64]string {
	f.mu.Lock()
	defer f.mu.Unlock()
	out := map[int64]string{}
	for pid, r := range f.state.alive {
		out[pid] = r.StartIdentity
	}
	return out
}

func (f *fakeProcAdapter) CommandMatches(fingerprints, executables []string, cwd string) *[]CommandMatch {
	f.mu.Lock()
	defer f.mu.Unlock()
	st := f.state
	if st.matchesUnknown {
		return nil
	}
	out := []CommandMatch{}
	for _, r := range st.alive {
		hit := false
		for _, fp := range fingerprints {
			if fp == r.CommandFingerprint {
				hit = true
				break
			}
		}
		if !hit && r.CommandLine != "" && CommandFingerprintMatches(r.CommandLine, fingerprints) {
			hit = true
		}
		if !hit && ExecutableMatches(r.CommandLine, executables) {
			hit = true
		}
		if !hit {
			continue
		}
		if recorded, ok := st.cwdByPid[r.Pid]; ok {
			if !pathsEqual(recorded, cwd) {
				continue
			}
		}
		out = append(out, CommandMatch{Pid: r.Pid, StartIdentity: r.StartIdentity})
	}
	return &out
}

func (f *fakeProcAdapter) SignalPID(pid int64, expectedStartIdentity string, signal ProcessSignal) {
	f.mu.Lock()
	defer f.mu.Unlock()
	st := f.state
	st.pidSigs = append(st.pidSigs, signalRec{pid, signal})
	// Same contract as the real adapter: a pid that no longer presents the resolved lstart is
	// not ours to signal — reclaim can never kill a recycled process in the fake either.
	survives := st.unkillable[pid] || (st.termImmune[pid] && signal == SignalTerm)
	if r, ok := st.alive[pid]; ok && r.StartIdentity == expectedStartIdentity && !survives {
		delete(st.alive, pid)
	}
}

func (f *fakeProcAdapter) StopContainer(command *catalog.ServiceCommand, _ OnOutput) (bool, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	st := f.state
	if command.DockerStopCommand != nil {
		st.stopCommands = append(st.stopCommands, commandSpecText(command.DockerStopCommand))
	}
	if command.ContainerName == nil {
		// A non-container `stop:` command (a `shared:` detach) just runs.
		return command.DockerStopCommand != nil, nil
	}
	name := *command.ContainerName
	st.stopContainerCalls = append(st.stopContainerCalls, name)
	delete(st.containerRecords, name)
	return true, nil
}

func (f *fakeProcAdapter) AttachOutput(serviceID string, source OutputSource, _ OnOutput) *OutputTail {
	kind := "process"
	if source.ContainerName != "" {
		kind = "container"
	}
	done := &atomic.Bool{}
	f.mu.Lock()
	st := f.state
	st.attachOutputCalls = append(st.attachOutputCalls, attachCall{serviceID, kind})
	st.tailDones[serviceID] = done
	f.mu.Unlock()
	return NewOutputTail(func() {
		f.mu.Lock()
		f.state.tailsStopped = append(f.state.tailsStopped, serviceID)
		f.mu.Unlock()
	}, done)
}

// --- Fake probe adapter ---

type fakeProbeState struct {
	tcpReady               map[uint16]bool
	portInUse              map[uint16]bool
	portHolders            map[uint16][]PortHolder
	portHoldersUnsupported bool
	portProbeUnknown       bool
	containerReady         map[string]bool
	tailnetReady           bool
}

type fakeProbeAdapter struct {
	mu      sync.Mutex
	state   *fakeProbeState
	process *fakeProcAdapter
}

func newFakeProbeAdapter() *fakeProbeAdapter {
	return &fakeProbeAdapter{
		state: &fakeProbeState{
			tcpReady:       map[uint16]bool{},
			portInUse:      map[uint16]bool{},
			portHolders:    map[uint16][]PortHolder{},
			containerReady: map[string]bool{},
		},
		process: newFakeProcAdapter(),
	}
}

func newFakeProbeAdapterWithProcess(process *fakeProcAdapter) *fakeProbeAdapter {
	p := newFakeProbeAdapter()
	p.process = process
	return p
}

func (p *fakeProbeAdapter) setTCPReady(port uint16, ready bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.state.tcpReady[port] = ready
}

func (p *fakeProbeAdapter) setPortInUse(port uint16, inUse bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.state.portInUse[port] = inUse
}

func (p *fakeProbeAdapter) setPortHoldersUnsupported() {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.state.portHoldersUnsupported = true
}

func (p *fakeProbeAdapter) setPortProbeUnknown() {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.state.portProbeUnknown = true
}

// setPortHolder registers a squatter holding `port`: the holder pid goes into the shared fake
// process table (so `signal_pid` can kill it) and into the port's holder list (so
// `port_holders` resolves it and `port_in_use` stays true until it dies).
func (p *fakeProbeAdapter) setPortHolder(port uint16, pid int64, startIdentity, command string) {
	p.process.mu.Lock()
	p.process.state.alive[pid] = PosixProcessRecord{
		Pid: pid, Pgid: pid,
		StartIdentity:      startIdentity,
		CommandFingerprint: command,
		CommandLine:        command,
	}
	p.process.mu.Unlock()
	p.mu.Lock()
	p.state.portHolders[port] = append(p.state.portHolders[port], PortHolder{
		Pid: pid, Pgid: pid, StartIdentity: startIdentity, Command: command,
	})
	p.mu.Unlock()
}

func (p *fakeProbeAdapter) setContainerReady(name string, ready bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.state.containerReady[name] = ready
}

func (p *fakeProbeAdapter) TCP(port uint16) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.state.tcpReady[port]
}

func (p *fakeProbeAdapter) HTTP(string) bool { return false }

func (p *fakeProbeAdapter) Container(name string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.state.containerReady[name]
}

func (p *fakeProbeAdapter) Tailnet() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.state.tailnetReady
}

func (p *fakeProbeAdapter) PortInUse(port uint16) *bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.state.portProbeUnknown {
		return nil
	}
	if holders, ok := p.state.portHolders[port]; ok {
		p.process.mu.Lock()
		defer p.process.mu.Unlock()
		any := false
		for _, h := range holders {
			if _, alive := p.process.state.alive[h.Pid]; alive {
				any = true
				break
			}
		}
		return &any
	}
	v := p.state.portInUse[port]
	return &v
}

func (p *fakeProbeAdapter) PortHolders(port uint16) *[]PortHolder {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.state.portHoldersUnsupported {
		return nil
	}
	p.process.mu.Lock()
	defer p.process.mu.Unlock()
	out := []PortHolder{}
	for _, h := range p.state.portHolders[port] {
		if _, alive := p.process.state.alive[h.Pid]; alive {
			out = append(out, h)
		}
	}
	return &out
}

// Deliberately NOT implementing a real command probe — the interface's nil return is exactly the
// sharp edge under test ("no adapter configured"). `commandCapableProbeAdapter` is used for tests
// that need a real command-probe result.
func (p *fakeProbeAdapter) Command(CommandContext, *catalog.CommandSpec, *string) *bool { return nil }

type commandCapableProbeAdapter struct {
	inner    *fakeProbeAdapter
	mu       sync.Mutex
	result   *bool
	delayMs  int64
	timeline []time.Time
}

func (c *commandCapableProbeAdapter) TCP(port uint16) bool {
	if c.inner != nil {
		return c.inner.TCP(port)
	}
	return false
}

func (c *commandCapableProbeAdapter) HTTP(url string) bool {
	if c.inner != nil {
		return c.inner.HTTP(url)
	}
	return false
}

func (c *commandCapableProbeAdapter) Container(name string) bool {
	if c.inner != nil {
		return c.inner.Container(name)
	}
	return false
}

func (c *commandCapableProbeAdapter) Tailnet() bool {
	if c.inner != nil {
		return c.inner.Tailnet()
	}
	return false
}

func (c *commandCapableProbeAdapter) PortInUse(port uint16) *bool {
	if c.inner != nil {
		return c.inner.PortInUse(port)
	}
	return nil
}

func (c *commandCapableProbeAdapter) PortHolders(port uint16) *[]PortHolder {
	if c.inner != nil {
		return c.inner.PortHolders(port)
	}
	return nil
}

func (c *commandCapableProbeAdapter) Command(_ CommandContext, _ *catalog.CommandSpec, _ *string) *bool {
	c.mu.Lock()
	c.timeline = append(c.timeline, time.Now())
	delay := c.delayMs
	c.mu.Unlock()
	if delay > 0 {
		time.Sleep(time.Duration(delay) * time.Millisecond)
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.result
}

// --- Fake run_build ---

type buildStart struct {
	text string
	at   time.Time
}

type fakeRunBuild struct {
	mu                 sync.Mutex
	shouldFail         map[string]bool
	startedAt          []buildStart
	hangUntilCancelled bool
}

func newFakeRunBuild() *fakeRunBuild {
	return &fakeRunBuild{shouldFail: map[string]bool{}}
}

func (b *fakeRunBuild) failFor(shellText string) {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.shouldFail[shellText] = true
}

func (b *fakeRunBuild) timeline() []buildStart {
	b.mu.Lock()
	defer b.mu.Unlock()
	return append([]buildStart{}, b.startedAt...)
}

func (b *fakeRunBuild) Run(command *catalog.ServiceCommand, _ OnOutput, cancel <-chan struct{}) error {
	text := commandSpecText(&command.Command)
	b.mu.Lock()
	b.startedAt = append(b.startedAt, buildStart{text, time.Now()})
	hang := b.hangUntilCancelled
	b.mu.Unlock()
	if hang {
		<-cancel
		return NewError("Build cancelled")
	}
	time.Sleep(5 * time.Millisecond)
	b.mu.Lock()
	fail := b.shouldFail[text]
	b.mu.Unlock()
	if fail {
		return NewError("Build failed")
	}
	return nil
}

type noPreparation struct{}

func (noPreparation) Prepare(string, []string) error { return nil }

// --- Fake host ---

type logEntry struct {
	serviceID string
	data      string
}

type eventEntry struct {
	eventType string
	data      map[string]any
}

type fakeHost struct {
	mu          sync.Mutex
	instanceID  string
	cat         *catalog.ServiceCatalog
	states      map[string]state.ServiceLifecycleState
	logs        []logEntry
	events      []eventEntry
	bgErrors    []string
	stateWrites atomic.Uint64
}

func newFakeHost(cat catalog.ServiceCatalog) *fakeHost {
	return &fakeHost{
		instanceID: "instance-1",
		cat:        &cat,
		states:     map[string]state.ServiceLifecycleState{},
	}
}

func (h *fakeHost) stateWritesCount() uint64 { return h.stateWrites.Load() }

func (h *fakeHost) stateOf(serviceID string) *state.ServiceLifecycleState {
	h.mu.Lock()
	defer h.mu.Unlock()
	st, ok := h.states[serviceID]
	if !ok {
		return nil
	}
	cp := st
	return &cp
}

// seed seeds a persisted state directly (bypassing the supervisor) — mirrors a test fixture
// asserting behavior against a pre-existing `state.json`-style entry.
func (h *fakeHost) seed(st state.ServiceLifecycleState) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.states[st.ServiceID] = st
}

func (h *fakeHost) InstanceID() string { return h.instanceID }

func (h *fakeHost) Catalog() *catalog.ServiceCatalog { return h.cat }

func (h *fakeHost) ServiceStates() []state.ServiceLifecycleState {
	h.mu.Lock()
	defer h.mu.Unlock()
	out := make([]state.ServiceLifecycleState, 0, len(h.cat.Services))
	for _, s := range h.cat.Services {
		if st, ok := h.states[s.ID]; ok {
			out = append(out, st)
		} else {
			out = append(out, defaultStoppedState(s.ID))
		}
	}
	return out
}

func (h *fakeHost) ServiceState(serviceID string) *state.ServiceLifecycleState {
	h.mu.Lock()
	defer h.mu.Unlock()
	st, ok := h.states[serviceID]
	if !ok {
		return nil
	}
	cp := st
	return &cp
}

func (h *fakeHost) SetServiceState(next *state.ServiceLifecycleState) {
	h.stateWrites.Add(1)
	h.mu.Lock()
	defer h.mu.Unlock()
	h.states[next.ServiceID] = *next
}

func (h *fakeHost) AppendLog(serviceID, data string) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.logs = append(h.logs, logEntry{serviceID, data})
}

func (h *fakeHost) Publish(eventType string, data map[string]any) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.events = append(h.events, eventEntry{eventType, data})
}

func (h *fakeHost) RecordBackgroundError(scope, err string) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.bgErrors = append(h.bgErrors, scope+":"+err)
}

func (h *fakeHost) logLines() []logEntry {
	h.mu.Lock()
	defer h.mu.Unlock()
	return append([]logEntry{}, h.logs...)
}

func (h *fakeHost) publishedEvents() []eventEntry {
	h.mu.Lock()
	defer h.mu.Unlock()
	return append([]eventEntry{}, h.events...)
}

func defaultStoppedState(serviceID string) state.ServiceLifecycleState {
	now := "2024-01-01T00:00:00.000Z"
	return state.ServiceLifecycleState{
		ServiceID:    serviceID,
		DesiredState: state.DesiredStopped,
		ActualState:  state.ActualStopped,
		Readiness:    state.ReadinessUnknown,
		Generation:   0,
		CreatedAt:    now,
		UpdatedAt:    now,
	}
}

// --- Catalog builders ---

func commandSpecText(spec *catalog.CommandSpec) string {
	if spec == nil {
		return ""
	}
	if spec.IsArgv() {
		return strings.Join(spec.Argv, " ")
	}
	return spec.Shell
}

func displayedCommand(command *catalog.ServiceCommand) string {
	return commandSpecText(&command.Command)
}

func withArgv(service catalog.ServiceDefinition, argv []string) catalog.ServiceDefinition {
	if service.Profiles.Run.Command != nil {
		service.Profiles.Run.Command.Command = catalog.CommandSpec{Argv: argv}
	}
	return service
}

func argvVerified(id string, readiness catalog.ReadinessSpec) catalog.ServiceDefinition {
	cmd := catalog.ServiceCommand{
		Command: catalog.CommandSpec{Argv: []string{id}},
		Cwd:     ".",
	}
	return catalog.ServiceDefinition{
		ID: id,
		Profiles: catalog.ServiceProfiles{Run: catalog.ServiceRunProfile{
			CommandStatus: "verified",
			Command:       &cmd,
			Readiness:     readiness,
		}},
	}
}

func withPreparationCommand(service catalog.ServiceDefinition, prep catalog.PreparationCommand) catalog.ServiceDefinition {
	if service.Profiles.Run.Command != nil {
		p := prep
		service.Profiles.Run.PreparationCommand = &p
	}
	return service
}

func oneServiceCatalog(service catalog.ServiceDefinition) catalog.ServiceCatalog {
	return catalog.ServiceCatalog{
		Services:           []catalog.ServiceDefinition{service},
		Groups:             map[string][]string{},
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
	}
}

// --- Harness ---

type harness struct {
	supervisor *ProcessSupervisor
	host       *fakeHost
	process    *fakeProcAdapter
	probes     *fakeProbeAdapter
	runBuild   *fakeRunBuild
	clock      *fakeClock
}

func buildHarness(cat catalog.ServiceCatalog) *harness {
	return buildHarnessWithTimeout(cat, 200)
}

func buildHarnessWithTimeout(cat catalog.ServiceCatalog, readinessTimeoutMs int64) *harness {
	host := newFakeHost(cat)
	process := newFakeProcAdapter()
	probes := newFakeProbeAdapterWithProcess(process)
	runBuild := newFakeRunBuild()
	clock := newFakeClock()
	options := SupervisorOptions{
		Process:            process,
		RunBuild:           runBuild,
		Probes:             probes,
		Preparation:        noPreparation{},
		Clock:              clock,
		ReadinessTimeoutMs: readinessTimeoutMs,
		ReadinessBackoffMs: 5,
		TerminationGraceMs: 50,
		IsClosing:          func() bool { return false },
	}
	sup := NewProcessSupervisor(host, options)
	return &harness{
		supervisor: sup,
		host:       host,
		process:    process,
		probes:     probes,
		runBuild:   runBuild,
		clock:      clock,
	}
}

// --- Test helpers ---

func waitUntilActual(t interface{ Fatalf(string, ...any) }, host *fakeHost, serviceID string, expected state.ActualServiceState) {
	deadline := time.Now().Add(2 * time.Second)
	for {
		if st := host.stateOf(serviceID); st != nil && st.ActualState == expected {
			return
		}
		if time.Now().After(deadline) {
			var last state.ActualServiceState
			if st := host.stateOf(serviceID); st != nil {
				last = st.ActualState
			}
			t.Fatalf("timed out waiting for %s to become %s, last state: %s", serviceID, expected, last)
		}
		runtime.Gosched()
	}
}

func waitForPosixPid(t interface{ Fatalf(string, ...any) }, host *fakeHost, serviceID string) int64 {
	deadline := time.Now().Add(2 * time.Second)
	for {
		if st := host.stateOf(serviceID); st != nil && st.Identity != nil && !st.Identity.IsDocker() {
			return st.Identity.PidValue()
		}
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for %s to get a posix pid", serviceID)
		}
		runtime.Gosched()
	}
}

func waitDead(t interface{ Fatalf(string, ...any) }, process *fakeProcAdapter, pid int64) {
	deadline := time.Now().Add(2 * time.Second)
	for {
		if !process.isAlive(pid) {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for pid %d to die", pid)
		}
		runtime.Gosched()
	}
}

func nowPlus2s() time.Time                  { return time.Now().Add(2 * time.Second) }
func afterDeadline(d time.Time) bool        { return time.Now().After(d) }
func gosched()                              { runtime.Gosched() }
func contains(haystack, needle string) bool { return strings.Contains(haystack, needle) }

func didNotSpawn(t interface{ Fatalf(string, ...any) }, process *fakeProcAdapter) {
	process.mu.Lock()
	next := process.state.nextPid
	process.mu.Unlock()
	if next != 900_001 {
		t.Fatalf("reconcile must not start a replacement")
	}
	if len(process.pidSignals()) != 0 || len(process.signals()) != 0 {
		t.Fatalf("reconcile must not signal a port holder: %v %v", process.pidSignals(), process.signals())
	}
}
