package supervisor

import (
	"regexp"
	"strconv"
	"strings"
	"sync/atomic"
	"time"
	"unicode/utf8"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

type readinessOutcome int

const (
	outcomeReady readinessOutcome = iota
	outcomeSuperseded
	outcomeExited
)

func (s *ProcessSupervisor) awaitExternalTask(serviceID string, profile *VerifiedProfile, generation, token uint64, operationID *string, exited <-chan int32) error {
	stop := make(chan struct{})
	done := make(chan error, 1)
	go func() { done <- s.awaitTaskProbe(serviceID, profile, generation, token, operationID, stop) }()
	select {
	case err := <-done:
		return err
	case code, ok := <-exited:
		close(stop)
		<-done
		if !ok {
			code = -1
		}
		if code != 0 {
			message := serviceID + " task exited with code " + itoa(int64(code))
			s.fail(serviceID, generation, token, nil, message, operationID, nil, "")
			return NewError(message)
		}
		return s.probeSettledTask(serviceID, profile, generation, token, operationID)
	}
}

func (s *ProcessSupervisor) awaitTaskProbe(serviceID string, profile *VerifiedProfile, generation, token uint64, operationID *string, stop <-chan struct{}) error {
	timeout := s.options.ReadinessTimeoutMs
	if profile.ReadinessTimeoutMs != nil {
		timeout = int64(*profile.ReadinessTimeoutMs)
	}
	deadline := s.nowMillis() + timeout
	for s.nowMillis() <= deadline {
		if stopped(stop) || !s.valid(serviceID, generation, token) || s.closing() {
			return nil
		}
		if s.probe(&profile.Readiness, serviceID) {
			if stopped(stop) {
				return nil
			}
			kind := readinessKindOf(&profile.Readiness)
			s.transitionIfCurrent(serviceID, generation, token, state.ActualReady, state.ReadinessReady, Changes{
				ReadinessKind: setv(kind), ReadinessDetail: setv("readiness verified"), Error: clear[string](),
			})
			return nil
		}
		if s.sleepUnless(s.options.ReadinessBackoffMs, stop) {
			return nil
		}
	}
	if stopped(stop) {
		return nil
	}
	message := "Readiness timed out"
	kind := readinessKindOf(&profile.Readiness)
	detail := "Readiness " + string(kind) + " probe timed out after " + itoa(timeout) + "ms"
	s.fail(serviceID, generation, token, nil, message, operationID, &kind, detail)
	return NewError(message)
}

func stopped(stop <-chan struct{}) bool {
	if stop == nil {
		return false
	}
	select {
	case <-stop:
		return true
	default:
		return false
	}
}

func (s *ProcessSupervisor) sleepUnless(ms int64, stop <-chan struct{}) bool {
	if stop == nil {
		s.sleep(ms)
		return false
	}
	done := make(chan struct{})
	go func() {
		s.sleep(ms)
		close(done)
	}()
	select {
	case <-stop:
		return true
	case <-done:
		return false
	}
}

func (s *ProcessSupervisor) probeSettledTask(serviceID string, profile *VerifiedProfile, generation, token uint64, operationID *string) error {
	budget := taskExitSettleMs
	if t := s.readinessTimeoutMs(serviceID); t < budget {
		budget = t
	}
	if budget < 0 {
		budget = 0
	}
	deadline := s.nowMillis() + budget
	for {
		if !s.valid(serviceID, generation, token) || s.closing() {
			return nil
		}
		if s.probe(&profile.Readiness, serviceID) {
			kind := readinessKindOf(&profile.Readiness)
			s.transitionIfCurrent(serviceID, generation, token, state.ActualReady, state.ReadinessReady, Changes{
				ReadinessKind: setv(kind), ReadinessDetail: setv("readiness verified"), Error: clear[string](),
			})
			return nil
		}
		if s.nowMillis() >= deadline {
			break
		}
		s.sleep(s.options.ReadinessBackoffMs)
	}
	message := serviceID + " task finished but the service is not ready"
	s.fail(serviceID, generation, token, nil, message, operationID, nil, "")
	return NewError(message)
}

func (s *ProcessSupervisor) readiness(serviceID string, profile *VerifiedProfile, generation uint64, identity *state.ProcessIdentity, token uint64, operationID *string, adopted bool) error {
	if profile.Readiness.IsExit() {
		outcome, message := s.awaitExit(serviceID, generation, identity, token, adopted)
		switch outcome {
		case outcomeReady, outcomeSuperseded:
			return nil
		}
		st := s.state(serviceID)
		settled := st != nil && (st.ActualState == state.ActualFailed || st.ActualState == state.ActualSucceeded || st.ActualState == state.ActualStopped)
		if !settled {
			s.fail(serviceID, generation, token, identity, message, operationID, nil, "")
		}
		return NewError(message)
	}
	if profile.Readiness.IsProcess() {
		return s.settleProcessLiveness(serviceID, generation, identity, token, operationID)
	}
	return s.beginContinuousProbe(serviceID, profile, generation, identity, token, operationID, adopted)
}

func (s *ProcessSupervisor) settleProcessLiveness(serviceID string, generation uint64, identity *state.ProcessIdentity, token uint64, operationID *string) error {
	if !s.valid(serviceID, generation, token) || s.closing() {
		return nil
	}
	if identity != nil && s.ownsIdentity(identity) != nil && !*s.ownsIdentity(identity) {
		if !s.valid(serviceID, generation, token) {
			return nil
		}
		message := "Process exited before liveness check"
		s.fail(serviceID, generation, token, identity, message, operationID, nil, "")
		return NewError(message)
	}
	var idPatch patch[state.ProcessIdentity]
	if identity != nil {
		idPatch = setv(*identity)
	}
	kind := state.ReadinessKindProcess
	s.transitionIfCurrent(serviceID, generation, token, state.ActualRunningUnready, state.ReadinessNotReady, Changes{
		Identity: idPatch, ReadinessKind: setv(kind), ReadinessDetail: setv("process-liveness-only"),
	})
	return nil
}

func (s *ProcessSupervisor) beginContinuousProbe(serviceID string, profile *VerifiedProfile, generation uint64, identity *state.ProcessIdentity, token uint64, operationID *string, adopted bool) error {
	if !s.valid(serviceID, generation, token) || s.closing() {
		return nil
	}
	if identity != nil && !s.stillRunning(serviceID, identity, adopted) {
		if !s.valid(serviceID, generation, token) {
			return nil
		}
		message := "Process exited before readiness"
		s.fail(serviceID, generation, token, identity, message, operationID, nil, "")
		return NewError(message)
	}
	passed := s.probe(&profile.Readiness, serviceID)
	if !s.valid(serviceID, generation, token) || s.closing() {
		return nil
	}
	if identity != nil && !s.stillRunning(serviceID, identity, adopted) {
		if !s.valid(serviceID, generation, token) {
			return nil
		}
		message := "Process exited before readiness"
		s.fail(serviceID, generation, token, identity, message, operationID, nil, "")
		return NewError(message)
	}
	kind := readinessKindOf(&profile.Readiness)
	if passed {
		detail := "readiness verified"
		if adopted {
			detail = "adopted readiness verified"
		}
		s.transitionIfCurrent(serviceID, generation, token, state.ActualReady, state.ReadinessReady, Changes{
			ReadinessKind: setv(kind), ReadinessDetail: setv(detail), Error: clear[string](),
		})
	} else {
		s.transitionIfCurrent(serviceID, generation, token, state.ActualRunningUnready, state.ReadinessNotReady, Changes{
			ReadinessKind: setv(kind), ReadinessDetail: setv("Readiness probe is currently unavailable"),
		})
	}
	s.spawnContinuousProbe(serviceID, profile, generation, identity, token, adopted)
	return nil
}

func (s *ProcessSupervisor) spawnContinuousProbe(serviceID string, profile *VerifiedProfile, generation uint64, identity *state.ProcessIdentity, token uint64, adopted bool) {
	if profile.Readiness.IsProcess() || profile.Readiness.IsExit() {
		return
	}
	s.probeLoops.Lock()
	if s.loops[serviceID] == token && token != 0 {
		// A second arm for the same start must not probe twice. Token 0 is unused;
		// cancel() starts at 1. A missing entry is 0, which must not match a real token.
		if _, ok := s.loops[serviceID]; ok {
			s.probeLoops.Unlock()
			return
		}
	}
	s.loops[serviceID] = token
	s.probeLoops.Unlock()
	readiness := profile.Readiness
	go func() {
		defer func() {
			s.probeLoops.Lock()
			if s.loops[serviceID] == token {
				delete(s.loops, serviceID)
			}
			s.probeLoops.Unlock()
		}()
		for {
			s.sleep(s.options.ReadinessBackoffMs)
			if s.continuousProbeStopped(serviceID, generation, token) {
				return
			}
			if identity != nil && !s.stillRunning(serviceID, identity, adopted) {
				return
			}
			passed := s.probe(&readiness, serviceID)
			stop := false
			s.queues.Run(serviceID, func() {
				if s.continuousProbeStopped(serviceID, generation, token) || (identity != nil && !s.stillRunning(serviceID, identity, adopted)) {
					stop = true
					return
				}
				st := s.state(serviceID)
				if st == nil || (st.ActualState != state.ActualReady && st.ActualState != state.ActualRunningUnready) {
					stop = true
					return
				}
				kind := readinessKindOf(&readiness)
				if passed && st.ActualState != state.ActualReady {
					s.transition(serviceID, generation, state.ActualReady, state.ReadinessReady, Changes{
						ReadinessKind: setv(kind), ReadinessDetail: setv("readiness verified"), Error: clear[string](),
					})
				} else if !passed && st.ActualState != state.ActualRunningUnready {
					s.transition(serviceID, generation, state.ActualRunningUnready, state.ReadinessNotReady, Changes{
						ReadinessKind: setv(kind), ReadinessDetail: setv("Readiness probe is currently unavailable"),
					})
				}
			})
			if stop {
				return
			}
		}
	}()
}

func (s *ProcessSupervisor) continuousProbeStopped(serviceID string, generation, token uint64) bool {
	return !s.valid(serviceID, generation, token) || s.closing()
}

func (s *ProcessSupervisor) awaitExit(serviceID string, generation uint64, identity *state.ProcessIdentity, token uint64, adopted bool) (readinessOutcome, string) {
	if !s.valid(serviceID, generation, token) {
		return outcomeSuperseded, ""
	}
	var idPatch patch[state.ProcessIdentity]
	if identity != nil {
		idPatch = setv(*identity)
	}
	kind := state.ReadinessKindExit
	s.transitionIfCurrent(serviceID, generation, token, state.ActualRunning, state.ReadinessUnknown, Changes{
		Identity: idPatch, ReadinessKind: setv(kind), ReadinessDetail: clear[string](),
	})
	for {
		if !s.valid(serviceID, generation, token) {
			return outcomeSuperseded, ""
		}
		if !adopted {
			if code, ok := s.observedExitCode(serviceID, generation); ok {
				return s.finishExit(serviceID, generation, token, code)
			}
		} else if identity != nil && s.ownsIdentity(identity) != nil && !*s.ownsIdentity(identity) {
			message := "Command exited while the daemon was down; exit code unknown"
			stopped := state.DesiredStopped
			s.transitionIfCurrent(serviceID, generation, token, state.ActualFailed, state.ReadinessFailed, Changes{
				Desired: &stopped, Error: setv(message), Identity: clear[state.ProcessIdentity](),
				ExitedAt: setv(s.now()), ReadinessKind: setv(kind), ReadinessDetail: setv(message),
			})
			return outcomeExited, message
		}
		s.sleep(s.options.ReadinessBackoffMs)
	}
}

func (s *ProcessSupervisor) observedExitCode(serviceID string, generation uint64) (int32, bool) {
	s.active.Lock()
	defer s.active.Unlock()
	a := s.actives[serviceID]
	if a == nil || a.identity.Generation != generation || !a.exited.Load() {
		return 0, false
	}
	return a.exitCode.Load(), true
}

func (s *ProcessSupervisor) finishExit(serviceID string, generation, token uint64, code int32) (readinessOutcome, string) {
	if !s.valid(serviceID, generation, token) {
		return outcomeSuperseded, ""
	}
	actual := state.ActualSucceeded
	readiness := state.ReadinessUnknown
	message := "exited 0"
	errPatch := clear[string]()
	if code != 0 {
		actual = state.ActualFailed
		readiness = state.ReadinessFailed
		message = "Process exited with code " + itoa(int64(code))
		errPatch = setv(message)
	}
	stopped := state.DesiredStopped
	kind := state.ReadinessKindExit
	s.transitionIfCurrent(serviceID, generation, token, actual, readiness, Changes{
		Desired: &stopped, ExitCode: setv(code), ExitedAt: setv(s.now()), Error: errPatch,
		Identity: clear[state.ProcessIdentity](), ReadinessKind: setv(kind), ReadinessDetail: setv(message),
	})
	if st := s.state(serviceID); st == nil || st.ActualState != actual {
		return outcomeSuperseded, message
	}
	s.active.Lock()
	if a := s.actives[serviceID]; a != nil && a.identity.Generation == generation {
		delete(s.actives, serviceID)
	}
	s.active.Unlock()
	s.detachOutput(serviceID)
	if code == 0 {
		return outcomeReady, message
	}
	return outcomeExited, message
}

func (s *ProcessSupervisor) exitCommandIsIdle(serviceID string, st *state.ServiceLifecycleState) (bool, error) {
	profile, err := profileFor(s.host.Catalog(), serviceID)
	if err != nil || !profile.Readiness.IsExit() {
		return false, nil
	}
	if st.ActualState != state.ActualSucceeded && st.ActualState != state.ActualFailed {
		return false, nil
	}
	if st.Identity == nil {
		return true, nil
	}
	switch owned := s.ownsIdentity(st.Identity); {
	case owned != nil && *owned:
		return false, nil
	case owned == nil:
		return false, Errorf("%s cannot be stopped: the process's liveness could not be verified", serviceID)
	}
	insp := s.options.Process.Inspect(st.Identity)
	if insp.Kind == InspectionObserved && insp.Observed != nil && insp.Observed.Alive {
		return false, nil
	}
	if insp.Kind == InspectionUnknown {
		return false, Errorf("%s cannot be stopped: the process's liveness could not be verified", serviceID)
	}
	return true, nil
}

func (s *ProcessSupervisor) fail(serviceID string, generation, token uint64, identity *state.ProcessIdentity, message string, operationID *string, kind *state.ReadinessKind, detail string) {
	if !s.valid(serviceID, generation, token) {
		return
	}
	profile, err := profileFor(s.host.Catalog(), serviceID)
	exitCommand := err == nil && profile.Readiness.IsExit()
	var desired *state.DesiredServiceState
	if exitCommand {
		stopped := state.DesiredStopped
		desired = &stopped
	}
	var idPatch patch[state.ProcessIdentity]
	if identity != nil {
		idPatch = setv(*identity)
	}
	kindPatch := keep[state.ReadinessKind]()
	detailPatch := keep[string]()
	if kind != nil {
		kindPatch = setv(*kind)
		detailPatch = setv(detail)
	}
	s.transitionIfCurrent(serviceID, generation, token, state.ActualFailed, state.ReadinessFailed, Changes{
		Desired: desired, Identity: idPatch, Error: setv(message), OperationID: operationPatch(operationID),
		ReadinessKind: kindPatch, ReadinessDetail: detailPatch,
	})
	s.cancel(serviceID)
	s.active.Lock()
	a := s.actives[serviceID]
	var activeGen uint64
	has := a != nil
	if has {
		activeGen = a.identity.Generation
	}
	s.active.Unlock()
	if has && activeGen == generation {
		s.finalize(serviceID, true)
	}
}

func (s *ProcessSupervisor) onExit(serviceID string, generation, token uint64, code int32) {
	s.queues.Run(serviceID, func() {
		if !s.valid(serviceID, generation, token) {
			return
		}
		st := s.state(serviceID)
		if st == nil {
			return
		}
		s.active.Lock()
		a := s.actives[serviceID]
		activeMatches := a != nil && !a.stopped
		s.active.Unlock()
		if st.Generation != generation || !activeMatches {
			return
		}
		switch st.ActualState {
		case state.ActualSucceeded, state.ActualStopped, state.ActualStopping, state.ActualFailed:
			s.active.Lock()
			if a := s.actives[serviceID]; a != nil && a.identity.Generation == generation {
				delete(s.actives, serviceID)
			}
			s.active.Unlock()
			return
		}
		s.restartsMu.Lock()
		s.restarts[serviceID]++
		attempt := s.restarts[serviceID]
		s.restartsMu.Unlock()
		definition := definitionFor(s.host.Catalog(), serviceID)
		var policy *catalog.ServiceRestartPolicy
		if definition != nil {
			policy = definition.Restart
		}
		delay := restartDelayMs
		if policy != nil && policy.DelayMs != nil {
			delay = *policy.DelayMs
		}
		will := st.DesiredState == state.DesiredRunning && !s.closing() && policy != nil &&
			(policy.On == catalog.RestartAlways || (policy.On == catalog.RestartOnFailure && code != 0)) &&
			(policy.MaxRestarts == nil || attempt <= *policy.MaxRestarts)
		message := "Process exited with code " + itoa(int64(code))
		if will {
			message = message + "; restarting (attempt " + itoa(int64(attempt)) + ")"
		}
		s.transitionIfCurrent(serviceID, generation, token, state.ActualFailed, state.ReadinessFailed, Changes{
			ExitCode: setv(code), ExitedAt: setv(s.now()), Error: setv(message),
		})
		s.active.Lock()
		if a := s.actives[serviceID]; a != nil && a.identity.Generation == generation {
			delete(s.actives, serviceID)
		}
		s.active.Unlock()
		if will {
			go func() {
				s.sleep(int64(delay))
				s.queues.Run(serviceID, func() {
					st := s.state(serviceID)
					if st == nil || st.ActualState != state.ActualFailed || st.DesiredState != state.DesiredRunning || s.closing() {
						return
					}
					_ = s.startLocked(serviceID, nil, StartOptions{})
				})
			}()
		}
	})
}

func (s *ProcessSupervisor) finalize(serviceID string, terminate bool) {
	s.active.Lock()
	a := s.actives[serviceID]
	delete(s.actives, serviceID)
	s.active.Unlock()
	if a == nil {
		return
	}
	identity := cloneIdentity(&a.identity)
	owned := s.ownsIdentity(identity)
	if terminate && (owned == nil || *owned) {
		s.reportTerminateFailure(identity.ServiceID, s.terminate(identity, &a.command))
	}
}

func (s *ProcessSupervisor) terminate(identity *state.ProcessIdentity, command *catalog.ServiceCommand) error {
	switch owned := s.ownsIdentity(identity); {
	case owned != nil && !*owned:
		return nil
	case owned == nil:
		return NewError("the process's liveness could not be verified; refusing to report it stopped")
	}
	if identity.IsDocker() {
		supported, err := s.options.Process.StopContainer(command, func(string) {})
		s.detachOutput(identity.ServiceID)
		if !supported {
			return NewError("Docker container stop is unavailable")
		}
		return asSupervisor(err)
	}
	s.detachOutput(identity.ServiceID)
	known := s.knownDescendants(identity.ServiceID, identity.PidValue())
	return s.terminatePosixTree(identity.PidValue(), identity.StartIdentityValue(), identity.PgidValue(), known)
}

func (s *ProcessSupervisor) reportTerminateFailure(serviceID string, err error) {
	if err != nil {
		s.host.RecordBackgroundError("supervisor.terminate", serviceID+": "+err.Error())
	}
}

func (s *ProcessSupervisor) processTree(pid int64, startIdentity string) ProcessTreeSnapshot {
	return s.options.Process.ProcessTree(pid, startIdentity)
}

func (s *ProcessSupervisor) processTreeAlive(tree []ProcessTreeEntry) (alive bool, ok bool) {
	if len(tree) == 0 {
		return false, true
	}
	live := s.options.Process.LiveStartIdentities()
	if live == nil {
		return false, false
	}
	return ProcessTreeAlive(tree, live), true
}

func (s *ProcessSupervisor) signalVerifiedMembers(tree []ProcessTreeEntry, leaderPgid int64, signal ProcessSignal, includeLeader bool) {
	alive := s.options.Process.LiveStartIdentities()
	if alive == nil {
		return
	}
	if includeLeader {
		leaderLive := false
		for _, member := range tree {
			if member.Pgid == leaderPgid && alive[member.Pid] == member.StartIdentity {
				leaderLive = true
				break
			}
		}
		if leaderLive {
			s.options.Process.SignalGroup(leaderPgid, signal)
		}
	}
	for pgid, members := range SecondaryProcessGroups(tree, leaderPgid) {
		for _, member := range members {
			if alive[member.Pid] == member.StartIdentity {
				s.options.Process.SignalGroup(pgid, signal)
				break
			}
		}
	}
}

func (s *ProcessSupervisor) terminatePosixTree(pid int64, startIdentity string, pgid int64, known []ProcessTreeEntry) error {
	snap := s.processTree(pid, startIdentity)
	switch snap.Kind {
	case SnapshotUnknown:
		return NewError("the process tree could not be read; refusing to report it stopped")
	case SnapshotAbsent:
		return nil
	}
	tree := append([]ProcessTreeEntry{}, snap.Entries...)
	for _, entry := range known {
		found := false
		for _, have := range tree {
			if have.Pid == entry.Pid && have.StartIdentity == entry.StartIdentity {
				found = true
				break
			}
		}
		if !found {
			tree = append(tree, entry)
		}
	}
	s.signalVerifiedMembers(tree, pgid, SignalTerm, true)
	deadline := s.nowMillis() + s.options.TerminationGraceMs
	for s.nowMillis() < deadline {
		alive, ok := s.processTreeAlive(tree)
		if !ok {
			return NewError("the process's liveness could not be verified; refusing to report it stopped")
		}
		if !alive {
			return nil
		}
		s.sleep(s.options.ReadinessBackoffMs)
	}
	alive, ok := s.processTreeAlive(tree)
	if !ok {
		return NewError("the process's liveness could not be verified; refusing to report it stopped")
	}
	if !alive {
		return nil
	}
	s.signalVerifiedMembers(tree, pgid, SignalKill, true)
	return nil
}

func (s *ProcessSupervisor) reapSecondaryGroups(tree []ProcessTreeEntry, leaderPgid int64) {
	if len(tree) == 0 {
		return
	}
	if alive, ok := s.processTreeAlive(tree); !ok || !alive {
		return
	}
	s.signalVerifiedMembers(tree, leaderPgid, SignalTerm, true)
	deadline := s.nowMillis() + s.options.TerminationGraceMs
	for s.nowMillis() < deadline {
		alive, ok := s.processTreeAlive(tree)
		if !ok || !alive {
			return
		}
		s.sleep(s.options.ReadinessBackoffMs)
	}
	if alive, ok := s.processTreeAlive(tree); ok && alive {
		s.signalVerifiedMembers(tree, leaderPgid, SignalKill, true)
	}
}

func (s *ProcessSupervisor) recordTreeSample(box *treeBox, fresh []ProcessTreeEntry) {
	box.mu.Lock()
	previous := append([]ProcessTreeEntry{}, box.entries...)
	box.mu.Unlock()
	var merged []ProcessTreeEntry
	if HasDepartedMembers(previous, fresh) {
		merged = MergeKnownDescendants(previous, fresh, s.options.Process.LiveStartIdentities())
	} else {
		merged = fresh
	}
	box.mu.Lock()
	box.entries = merged
	box.mu.Unlock()
}

func (s *ProcessSupervisor) knownDescendants(serviceID string, pid int64) []ProcessTreeEntry {
	s.active.Lock()
	defer s.active.Unlock()
	a := s.actives[serviceID]
	if a == nil || a.identity.IsDocker() || a.identity.PidValue() != pid || a.lastTree == nil {
		return nil
	}
	a.lastTree.mu.Lock()
	defer a.lastTree.mu.Unlock()
	return append([]ProcessTreeEntry{}, a.lastTree.entries...)
}

func (s *ProcessSupervisor) insertActive(serviceID string, command catalog.ServiceCommand, identity state.ProcessIdentity) *activeProc {
	a := &activeProc{command: command, identity: identity, lastTree: &treeBox{}}
	s.active.Lock()
	s.actives[serviceID] = a
	s.active.Unlock()
	return a
}

func (s *ProcessSupervisor) trackSpawned(serviceID string, command catalog.ServiceCommand, identity state.ProcessIdentity, generation, token uint64, exited <-chan int32) {
	a := s.insertActive(serviceID, command, identity)
	go func() {
		for {
			if a.exited.Load() {
				return
			}
			s.active.Lock()
			cur := s.actives[serviceID]
			still := cur != nil && !cur.stopped && cur.lastTree == a.lastTree
			s.active.Unlock()
			if !still {
				return
			}
			if !identity.IsDocker() {
				if snap := s.options.Process.ProcessTree(identity.PidValue(), identity.StartIdentityValue()); snap.Kind == SnapshotPresent {
					s.recordTreeSample(a.lastTree, snap.Entries)
				}
			}
			time.Sleep(treeSampleInterval)
		}
	}()
	go func() {
		code, ok := <-exited
		if !ok {
			code = -1
		}
		a.lastTree.mu.Lock()
		tree := append([]ProcessTreeEntry{}, a.lastTree.entries...)
		a.lastTree.mu.Unlock()
		a.exitCode.Store(code)
		a.exited.Store(true)
		if !identity.IsDocker() {
			s.reapSecondaryGroups(tree, identity.PgidValue())
		}
		s.onExit(serviceID, generation, token, code)
	}()
}

func (s *ProcessSupervisor) armAdoptedWatch(serviceID string, identity state.ProcessIdentity, command catalog.ServiceCommand, generation, token uint64) {
	s.active.Lock()
	_, exists := s.actives[serviceID]
	s.active.Unlock()
	if exists {
		return
	}
	a := s.insertActive(serviceID, command, identity)
	go func() {
		for {
			if a.exited.Load() {
				return
			}
			s.active.Lock()
			cur := s.actives[serviceID]
			still := cur != nil && !cur.stopped && cur.lastTree == a.lastTree
			s.active.Unlock()
			if !still {
				return
			}
			switch insp := s.options.Process.Inspect(&identity); insp.Kind {
			case InspectionUnknown:
			case InspectionGone:
				s.finishAdopted(serviceID, identity, a, generation, token)
				return
			case InspectionObserved:
				if insp.Observed == nil || !insp.Observed.Alive {
					s.finishAdopted(serviceID, identity, a, generation, token)
					return
				}
				if !identity.IsDocker() {
					if snap := s.options.Process.ProcessTree(identity.PidValue(), identity.StartIdentityValue()); snap.Kind == SnapshotPresent {
						s.recordTreeSample(a.lastTree, snap.Entries)
					}
				}
			}
			time.Sleep(treeSampleInterval)
		}
	}()
}

func (s *ProcessSupervisor) finishAdopted(serviceID string, identity state.ProcessIdentity, a *activeProc, generation, token uint64) {
	a.lastTree.mu.Lock()
	tree := append([]ProcessTreeEntry{}, a.lastTree.entries...)
	a.lastTree.mu.Unlock()
	a.exitCode.Store(-1)
	a.exited.Store(true)
	if !identity.IsDocker() {
		s.reapSecondaryGroups(tree, identity.PgidValue())
	}
	s.onExit(serviceID, generation, token, -1)
}

func (s *ProcessSupervisor) build(serviceID string, definition *catalog.ServiceDefinition, generation, token uint64, operationID *string) error {
	if definition.Profiles.Build == nil {
		return nil
	}
	build := definition.Profiles.Build
	abort := newBuildAbort()
	s.buildAborts.Lock()
	s.aborts[serviceID] = abort
	s.buildAborts.Unlock()
	defer func() {
		s.buildAborts.Lock()
		delete(s.aborts, serviceID)
		s.buildAborts.Unlock()
	}()
	timeout := uint64(15 * 60_000)
	if build.TimeoutMs != nil {
		timeout = *build.TimeoutMs
	}
	var timedOut atomic.Bool
	timer := time.AfterFunc(time.Duration(timeout)*time.Millisecond, func() {
		timedOut.Store(true)
		abort.cancel()
	})
	defer timer.Stop()
	onOutput := func(data string) {
		if s.valid(serviceID, generation, token) {
			s.appendOutput(serviceID, data)
		}
	}
	var err error
	if s.options.RunBuild == nil {
		err = NewError("Build failed")
	} else if build.SerializationKey != nil {
		key := *build.SerializationKey
		cmd := build.Command
		s.buildSerials.Run(key, func() {
			if abort.Cancelled() {
				err = NewError("Build cancelled")
				return
			}
			err = s.options.RunBuild.Run(&cmd, onOutput, abort.ch)
		})
	} else {
		err = s.options.RunBuild.Run(&build.Command, onOutput, abort.ch)
	}
	if err == nil {
		return nil
	}
	if !s.valid(serviceID, generation, token) || s.closing() {
		return nil
	}
	message := "Build failed"
	if timedOut.Load() {
		message = "Build timed out"
	} else if abort.Cancelled() {
		message = "Build cancelled"
	}
	s.transitionIfCurrent(serviceID, generation, token, state.ActualFailed, state.ReadinessFailed, Changes{
		Error: setv(message), OperationID: operationPatch(operationID),
	})
	return asSupervisor(err)
}

func (s *ProcessSupervisor) serializedPreparation(key string, command *catalog.CommandSpec, cwd *string) *bool {
	var ok *bool
	s.preparationSerials.Run(key, func() {
		if s.options.Probes != nil {
			ok = s.options.Probes.Command(nil, command, cwd)
		}
	})
	return ok
}

func (s *ProcessSupervisor) abortBuild(serviceID string) {
	s.buildAborts.Lock()
	abort := s.aborts[serviceID]
	s.buildAborts.Unlock()
	if abort != nil {
		abort.cancel()
	}
}

func (s *ProcessSupervisor) probe(readiness *catalog.ReadinessSpec, serviceID string) bool {
	if readiness == nil || s.options.Probes == nil {
		return false
	}
	switch readiness.Kind {
	case "tcp":
		return readiness.Port != nil && s.options.Probes.TCP(*readiness.Port)
	case "http":
		return s.options.Probes.HTTP(readiness.URL)
	case "tailnet":
		return s.options.Probes.Tailnet()
	case "command":
		return s.probeCommand(serviceID, readiness.Command, readiness.Cwd)
	case "container":
		profile, err := profileFor(s.host.Catalog(), serviceID)
		if err != nil || profile.Command.ContainerName == nil {
			return false
		}
		return s.options.Probes.Container(*profile.Command.ContainerName)
	case "log":
		s.matchersMu.Lock()
		m := s.matchers[serviceID]
		s.matchersMu.Unlock()
		return m != nil && m.matched.Load()
	default:
		return false
	}
}

func (s *ProcessSupervisor) probeCommand(serviceID string, command *catalog.CommandSpec, cwd *string) bool {
	if command == nil {
		return false
	}
	budget := s.readinessTimeoutMs(serviceID)
	if budget < 1 {
		budget = 1
	}
	cancel := make(chan struct{})
	type result struct{ v *bool }
	ch := make(chan result, 1)
	go func() {
		ch <- result{s.options.Probes.Command(doneCtx{cancel}, command, cwd)}
	}()
	timer := time.NewTimer(time.Duration(budget) * time.Millisecond)
	defer timer.Stop()
	select {
	case r := <-ch:
		return r.v != nil && *r.v
	case <-timer.C:
		close(cancel)
		return false
	}
}

type doneCtx struct{ ch <-chan struct{} }

func (d doneCtx) Done() <-chan struct{} { return d.ch }

func (s *ProcessSupervisor) readinessTimeoutMs(serviceID string) int64 {
	profile, err := profileFor(s.host.Catalog(), serviceID)
	if err == nil && profile.ReadinessTimeoutMs != nil {
		return int64(*profile.ReadinessTimeoutMs)
	}
	return s.options.ReadinessTimeoutMs
}

func (s *ProcessSupervisor) attachOutput(serviceID string, source OutputSource) {
	s.detachOutput(serviceID)
	tail := s.options.Process.AttachOutput(serviceID, source, func(data string) {
		s.appendOutput(serviceID, data)
	})
	if tail == nil {
		return
	}
	s.tailsMu.Lock()
	s.tails[serviceID] = tail
	s.tailsMu.Unlock()
}

func (s *ProcessSupervisor) detachOutput(serviceID string) {
	s.tailsMu.Lock()
	tail := s.tails[serviceID]
	delete(s.tails, serviceID)
	s.tailsMu.Unlock()
	s.matchersMu.Lock()
	delete(s.matchers, serviceID)
	delete(s.noLogMatcher, serviceID)
	s.matchersMu.Unlock()
	if tail != nil {
		tail.Stop()
	}
}

func (s *ProcessSupervisor) hasLiveOutputTail(serviceID string) bool {
	s.tailsMu.Lock()
	tail := s.tails[serviceID]
	s.tailsMu.Unlock()
	return tail != nil && !tail.IsDone()
}

func (s *ProcessSupervisor) appendOutput(serviceID, data string) {
	s.forwardersMu.Lock()
	f := s.forwarders[serviceID]
	if f == nil {
		f = s.spawnLogForwarder(serviceID)
		s.forwarders[serviceID] = f
	}
	s.forwardersMu.Unlock()
	select {
	case f.ch <- data:
	default:
		f.dropped.Add(1)
	}
	s.feedLogMatcher(serviceID, data)
}

func (s *ProcessSupervisor) spawnLogForwarder(serviceID string) *logForwarder {
	f := &logForwarder{ch: make(chan string, logForwardCapacity)}
	go func() {
		for chunk := range f.ch {
			if missed := f.dropped.Swap(0); missed > 0 {
				s.host.AppendLog(serviceID, "[hearth: "+itoa(int64(missed))+" log chunks dropped — output outpaced the log store]\n")
			}
			s.host.AppendLog(serviceID, chunk)
		}
	}()
	return f
}

func (s *ProcessSupervisor) feedLogMatcher(serviceID, data string) {
	s.matchersMu.Lock()
	if s.noLogMatcher[serviceID] {
		s.matchersMu.Unlock()
		return
	}
	m := s.matchers[serviceID]
	if m == nil {
		profile, err := profileFor(s.host.Catalog(), serviceID)
		if err != nil || profile.Readiness.Kind != "log" || profile.Readiness.Pattern == "" {
			s.noLogMatcher[serviceID] = true
			s.matchersMu.Unlock()
			return
		}
		re, compErr := regexp.Compile(profile.Readiness.Pattern)
		if compErr != nil {
			s.noLogMatcher[serviceID] = true
			s.matchersMu.Unlock()
			s.host.RecordBackgroundError(serviceID, "readiness.pattern does not compile: "+compErr.Error())
			return
		}
		m = &logMatcher{pattern: re}
		s.matchers[serviceID] = m
	}
	s.matchersMu.Unlock()
	if m.matched.Load() {
		return
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	m.recent += data
	if m.pattern.MatchString(m.recent) {
		m.matched.Store(true)
		m.recent = ""
		return
	}
	if len(m.recent) > logMatchWindow {
		keepFrom := len(m.recent) - logMatchWindow
		for keepFrom < len(m.recent) && !utf8.RuneStart(m.recent[keepFrom]) {
			keepFrom++
		}
		m.recent = m.recent[keepFrom:]
	}
}

func (s *ProcessSupervisor) state(serviceID string) *state.ServiceLifecycleState {
	st := s.host.ServiceState(serviceID)
	if st == nil {
		return nil
	}
	cp := *st
	cp.Identity = cloneIdentity(st.Identity)
	return &cp
}

func (s *ProcessSupervisor) currentToken(serviceID string) uint64 {
	s.tokensMu.Lock()
	defer s.tokensMu.Unlock()
	return s.tokens[serviceID]
}

func (s *ProcessSupervisor) cancel(serviceID string) uint64 {
	s.tokensMu.Lock()
	defer s.tokensMu.Unlock()
	next := s.tokens[serviceID] + 1
	s.tokens[serviceID] = next
	return next
}

func (s *ProcessSupervisor) valid(serviceID string, generation, token uint64) bool {
	if s.currentToken(serviceID) != token {
		return false
	}
	st := s.state(serviceID)
	return st != nil && st.Generation == generation
}

func (s *ProcessSupervisor) identityMatchesState(st *state.ServiceLifecycleState) bool {
	return st != nil && st.Identity != nil && st.Identity.ServiceID == st.ServiceID && st.Identity.Generation == st.Generation
}

func (s *ProcessSupervisor) owns(st *state.ServiceLifecycleState) *bool {
	if !s.identityMatchesState(st) {
		return boolPtr(false)
	}
	return s.ownsIdentity(st.Identity)
}

func (s *ProcessSupervisor) sameExecutable(record ProcessRecord, identity *state.ProcessIdentity) bool {
	if record.Posix == nil || identity == nil || identity.IsDocker() {
		return false
	}
	profile, err := profileFor(s.host.Catalog(), identity.ServiceID)
	if err != nil {
		return false
	}
	exe := absoluteArgv0(&profile.Command)
	if exe == "" {
		return false
	}
	return ExecutableMatches(record.Posix.CommandLine, []string{exe})
}

func (s *ProcessSupervisor) observedMatches(identity *state.ProcessIdentity) *bool {
	insp := s.options.Process.Inspect(identity)
	switch insp.Kind {
	case InspectionGone:
		return boolPtr(false)
	case InspectionUnknown:
		return nil
	}
	if insp.Observed == nil || !insp.Observed.Alive {
		return boolPtr(false)
	}
	if insp.Observed.Record.CommandFingerprint() != identity.CommandFingerprint && !s.sameExecutable(insp.Observed.Record, identity) {
		return boolPtr(false)
	}
	if identity.IsDocker() {
		rec := insp.Observed.Record.Docker
		if rec == nil || identity.ContainerName == nil || identity.ContainerID == nil || identity.ContainerStartedAt == nil {
			return boolPtr(false)
		}
		return boolPtr(rec.ContainerName == *identity.ContainerName && rec.ContainerID == *identity.ContainerID && rec.ContainerStartedAt == *identity.ContainerStartedAt)
	}
	rec := insp.Observed.Record.Posix
	if rec == nil {
		return boolPtr(false)
	}
	return boolPtr(rec.Pid == identity.PidValue() && rec.Pgid == identity.PgidValue() && rec.StartIdentity == identity.StartIdentityValue())
}

func (s *ProcessSupervisor) ownsIdentity(identity *state.ProcessIdentity) *bool {
	if identity == nil || identity.ManagerInstanceID != s.host.InstanceID() {
		return boolPtr(false)
	}
	return s.observedMatches(identity)
}

func (s *ProcessSupervisor) stillRunning(serviceID string, identity *state.ProcessIdentity, adopted bool) bool {
	if !adopted && identity != nil {
		s.active.Lock()
		a := s.actives[serviceID]
		if a != nil && a.identity.Generation == identity.Generation {
			exited := a.exited.Load()
			s.active.Unlock()
			return !exited
		}
		s.active.Unlock()
	}
	owned := s.ownsIdentity(identity)
	return owned == nil || *owned
}

func (s *ProcessSupervisor) orphan(st *state.ServiceLifecycleState, operationID *string) {
	s.transition(st.ServiceID, st.Generation, state.ActualOrphaned, state.ReadinessFailed, Changes{
		Error: setv("Process ownership identity no longer matches"), OperationID: operationPatch(operationID),
	})
}

func (s *ProcessSupervisor) transitionIfCurrent(serviceID string, generation, token uint64, actual state.ActualServiceState, readiness state.ServiceReadiness, changes Changes) {
	if s.valid(serviceID, generation, token) {
		s.transition(serviceID, generation, actual, readiness, changes)
	}
}

func (s *ProcessSupervisor) transition(serviceID string, generation uint64, actual state.ActualServiceState, readiness state.ServiceReadiness, changes Changes) {
	previous := s.state(serviceID)
	now := s.now()
	desired := state.DesiredRunning
	if previous != nil {
		desired = previous.DesiredState
	}
	if changes.Desired != nil {
		desired = *changes.Desired
	}
	created := now
	if previous != nil && previous.CreatedAt != "" {
		created = previous.CreatedAt
	}
	var prevID *state.ProcessIdentity
	var prevKind *state.ReadinessKind
	var prevDetail, prevExited, prevErr, prevOp *string
	var prevCode *int32
	if previous != nil {
		prevID = previous.Identity
		prevKind = previous.ReadinessKind
		prevDetail = previous.ReadinessDetail
		prevExited = previous.ExitedAt
		prevCode = previous.ExitCode
		prevErr = previous.Error
		prevOp = previous.CurrentOperationID
	}
	next := &state.ServiceLifecycleState{
		ServiceID:          serviceID,
		DesiredState:       desired,
		ActualState:        actual,
		Readiness:          readiness,
		Generation:         generation,
		Identity:           cloneIdentity(changes.Identity.apply(prevID)),
		ReadinessKind:      changes.ReadinessKind.apply(prevKind),
		ReadinessDetail:    changes.ReadinessDetail.apply(prevDetail),
		CreatedAt:          created,
		UpdatedAt:          now,
		ExitedAt:           changes.ExitedAt.apply(prevExited),
		ExitCode:           changes.ExitCode.apply(prevCode),
		Error:              changes.Error.apply(prevErr),
		CurrentOperationID: changes.OperationID.apply(prevOp),
	}
	errText := next.Error
	s.host.SetServiceState(next)
	if next.Readiness == state.ReadinessReady {
		s.restartsMu.Lock()
		delete(s.restarts, serviceID)
		s.restartsMu.Unlock()
	}
	if errText != nil {
		s.host.AppendLog(serviceID, now+" "+*errText+"\n")
	}
}

func (s *ProcessSupervisor) reapPersistedPosixIdentity(serviceID string) {
	st := s.state(serviceID)
	if st == nil || st.Identity == nil || st.Identity.IsDocker() || !s.identityMatchesState(st) {
		return
	}
	if owned := s.observedMatches(st.Identity); owned == nil || !*owned {
		return
	}
	identity := st.Identity
	known := s.knownDescendants(identity.ServiceID, identity.PidValue())
	if err := s.terminatePosixTree(identity.PidValue(), identity.StartIdentityValue(), identity.PgidValue(), known); err != nil {
		s.host.RecordBackgroundError("supervisor.terminate", identity.ServiceID+": "+err.Error())
	}
}

func (s *ProcessSupervisor) holderIsService(holder *PortHolder, profile *VerifiedProfile) bool {
	if holder.Pid <= 1 {
		return false
	}
	fingerprints := execTargetFingerprints(&profile.Command)
	cwd := processCwd(&profile.Command)
	if len(fingerprints) > 0 {
		if matches := s.options.Process.CommandMatches(fingerprints, nil, cwd); matches != nil {
			for _, matched := range *matches {
				if matched.Pid == holder.Pid && matched.StartIdentity == holder.StartIdentity {
					return true
				}
			}
		}
	}
	if marker, ok := installDirMarker(&profile.Command); ok && strings.Contains(holder.Command, marker) {
		return true
	}
	return false
}

func (s *ProcessSupervisor) adoptPortHolder(serviceID string, generation uint64, profile *VerifiedProfile, holder *PortHolder) {
	stub := posixIdentity(s.host.InstanceID(), serviceID, generation, s.now(), holder.Pid, holder.Pgid, holder.StartIdentity, "")
	insp := s.options.Process.Inspect(stub)
	if insp.Kind != InspectionObserved || insp.Observed == nil || !insp.Observed.Alive || insp.Observed.Record.Posix == nil {
		return
	}
	rec := insp.Observed.Record.Posix
	if rec.Pid != holder.Pid || rec.StartIdentity != holder.StartIdentity {
		return
	}
	identity := posixIdentity(s.host.InstanceID(), serviceID, generation, s.now(), rec.Pid, rec.Pgid, rec.StartIdentity, rec.CommandFingerprint)
	token := s.currentToken(serviceID)
	s.transition(serviceID, generation, state.ActualRunningUnready, state.ReadinessNotReady, Changes{
		Identity: setv(*identity), Error: clear[string](),
	})
	if !s.hasLiveOutputTail(serviceID) {
		s.attachOutput(serviceID, outputSource(identity, true))
	}
	s.armAdoptedWatch(serviceID, *identity, profile.Command, generation, token)
	_ = s.readiness(serviceID, profile, generation, identity, token, nil, true)
}

func (s *ProcessSupervisor) takeActiveTree(serviceID string) ([]ProcessTreeEntry, *int64) {
	s.active.Lock()
	a := s.actives[serviceID]
	delete(s.actives, serviceID)
	s.active.Unlock()
	if a == nil || a.lastTree == nil {
		return nil, nil
	}
	a.lastTree.mu.Lock()
	tree := append([]ProcessTreeEntry{}, a.lastTree.entries...)
	a.lastTree.mu.Unlock()
	if a.identity.IsDocker() {
		return tree, nil
	}
	pgid := a.identity.PgidValue()
	return tree, &pgid
}

func profileFor(cat *catalog.ServiceCatalog, serviceID string) (VerifiedProfile, error) {
	if cat == nil {
		return VerifiedProfile{}, Errorf("Unsupported service %s", serviceID)
	}
	for i := range cat.Services {
		service := &cat.Services[i]
		if service.ID != serviceID {
			continue
		}
		run := service.Profiles.Run
		if !run.IsVerified() || run.Command == nil {
			break
		}
		return VerifiedProfile{
			Command:            *run.Command,
			Readiness:          run.Readiness,
			ReadinessTimeoutMs: run.ReadinessTimeoutMs,
			Preparation:        run.Preparation,
			PreparationCommand: run.PreparationCommand,
		}, nil
	}
	return VerifiedProfile{}, Errorf("Unsupported service %s", serviceID)
}

func definitionFor(cat *catalog.ServiceCatalog, serviceID string) *catalog.ServiceDefinition {
	if cat == nil {
		return nil
	}
	for i := range cat.Services {
		if cat.Services[i].ID == serviceID {
			return &cat.Services[i]
		}
	}
	return nil
}

func isTaskCommand(readiness *catalog.ReadinessSpec) bool {
	return readiness != nil && readiness.Kind == "tailnet"
}

func isExternalTask(definition *catalog.ServiceDefinition, profile *VerifiedProfile) bool {
	return definition != nil && definition.Ownership != nil && *definition.Ownership == catalog.OwnershipExternal && profile.Readiness.Kind == "command"
}

func readinessKindOf(readiness *catalog.ReadinessSpec) state.ReadinessKind {
	if readiness == nil {
		return ""
	}
	switch readiness.Kind {
	case "process":
		return state.ReadinessKindProcess
	case "tcp":
		return state.ReadinessKindTCP
	case "http":
		return state.ReadinessKindHTTP
	case "container":
		return state.ReadinessKindContainer
	case "tailnet":
		return state.ReadinessKindTailnet
	case "command":
		return state.ReadinessKindCommand
	case "exit":
		return state.ReadinessKindExit
	case "log":
		return state.ReadinessKindLog
	default:
		return state.ReadinessKind(readiness.Kind)
	}
}

func readinessTCPPort(readiness *catalog.ReadinessSpec) (uint16, bool) {
	if readiness == nil || readiness.Kind != "tcp" || readiness.Port == nil {
		return 0, false
	}
	return *readiness.Port, true
}

func operationPatch(id *string) patch[string] {
	if id == nil {
		return keep[string]()
	}
	return setv(*id)
}

func desiredPtr(v state.DesiredServiceState) *state.DesiredServiceState { return &v }

func asSupervisor(err error) error {
	if err == nil {
		return nil
	}
	if _, ok := err.(*SupervisorError); ok {
		return err
	}
	return NewError(err.Error())
}

func containsStr(list []string, want string) bool {
	for _, v := range list {
		if v == want {
			return true
		}
	}
	return false
}

func itoa(n int64) string { return strconv.FormatInt(n, 10) }

func cloneStr(s *string) *string {
	if s == nil {
		return nil
	}
	v := *s
	return &v
}

func cloneI64(n *int64) *int64 {
	if n == nil {
		return nil
	}
	v := *n
	return &v
}

func cloneIdentity(id *state.ProcessIdentity) *state.ProcessIdentity {
	if id == nil {
		return nil
	}
	cp := *id
	cp.Pid = cloneI64(id.Pid)
	cp.Pgid = cloneI64(id.Pgid)
	cp.StartIdentity = cloneStr(id.StartIdentity)
	cp.ContainerName = cloneStr(id.ContainerName)
	cp.ContainerID = cloneStr(id.ContainerID)
	cp.ContainerStartedAt = cloneStr(id.ContainerStartedAt)
	return &cp
}

func withManagerInstance(id *state.ProcessIdentity, instance string) *state.ProcessIdentity {
	cp := cloneIdentity(id)
	if cp == nil {
		return nil
	}
	cp.ManagerInstanceID = instance
	return cp
}

func posixIdentity(instance, serviceID string, generation uint64, started string, pid, pgid int64, start, fingerprint string) *state.ProcessIdentity {
	return &state.ProcessIdentity{
		ManagerInstanceID: instance, ServiceID: serviceID, Generation: generation, StartedAt: started,
		CommandFingerprint: fingerprint, Pid: cloneI64(&pid), Pgid: cloneI64(&pgid), StartIdentity: cloneStr(&start),
	}
}

func identityFromRecord(instance, serviceID string, generation uint64, started string, record ProcessRecord) *state.ProcessIdentity {
	if record.Docker != nil {
		rec := record.Docker
		return &state.ProcessIdentity{
			ManagerInstanceID: instance, ServiceID: serviceID, Generation: generation, StartedAt: started,
			CommandFingerprint: rec.CommandFingerprint,
			ContainerName:      cloneStr(&rec.ContainerName), ContainerID: cloneStr(&rec.ContainerID),
			ContainerStartedAt: cloneStr(&rec.ContainerStartedAt),
		}
	}
	if record.Posix == nil {
		return nil
	}
	rec := record.Posix
	return posixIdentity(instance, serviceID, generation, started, rec.Pid, rec.Pgid, rec.StartIdentity, rec.CommandFingerprint)
}

func outputSource(identity *state.ProcessIdentity, skipBacklog bool) OutputSource {
	if identity != nil && identity.IsDocker() && identity.ContainerName != nil {
		var since *string
		if identity.ContainerStartedAt != nil && *identity.ContainerStartedAt != "" {
			since = identity.ContainerStartedAt
		}
		return ContainerSource(*identity.ContainerName, since, nil)
	}
	return ProcessSource(skipBacklog)
}

func verifiedPosixPgid(identity *state.ProcessIdentity, insp Inspection) (int64, bool) {
	if identity == nil || identity.IsDocker() || identity.PidValue() <= 0 || identity.StartIdentityValue() == "" {
		return 0, false
	}
	if insp.Kind != InspectionObserved || insp.Observed == nil || !insp.Observed.Alive || insp.Observed.Record.Posix == nil {
		return 0, false
	}
	rec := insp.Observed.Record.Posix
	if rec.Pid == identity.PidValue() && rec.StartIdentity == identity.StartIdentityValue() {
		return rec.Pgid, true
	}
	return 0, false
}

func lastShellWord(text, word string) (int, bool) {
	found := -1
	for i := 0; i+len(word) <= len(text); {
		j := strings.Index(text[i:], word)
		if j < 0 {
			break
		}
		index := i + j
		beforeOK := index == 0 || !isShellIdent(text[index-1])
		after := index + len(word)
		afterOK := after >= len(text) || !isShellIdent(text[after])
		if beforeOK && afterOK {
			found = index
		}
		i = index + len(word)
	}
	return found, found >= 0
}

func isShellIdent(b byte) bool {
	return (b >= 'a' && b <= 'z') || (b >= 'A' && b <= 'Z') || (b >= '0' && b <= '9') || b == '_'
}

func execProgram(shell string) (string, bool) {
	execAt, ok := lastShellWord(shell, "exec")
	if !ok {
		return "", false
	}
	rest := strings.TrimSpace(shell[execAt+4:])
	if rest == "" {
		return "", false
	}
	return unquoteExec(rest), true
}

func unquoteExec(rest string) string {
	rest = strings.TrimSpace(rest)
	if len(rest) >= 2 {
		q := rest[0]
		if (q == '\'' || q == '"') && rest[len(rest)-1] == q && !strings.ContainsRune(rest[1:len(rest)-1], rune(q)) {
			return rest[1 : len(rest)-1]
		}
	}
	return rest
}

func shellCdDir(shell string) (string, bool) {
	execAt, ok := lastShellWord(shell, "exec")
	if !ok {
		return "", false
	}
	before := shell[:execAt]
	cdAt, ok := lastShellWord(before, "cd")
	if !ok {
		return "", false
	}
	after := strings.TrimLeft(before[cdAt+2:], " \t")
	if rest, ok := strings.CutPrefix(after, "--"); ok {
		after = strings.TrimLeft(rest, " \t")
	}
	if after == "" {
		return "", false
	}
	q := after[0]
	if q == '\'' || q == '"' {
		body := after[1:]
		end := strings.IndexByte(body, q)
		if end < 0 {
			return "", false
		}
		return body[:end], true
	}
	return strings.Fields(after)[0], true
}

func processCwd(command *catalog.ServiceCommand) string {
	if command == nil {
		return "."
	}
	if !command.Command.IsShell() {
		return command.Cwd
	}
	dir, ok := shellCdDir(command.Command.Shell)
	if !ok {
		return command.Cwd
	}
	if strings.HasPrefix(dir, "/") || command.Cwd == "." || command.Cwd == "" {
		return dir
	}
	return strings.TrimRight(command.Cwd, "/") + "/" + dir
}

func execTargetFingerprints(command *catalog.ServiceCommand) []string {
	fingerprints := []string{NormalizeCommandFingerprint(command)}
	if command == nil || !command.Command.IsShell() {
		return fingerprints
	}
	if program, ok := execProgram(command.Command.Shell); ok {
		observed := NormalizeObservedCommandFingerprint(program)
		if !containsStr(fingerprints, observed) {
			fingerprints = append(fingerprints, observed)
		}
	}
	return fingerprints
}

func installDirMarker(command *catalog.ServiceCommand) (string, bool) {
	if command == nil || !command.Command.IsShell() {
		return "", false
	}
	program, ok := execProgram(command.Command.Shell)
	if !ok {
		return "", false
	}
	first := strings.Fields(program)
	if len(first) == 0 {
		return "", false
	}
	dir, bin, ok := strings.Cut(first[0], "/bin/")
	_ = bin
	if !ok || dir == "" || dir == "." || dir == ".." {
		return "", false
	}
	// rsplit: the last /bin/ is the boundary. Cut finds the first. Match Rust.
	if i := strings.LastIndex(first[0], "/bin/"); i >= 0 {
		dir = first[0][:i]
	}
	if dir == "" || dir == "." || dir == ".." {
		return "", false
	}
	marker := dir
	if cd, ok := shellCdDir(command.Command.Shell); ok && !strings.HasPrefix(dir, "/") {
		marker = cd + "/" + dir
	}
	marker = strings.TrimPrefix(marker, "./")
	if strings.Contains(marker, "build/install/") {
		return marker, true
	}
	return "", false
}
