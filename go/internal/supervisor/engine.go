// ProcessSupervisor is the start/stop/restart/readiness/adoption state machine.
// OS access goes through the adapter interfaces so tests can fake it.
// Ported from rust/crates/hearth-core/src/supervisor/engine/mod.rs — the
// ownership, tree-signalling, and "stop must actually stop something" rules
// are the same ones AGENTS.md calls out. Do not collapse the shutdown passes.
package supervisor

import (
	"regexp"
	"sync"
	"sync/atomic"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
	"github.com/ngosangns/hearth/go/internal/syncx"
)

// ActiveStates stay active so reconcile does not ignore them. Running is the
// in-flight state of a readiness:exit command.
var ActiveStates = []state.ActualServiceState{
	state.ActualQueuedStart,
	state.ActualPreparing,
	state.ActualStarting,
	state.ActualRunning,
	state.ActualRunningUnready,
	state.ActualReady,
	state.ActualStopping,
}

func isActiveState(s state.ActualServiceState) bool {
	for _, v := range ActiveStates {
		if v == s {
			return true
		}
	}
	return false
}

const (
	externalLogBacklogLines = uint64(200)
	treeSampleInterval      = 200 * time.Millisecond
	taskExitSettleMs        = int64(5_000)
	logForwardCapacity      = 1024
	logMatchWindow          = 64 * 1024
	restartDelayMs          = uint64(1_000)
)

// VerifiedProfile is an owned copy of a verified run profile.
type VerifiedProfile struct {
	Command            catalog.ServiceCommand
	Readiness          catalog.ReadinessSpec
	ReadinessTimeoutMs *uint64
	Preparation        []string
	PreparationCommand *catalog.PreparationCommand
}

type activeProc struct {
	command  catalog.ServiceCommand
	identity state.ProcessIdentity
	stopped  bool
	exited   atomic.Bool
	exitCode atomic.Int32
	lastTree *treeBox
}

type treeBox struct {
	mu      sync.Mutex
	entries []ProcessTreeEntry
}

type patchKind int

const (
	patchKeep patchKind = iota
	patchClear
	patchSet
)

type patch[T any] struct {
	kind patchKind
	val  T
}

func keep[T any]() patch[T]    { return patch[T]{} }
func clear[T any]() patch[T]   { return patch[T]{kind: patchClear} }
func setv[T any](v T) patch[T] { return patch[T]{kind: patchSet, val: v} }

func (p patch[T]) apply(prev *T) *T {
	switch p.kind {
	case patchClear:
		return nil
	case patchSet:
		v := p.val
		return &v
	default:
		return prev
	}
}

// Changes is a partial update applied on top of the previous lifecycle row.
type Changes struct {
	Desired         *state.DesiredServiceState
	OperationID     patch[string]
	Identity        patch[state.ProcessIdentity]
	ReadinessKind   patch[state.ReadinessKind]
	ReadinessDetail patch[string]
	ExitedAt        patch[string]
	ExitCode        patch[int32]
	Error           patch[string]
}

type logForwarder struct {
	ch      chan string
	dropped atomic.Uint64
}

type logMatcher struct {
	pattern *regexp.Regexp
	matched atomic.Bool
	mu      sync.Mutex
	recent  string
}

type buildAbort struct {
	ch   chan struct{}
	once sync.Once
}

func newBuildAbort() *buildAbort { return &buildAbort{ch: make(chan struct{})} }

func (b *buildAbort) cancel() { b.once.Do(func() { close(b.ch) }) }

func (b *buildAbort) Cancelled() bool {
	select {
	case <-b.ch:
		return true
	default:
		return false
	}
}

// ProcessSupervisor serializes each service on its own queue.
type ProcessSupervisor struct {
	host               Host
	options            SupervisorOptions
	active             sync.Mutex
	actives            map[string]*activeProc
	tokensMu           sync.Mutex
	tokens             map[string]uint64
	queues             *syncx.KeyedLock[string]
	buildAborts        sync.Mutex
	aborts             map[string]*buildAbort
	buildSerials       *syncx.KeyedLock[string]
	preparationSerials *syncx.KeyedLock[string]
	tailsMu            sync.Mutex
	tails              map[string]*OutputTail
	forwardersMu       sync.Mutex
	forwarders         map[string]*logForwarder
	probeLoops         sync.Mutex
	loops              map[string]uint64
	matchersMu         sync.Mutex
	matchers           map[string]*logMatcher
	noLogMatcher       map[string]bool
	restartsMu         sync.Mutex
	restarts           map[string]uint32
	composeStart       sync.Mutex
}

// NewProcessSupervisor wires a supervisor. Nil clock and IsClosing get defaults.
func NewProcessSupervisor(host Host, options SupervisorOptions) *ProcessSupervisor {
	if options.Clock == nil {
		options.Clock = SystemClock{}
	}
	if options.IsClosing == nil {
		options.IsClosing = func() bool { return false }
	}
	return &ProcessSupervisor{
		host:               host,
		options:            options,
		actives:            map[string]*activeProc{},
		tokens:             map[string]uint64{},
		queues:             syncx.NewKeyedLock[string](),
		aborts:             map[string]*buildAbort{},
		buildSerials:       syncx.NewKeyedLock[string](),
		preparationSerials: syncx.NewKeyedLock[string](),
		tails:              map[string]*OutputTail{},
		forwarders:         map[string]*logForwarder{},
		loops:              map[string]uint64{},
		matchers:           map[string]*logMatcher{},
		noLogMatcher:       map[string]bool{},
		restarts:           map[string]uint32{},
	}
}

func (s *ProcessSupervisor) withQueue(id string, fn func() error) error {
	var err error
	s.queues.Run(id, func() { err = fn() })
	return err
}

func (s *ProcessSupervisor) closing() bool { return s.options.IsClosing() }

func (s *ProcessSupervisor) now() string { return s.options.Clock.Now() }

func (s *ProcessSupervisor) nowMillis() int64 { return s.options.Clock.NowMillis() }

func (s *ProcessSupervisor) sleep(ms int64) { s.options.Clock.Sleep(ms) }

// Start is an explicit start: it resets the restart: budget. Auto-respawn
// calls startLocked directly so it does not reset its own counter.
func (s *ProcessSupervisor) Start(serviceID string, operationID *string) error {
	return s.StartWithOptions(serviceID, operationID, StartOptions{})
}

// StartWithOptions is the only start that may set KillUnowned, and only from
// a confirmed client request.
func (s *ProcessSupervisor) StartWithOptions(serviceID string, operationID *string, options StartOptions) error {
	s.restartsMu.Lock()
	delete(s.restarts, serviceID)
	s.restartsMu.Unlock()
	return s.withQueue(serviceID, func() error {
		return s.startLocked(serviceID, operationID, options)
	})
}

// Restart skips the "no identity and no stop command" refusal that Stop still
// makes. A holder that is this service is recorded first so the stop replaces
// it; a different program is not signalled.
func (s *ProcessSupervisor) Restart(serviceID string, operationID *string) error {
	s.cancel(serviceID)
	s.abortBuild(serviceID)
	s.restartsMu.Lock()
	delete(s.restarts, serviceID)
	s.restartsMu.Unlock()
	return s.withQueue(serviceID, func() error {
		fingerprints := s.duplicateFingerprints(serviceID)
		cwd := s.serviceCwd(serviceID)
		if s.restartNeedsStop(serviceID) || s.noteServiceHolder(serviceID) {
			if err := s.stopLocked(serviceID, operationID); err != nil {
				return err
			}
		}
		if err := s.reapDuplicateServiceProcesses(serviceID, fingerprints, cwd); err != nil {
			return err
		}
		return s.startLocked(serviceID, operationID, StartOptions{})
	})
}

// Stop fails when it cannot stop something. A finished readiness:exit row is
// the exception: there is no process left.
func (s *ProcessSupervisor) Stop(serviceID string, operationID *string) error {
	s.cancel(serviceID)
	s.abortBuild(serviceID)
	return s.withQueue(serviceID, func() error {
		return s.stopLocked(serviceID, operationID)
	})
}

// StartGroup starts flattened members in order and stops at the first error.
func (s *ProcessSupervisor) StartGroup(group string, operationID *string) error {
	cat := s.host.Catalog()
	members, ok := cat.Groups[group]
	if !ok {
		return Errorf("Unknown service group %s", group)
	}
	for _, id := range members {
		if err := s.Start(id, operationID); err != nil {
			return err
		}
	}
	return nil
}

// BeginShutdown cancels in-flight starts and builds. It does not stop services.
func (s *ProcessSupervisor) BeginShutdown() {
	ids := map[string]struct{}{}
	s.tokensMu.Lock()
	for id := range s.tokens {
		ids[id] = struct{}{}
	}
	s.tokensMu.Unlock()
	s.buildAborts.Lock()
	for id := range s.aborts {
		ids[id] = struct{}{}
	}
	s.buildAborts.Unlock()
	for _, st := range s.host.ServiceStates() {
		ids[st.ServiceID] = struct{}{}
	}
	for id := range ids {
		s.cancel(id)
		s.abortBuild(id)
	}
}

// Shutdown is the stop-services pass only (`hearth manager stop`). Three
// passes, in order: daemon-owned active rows, then external services that
// declare stop:, then a reap of stale POSIX identities. Do not merge them.
func (s *ProcessSupervisor) Shutdown() {
	s.BeginShutdown()
	cat := s.host.Catalog()
	daemonOwned := map[string]struct{}{}
	for _, def := range cat.Services {
		if def.Ownership == nil || *def.Ownership != catalog.OwnershipExternal {
			daemonOwned[def.ID] = struct{}{}
		}
	}
	var toStop []string
	for _, st := range s.host.ServiceStates() {
		if _, ok := daemonOwned[st.ServiceID]; !ok {
			continue
		}
		if isActiveState(st.ActualState) || (st.ActualState == state.ActualOrphaned && st.Identity != nil) {
			toStop = append(toStop, st.ServiceID)
		}
	}
	for _, id := range toStop {
		_ = s.Stop(id, nil)
	}

	var external []string
	for _, st := range s.host.ServiceStates() {
		if !isActiveState(st.ActualState) {
			continue
		}
		for _, def := range cat.Services {
			if def.ID == st.ServiceID && def.Ownership != nil && *def.Ownership == catalog.OwnershipExternal {
				external = append(external, st.ServiceID)
			}
		}
	}
	for _, id := range external {
		if err := s.stopExternalOnShutdown(id); err != nil {
			s.host.RecordBackgroundError("supervisor.shutdown", id+": "+err.Error())
		}
	}

	var reap []string
	for _, st := range s.host.ServiceStates() {
		if _, ok := daemonOwned[st.ServiceID]; ok {
			reap = append(reap, st.ServiceID)
		}
	}
	for _, id := range reap {
		id := id
		s.queues.Run(id, func() { s.reapPersistedPosixIdentity(id) })
	}
	s.DetachAllOutput()
}

func (s *ProcessSupervisor) stopExternalOnShutdown(serviceID string) error {
	s.cancel(serviceID)
	s.abortBuild(serviceID)
	return s.withQueue(serviceID, func() error {
		st := s.state(serviceID)
		if st == nil || !isActiveState(st.ActualState) {
			return nil
		}
		if st.Identity != nil {
			return s.stopLocked(serviceID, nil)
		}
		profile, err := profileFor(s.host.Catalog(), serviceID)
		if err != nil {
			return err
		}
		if profile.Command.DockerStopCommand == nil {
			return nil
		}
		return s.stopUnowned(serviceID, st, nil, true)
	})
}

// DetachAllOutput stops every log follower, including on a leave-services
// shutdown. docker logs followers must not outlive the daemon.
func (s *ProcessSupervisor) DetachAllOutput() {
	s.tailsMu.Lock()
	tails := s.tails
	s.tails = map[string]*OutputTail{}
	s.tailsMu.Unlock()
	for _, tail := range tails {
		if tail != nil {
			tail.Stop()
		}
	}
}

// Status orphans a row whose identity is definitively not ours. An unknown
// probe does not orphan. It does not run the readiness probe.
func (s *ProcessSupervisor) Status(serviceID string) error {
	return s.withQueue(serviceID, func() error {
		st := s.state(serviceID)
		if st == nil || st.Identity == nil || !isActiveState(st.ActualState) {
			return nil
		}
		if owned := s.owns(st); owned != nil && !*owned {
			s.orphan(st, nil)
		}
		return nil
	})
}

// Reconcile re-reads persisted rows, including externally-owned ones. Unknown
// from a probe leaves the row unchanged.
func (s *ProcessSupervisor) Reconcile() {
	for _, snap := range s.host.ServiceStates() {
		snap := snap
		if snap.ActualState == state.ActualExternallyOwned {
			s.queues.Run(snap.ServiceID, func() {
				st := s.state(snap.ServiceID)
				if st == nil || st.ActualState != state.ActualExternallyOwned {
					return
				}
				s.reconcileExternallyOwned(st)
			})
			continue
		}
		if snap.Identity == nil || !isActiveState(snap.ActualState) {
			continue
		}
		s.queues.Run(snap.ServiceID, func() {
			s.reconcileLive(snap)
		})
	}
}

func (s *ProcessSupervisor) reconcileLive(snap state.ServiceLifecycleState) {
	id := snap.ServiceID
	identity := snap.Identity
	switch insp := s.options.Process.Inspect(identity); insp.Kind {
	case InspectionUnknown:
		return
	case InspectionGone:
		s.markGone(id, snap)
		return
	case InspectionObserved:
		if insp.Observed == nil || !insp.Observed.Alive {
			s.markGone(id, snap)
			return
		}
	}
	if !s.identityMatchesState(&snap) {
		s.orphan(&snap, nil)
		return
	}
	switch matched := s.observedMatches(identity); {
	case matched == nil:
		return
	case !*matched:
		s.orphan(&snap, nil)
		return
	}
	updated := withManagerInstance(identity, s.host.InstanceID())
	profile, profileErr := profileFor(s.host.Catalog(), id)
	exitJob := profileErr == nil && profile.Readiness.IsExit()
	actual := state.ActualRunningUnready
	readiness := state.ReadinessNotReady
	if exitJob {
		actual = state.ActualRunning
		readiness = state.ReadinessUnknown
	}
	s.transition(id, snap.Generation, actual, readiness, Changes{
		Identity: setv(*updated),
		Error:    clear[string](),
	})
	if !s.hasLiveOutputTail(id) {
		s.attachOutput(id, outputSource(updated, true))
	}
	if profileErr != nil {
		return
	}
	token := s.currentToken(id)
	s.armAdoptedWatch(id, *updated, profile.Command, snap.Generation, token)
	_ = s.readiness(id, &profile, snap.Generation, updated, token, nil, true)
}

func (s *ProcessSupervisor) markGone(id string, snap state.ServiceLifecycleState) {
	next := state.ActualStopped
	if snap.DesiredState == state.DesiredRunning {
		next = state.ActualFailed
	}
	s.transition(id, snap.Generation, next, state.ReadinessFailed, Changes{
		Error:    setv("Managed process is no longer alive"),
		ExitedAt: setv(s.now()),
	})
}

func (s *ProcessSupervisor) reconcileExternallyOwned(st *state.ServiceLifecycleState) {
	profile, err := profileFor(s.host.Catalog(), st.ServiceID)
	if err != nil {
		return
	}
	port, ok := readinessTCPPort(&profile.Readiness)
	if !ok || s.options.Probes == nil {
		return
	}
	inUse := s.options.Probes.PortInUse(port)
	if inUse == nil {
		return
	}
	if !*inUse {
		running := st.DesiredState == state.DesiredRunning
		actual := state.ActualStopped
		readiness := state.ReadinessUnknown
		errPatch := clear[string]()
		if running {
			actual = state.ActualFailed
			readiness = state.ReadinessFailed
			errPatch = setv("Managed process is no longer alive")
		}
		s.transition(st.ServiceID, st.Generation, actual, readiness, Changes{
			Error: errPatch, Identity: clear[state.ProcessIdentity](),
		})
		return
	}
	holders := s.options.Probes.PortHolders(port)
	if holders == nil {
		return
	}
	for _, holder := range *holders {
		if s.holderIsService(&holder, &profile) {
			s.adoptPortHolder(st.ServiceID, st.Generation, &profile, &holder)
			return
		}
	}
	if len(*holders) == 0 {
		return
	}
	message := "Port " + itoa(int64(port)) + " is held by " + s.describePortHolders(port)
	if st.Error == nil || *st.Error != message {
		s.transition(st.ServiceID, st.Generation, state.ActualExternallyOwned, state.ReadinessFailed, Changes{
			Error: setv(message),
		})
	}
}

// SyncExternalServices polls ownership:external units. It does not touch
// daemon-owned services.
func (s *ProcessSupervisor) SyncExternalServices() {
	var services []catalog.ServiceDefinition
	for _, def := range s.host.Catalog().Services {
		if def.Ownership != nil && *def.Ownership == catalog.OwnershipExternal {
			services = append(services, def)
		}
	}
	for _, service := range services {
		s.active.Lock()
		_, busy := s.actives[service.ID]
		s.active.Unlock()
		if busy {
			continue
		}
		service := service
		s.queues.Run(service.ID, func() {
			s.active.Lock()
			_, busy := s.actives[service.ID]
			s.active.Unlock()
			if busy || !service.Profiles.Run.IsVerified() || service.Profiles.Run.Command == nil {
				return
			}
			st := s.state(service.ID)
			ready := s.probe(&service.Profiles.Run.Readiness, service.ID)
			var actual state.ActualServiceState
			has := st != nil
			if has {
				actual = st.ActualState
			}
			if ready && (!has || actual == state.ActualStopped || actual == state.ActualFailed) {
				var generation uint64 = 1
				if has {
					generation = st.Generation + 1
				}
				kind := readinessKindOf(&service.Profiles.Run.Readiness)
				running := state.DesiredRunning
				s.transition(service.ID, generation, state.ActualReady, state.ReadinessReady, Changes{
					ReadinessKind:   setv(kind),
					ReadinessDetail: setv("adopted from external state"),
					Desired:         &running,
					Error:           clear[string](),
					ExitCode:        clear[int32](),
					ExitedAt:        clear[string](),
					Identity:        clear[state.ProcessIdentity](),
				})
			}
			if ready {
				if name := service.Profiles.Run.Command.ContainerName; name != nil && !s.hasLiveOutputTail(service.ID) {
					tail := externalLogBacklogLines
					s.attachOutput(service.ID, ContainerSource(*name, nil, &tail))
				}
			} else if st != nil && (st.ActualState == state.ActualReady || st.ActualState == state.ActualRunningUnready) {
				stopped := state.DesiredStopped
				s.transition(service.ID, st.Generation, state.ActualStopped, state.ReadinessUnknown, Changes{
					Desired:         &stopped,
					ExitedAt:        setv(s.now()),
					Error:           clear[string](),
					ExitCode:        clear[int32](),
					Identity:        clear[state.ProcessIdentity](),
					ReadinessDetail: clear[string](),
				})
				s.detachOutput(service.ID)
			}
		})
	}
}
