package supervisor

import (
	"strings"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/shared"
	"github.com/ngosangns/hearth/go/internal/state"
)

func (s *ProcessSupervisor) startLocked(serviceID string, operationID *string, options StartOptions) error {
	if s.closing() {
		return nil
	}
	profile, err := profileFor(s.host.Catalog(), serviceID)
	if err != nil {
		return err
	}
	current := s.state(serviceID)
	s.active.Lock()
	existing, hasActive := s.actives[serviceID]
	var stopped, exited bool
	if hasActive {
		stopped = existing.stopped
		exited = existing.exited.Load()
	}
	s.active.Unlock()
	if hasActive && !stopped && !exited && (current == nil || current.ActualState != state.ActualFailed) {
		return nil
	}
	if hasActive {
		s.finalize(serviceID, true)
	}
	if current != nil && current.Identity != nil && isActiveState(current.ActualState) {
		if owned := s.owns(current); owned != nil && *owned {
			return nil
		}
	}
	var retained *state.ProcessIdentity
	if current != nil && s.identityMatchesState(current) && current.Identity != nil && !current.Identity.IsDocker() {
		if owned := s.observedMatches(current.Identity); owned != nil && *owned {
			retained = cloneIdentity(current.Identity)
		}
	}
	if retained == nil {
		retained = s.untrackedSameExecutable(serviceID, &profile)
	}
	var generation uint64
	if retained != nil && !retained.IsDocker() {
		generation = retained.Generation
	} else if current != nil {
		generation = current.Generation + 1
	} else {
		generation = 1
	}
	token := s.cancel(serviceID)
	if retained != nil {
		identity := withManagerInstance(retained, s.host.InstanceID())
		if owned := s.ownsIdentity(identity); owned == nil || *owned {
			exitJob := profile.Readiness.IsExit()
			actual := state.ActualRunningUnready
			readiness := state.ReadinessNotReady
			if exitJob {
				actual = state.ActualRunning
				readiness = state.ReadinessUnknown
			}
			running := state.DesiredRunning
			s.transition(serviceID, generation, actual, readiness, Changes{
				Desired:     &running,
				OperationID: operationPatch(operationID),
				Identity:    setv(*identity),
				Error:       clear[string](),
				ExitCode:    clear[int32](),
				ExitedAt:    clear[string](),
			})
			s.attachOutput(serviceID, outputSource(identity, true))
			s.armAdoptedWatch(serviceID, *identity, profile.Command, generation, token)
			return s.readiness(serviceID, &profile, generation, identity, token, operationID, true)
		}
		token = s.cancel(serviceID)
	}

	running := state.DesiredRunning
	s.transition(serviceID, generation, state.ActualPreparing, state.ReadinessUnknown, Changes{
		Desired:         &running,
		OperationID:     operationPatch(operationID),
		Error:           clear[string](),
		ExitCode:        clear[int32](),
		ExitedAt:        clear[string](),
		Identity:        clear[state.ProcessIdentity](),
		ReadinessDetail: clear[string](),
	})
	definition := definitionFor(s.host.Catalog(), serviceID)
	if definition != nil && definition.Artifact != nil {
		if s.options.ArtifactInstaller == nil {
			msg := "artifact installs are not supported by this host"
			s.transitionIfCurrent(serviceID, generation, token, state.ActualFailed, state.ReadinessFailed, Changes{
				Error: setv(msg), OperationID: operationPatch(operationID),
			})
			return Errorf("%s: %s", serviceID, msg)
		}
		sup := s
		err := s.options.ArtifactInstaller.Install(definition, func(line string) {
			if sup.valid(serviceID, generation, token) {
				sup.appendOutput(serviceID, line)
			}
		})
		if err != nil {
			if !s.valid(serviceID, generation, token) || s.closing() {
				return nil
			}
			s.transitionIfCurrent(serviceID, generation, token, state.ActualFailed, state.ReadinessFailed, Changes{
				Error: setv("Install failed: " + err.Error()), OperationID: operationPatch(operationID),
			})
			return asSupervisor(err)
		}
	}
	if !s.valid(serviceID, generation, token) || s.closing() {
		return nil
	}
	slow := len(profile.Preparation) > 0 || profile.PreparationCommand != nil || (definition != nil && definition.Profiles.Build != nil)
	if slow {
		if err := s.claimTCPPort(serviceID, &profile, generation, options.KillUnowned); err != nil {
			return err
		}
	}
	prepFailed := false
	if len(profile.Preparation) > 0 && s.options.Preparation != nil {
		if err := s.options.Preparation.Prepare(serviceID, profile.Preparation); err != nil {
			prepFailed = true
		}
	}
	if !prepFailed && profile.PreparationCommand != nil {
		prep := profile.PreparationCommand
		var ok *bool
		if prep.SerializationKey != nil {
			ok = s.serializedPreparation(*prep.SerializationKey, &prep.Command, prep.Cwd)
		} else if s.options.Probes != nil {
			ok = s.options.Probes.Command(nil, &prep.Command, prep.Cwd)
		}
		if ok == nil || !*ok {
			prepFailed = true
		}
	}
	if prepFailed {
		s.transitionIfCurrent(serviceID, generation, token, state.ActualFailed, state.ReadinessFailed, Changes{
			Error: setv("Preparation failed"), OperationID: operationPatch(operationID),
		})
		return Errorf("Preparation failed for %s", serviceID)
	}
	if !s.valid(serviceID, generation, token) || s.closing() {
		return nil
	}
	if definition != nil && definition.Profiles.Build != nil {
		if err := s.build(serviceID, definition, generation, token, operationID); err != nil {
			return err
		}
	}
	if !s.valid(serviceID, generation, token) || s.closing() {
		return nil
	}
	if err := s.claimTCPPort(serviceID, &profile, generation, options.KillUnowned); err != nil {
		return err
	}
	return s.spawnAndWait(serviceID, &profile, generation, token, operationID)
}

func (s *ProcessSupervisor) claimTCPPort(serviceID string, profile *VerifiedProfile, generation uint64, killUnowned bool) error {
	port, ok := readinessTCPPort(&profile.Readiness)
	if !ok || s.options.Probes == nil {
		return nil
	}
	inUse := s.options.Probes.PortInUse(port)
	if inUse == nil || !*inUse {
		return nil
	}
	var reclaimErr error
	if killUnowned {
		reclaimErr = s.reclaimPort(port)
	}
	if !killUnowned {
		held := s.describePortHolders(port)
		msg := "Port " + itoa(int64(port)) + " is held by " + held
		s.transition(serviceID, generation, state.ActualExternallyOwned, state.ReadinessFailed, Changes{Error: setv(msg)})
		return Errorf("%s: port %d is externally owned (held by %s)", serviceID, port, held)
	}
	if reclaimErr == nil {
		return nil
	}
	s.transition(serviceID, generation, state.ActualExternallyOwned, state.ReadinessFailed, Changes{Error: setv(reclaimErr.Error())})
	return asSupervisor(reclaimErr)
}

func (s *ProcessSupervisor) describePortHolders(port uint16) string {
	if s.options.Probes == nil {
		return "an unowned process"
	}
	holders := s.options.Probes.PortHolders(port)
	if holders == nil || len(*holders) == 0 {
		return "an unowned process"
	}
	parts := make([]string, 0, len(*holders))
	for _, h := range *holders {
		if h.Command == "" {
			parts = append(parts, "pid "+itoa(h.Pid))
		} else {
			parts = append(parts, "pid "+itoa(h.Pid)+" ("+h.Command+")")
		}
	}
	return strings.Join(parts, ", ")
}

// reclaimPort signals holder pids only — never the process group. The group
// of an unowned process is untrusted.
func (s *ProcessSupervisor) reclaimPort(port uint16) error {
	for _, signal := range []ProcessSignal{SignalTerm, SignalKill} {
		if holders := s.options.Probes.PortHolders(port); holders != nil {
			for _, holder := range *holders {
				s.options.Process.SignalPID(holder.Pid, holder.StartIdentity, signal)
			}
		}
		deadline := s.nowMillis() + s.options.TerminationGraceMs
		for {
			if inUse := s.options.Probes.PortInUse(port); inUse != nil && !*inUse {
				return nil
			}
			if s.nowMillis() >= deadline {
				break
			}
			s.sleep(s.options.ReadinessBackoffMs)
		}
	}
	return Errorf("Port %d is still held by %s after SIGKILL", port, s.describePortHolders(port))
}

func (s *ProcessSupervisor) duplicateFingerprints(serviceID string) []string {
	profile, err := profileFor(s.host.Catalog(), serviceID)
	if err != nil {
		return nil
	}
	fingerprints := []string{NormalizeCommandFingerprint(&profile.Command)}
	if st := s.state(serviceID); st != nil && st.Identity != nil && st.Identity.CommandFingerprint != "" {
		extra := st.Identity.CommandFingerprint
		if !containsStr(fingerprints, extra) {
			fingerprints = append(fingerprints, extra)
		}
	}
	return fingerprints
}

func (s *ProcessSupervisor) serviceCwd(serviceID string) string {
	profile, err := profileFor(s.host.Catalog(), serviceID)
	if err != nil || profile.Command.Cwd == "" {
		return "."
	}
	return profile.Command.Cwd
}

func absoluteArgv0(command *catalog.ServiceCommand) string {
	if command == nil || !command.Command.IsArgv() || len(command.Command.Argv) == 0 {
		return ""
	}
	exe := command.Command.Argv[0]
	if strings.HasPrefix(exe, "/") {
		return exe
	}
	return ""
}

func (s *ProcessSupervisor) duplicateExecutables(serviceID string) []string {
	profile, err := profileFor(s.host.Catalog(), serviceID)
	if err != nil {
		return nil
	}
	exe := absoluteArgv0(&profile.Command)
	if exe == "" {
		return nil
	}
	cat := s.host.Catalog()
	cwd := profile.Command.Cwd
	for _, other := range cat.Services {
		if other.ID == serviceID {
			continue
		}
		otherProfile, oerr := profileFor(cat, other.ID)
		if oerr != nil {
			continue
		}
		otherExe := absoluteArgv0(&otherProfile.Command)
		if otherExe != "" && pathsEqual(otherExe, exe) && pathsEqual(otherProfile.Command.Cwd, cwd) {
			return nil
		}
	}
	return []string{exe}
}

func (s *ProcessSupervisor) untrackedSameExecutable(serviceID string, profile *VerifiedProfile) *state.ProcessIdentity {
	executables := s.duplicateExecutables(serviceID)
	if len(executables) == 0 || s.options.Process == nil {
		return nil
	}
	matches := s.options.Process.CommandMatches(nil, executables, profile.Command.Cwd)
	if matches == nil {
		return nil
	}
	var matched *CommandMatch
	for i := range *matches {
		if (*matches)[i].Pid > 1 {
			m := (*matches)[i]
			matched = &m
			break
		}
	}
	if matched == nil {
		return nil
	}
	generation := uint64(1)
	if st := s.state(serviceID); st != nil {
		generation = st.Generation + 1
	}
	stub := posixIdentity(s.host.InstanceID(), serviceID, generation, s.now(), matched.Pid, matched.Pid, matched.StartIdentity, "")
	insp := s.options.Process.Inspect(stub)
	if insp.Kind != InspectionObserved || insp.Observed == nil || !insp.Observed.Alive || insp.Observed.Record.Posix == nil {
		return nil
	}
	rec := insp.Observed.Record.Posix
	if rec.Pid != matched.Pid || rec.StartIdentity != matched.StartIdentity {
		return nil
	}
	if !ExecutableMatches(rec.CommandLine, executables) {
		return nil
	}
	return posixIdentity(s.host.InstanceID(), serviceID, generation, s.now(), rec.Pid, rec.Pgid, rec.StartIdentity, rec.CommandFingerprint)
}

func (s *ProcessSupervisor) reapDuplicateServiceProcesses(serviceID string, fingerprints []string, cwd string) error {
	executables := s.duplicateExecutables(serviceID)
	if len(fingerprints) == 0 && len(executables) == 0 {
		return nil
	}
	matches := s.options.Process.CommandMatches(fingerprints, executables, cwd)
	if matches == nil {
		return Errorf("%s: could not list processes; refusing to start beside an unchecked duplicate", serviceID)
	}
	seen := map[int64]struct{}{}
	var entries []ProcessTreeEntry
	for _, matched := range *matches {
		if matched.Pid <= 1 {
			continue
		}
		snap := s.processTree(matched.Pid, matched.StartIdentity)
		switch snap.Kind {
		case SnapshotUnknown:
			return Errorf("%s: the process tree could not be read; refusing to start beside an unchecked duplicate", serviceID)
		case SnapshotAbsent:
			continue
		}
		for _, entry := range snap.Entries {
			if _, ok := seen[entry.Pid]; ok {
				continue
			}
			seen[entry.Pid] = struct{}{}
			entries = append(entries, entry)
		}
	}
	if len(entries) == 0 {
		return nil
	}
	s.signalEntries(entries, SignalTerm)
	deadline := s.nowMillis() + s.options.TerminationGraceMs
	for s.nowMillis() < deadline {
		alive, ok := s.processTreeAlive(entries)
		if !ok {
			return Errorf("%s: a duplicate process's liveness could not be verified", serviceID)
		}
		if !alive {
			return nil
		}
		s.sleep(s.options.ReadinessBackoffMs)
	}
	alive, ok := s.processTreeAlive(entries)
	if !ok {
		return Errorf("%s: a duplicate process's liveness could not be verified", serviceID)
	}
	if !alive {
		return nil
	}
	s.signalEntries(entries, SignalKill)
	alive, ok = s.processTreeAlive(entries)
	if !ok {
		return Errorf("%s: a duplicate process's liveness could not be verified", serviceID)
	}
	if alive {
		var pids []string
		for _, entry := range entries {
			pids = append(pids, itoa(entry.Pid))
		}
		return Errorf("%s still has a duplicate process (pid %s)", serviceID, strings.Join(pids, ", "))
	}
	return nil
}

func (s *ProcessSupervisor) signalEntries(entries []ProcessTreeEntry, signal ProcessSignal) {
	for _, entry := range entries {
		if entry.Pid > 1 {
			s.options.Process.SignalPID(entry.Pid, entry.StartIdentity, signal)
		}
	}
}

func (s *ProcessSupervisor) restartNeedsStop(serviceID string) bool {
	st := s.state(serviceID)
	if st == nil || st.ActualState == state.ActualStopped {
		return false
	}
	switch st.ActualState {
	case state.ActualQueuedStart, state.ActualPreparing, state.ActualStarting:
		return true
	}
	if st.Identity != nil {
		return true
	}
	profile, err := profileFor(s.host.Catalog(), serviceID)
	return err == nil && profile.Command.DockerStopCommand != nil
}

func (s *ProcessSupervisor) noteServiceHolder(serviceID string) bool {
	profile, err := profileFor(s.host.Catalog(), serviceID)
	if err != nil {
		return false
	}
	st := s.state(serviceID)
	if st == nil || st.Identity != nil {
		return false
	}
	port, ok := readinessTCPPort(&profile.Readiness)
	if !ok || s.options.Probes == nil {
		return false
	}
	if inUse := s.options.Probes.PortInUse(port); inUse == nil || !*inUse {
		return false
	}
	holders := s.options.Probes.PortHolders(port)
	if holders == nil {
		return false
	}
	var chosen *PortHolder
	for i := range *holders {
		if s.holderIsService(&(*holders)[i], &profile) {
			h := (*holders)[i]
			chosen = &h
			break
		}
	}
	if chosen == nil {
		return false
	}
	stub := posixIdentity(s.host.InstanceID(), serviceID, st.Generation, s.now(), chosen.Pid, chosen.Pgid, chosen.StartIdentity, "")
	insp := s.options.Process.Inspect(stub)
	if insp.Kind != InspectionObserved || insp.Observed == nil || !insp.Observed.Alive || insp.Observed.Record.Posix == nil {
		return false
	}
	rec := insp.Observed.Record.Posix
	if rec.Pid != chosen.Pid || rec.StartIdentity != chosen.StartIdentity {
		return false
	}
	identity := posixIdentity(s.host.InstanceID(), serviceID, st.Generation, s.now(), rec.Pid, rec.Pgid, rec.StartIdentity, rec.CommandFingerprint)
	// Stopped would make stopLocked return before signalling. Leave that state
	// so restart actually replaces the listener.
	s.transition(serviceID, st.Generation, state.ActualRunningUnready, state.ReadinessNotReady, Changes{
		Identity: setv(*identity), Error: clear[string](),
	})
	return true
}

func (s *ProcessSupervisor) stopLocked(serviceID string, operationID *string) error {
	st := s.state(serviceID)
	if st == nil || st.ActualState == state.ActualStopped {
		return nil
	}
	idle, err := s.exitCommandIsIdle(serviceID, st)
	if err != nil {
		return err
	}
	if idle {
		return nil
	}
	s.active.Lock()
	_, hasActive := s.actives[serviceID]
	s.active.Unlock()
	if st.Identity == nil {
		switch st.ActualState {
		case state.ActualQueuedStart, state.ActualPreparing, state.ActualStarting:
			stopped := state.DesiredStopped
			s.transition(serviceID, st.Generation, state.ActualStopped, state.ReadinessUnknown, Changes{
				Desired: &stopped, OperationID: operationPatch(operationID),
				ExitedAt: setv(s.now()), Error: clear[string](), ExitCode: clear[int32](),
			})
			return nil
		}
		return s.stopUnowned(serviceID, st, operationID, false)
	}
	owned := s.owns(st)
	if owned == nil || !*owned {
		if owned == nil {
			return Errorf("%s cannot be stopped: the process's liveness could not be verified", serviceID)
		}
		insp := s.options.Process.Inspect(st.Identity)
		switch insp.Kind {
		case InspectionUnknown:
			return Errorf("%s cannot be stopped: the process's liveness could not be verified", serviceID)
		case InspectionObserved:
			if insp.Observed != nil && insp.Observed.Alive {
				if s.identityMatchesState(st) && !st.Identity.IsDocker() {
					if pgid, ok := verifiedPosixPgid(st.Identity, insp); ok {
						return s.stopVerifiedPosix(serviceID, st, st.Identity, pgid, hasActive, operationID)
					}
				}
				break
			}
			fallthrough
		case InspectionGone:
			tree, leader := s.takeActiveTree(serviceID)
			if leader != nil {
				s.reapSecondaryGroups(tree, *leader)
			}
			stopped := state.DesiredStopped
			s.transition(serviceID, st.Generation, state.ActualStopped, state.ReadinessUnknown, Changes{
				Desired: &stopped, ExitedAt: setv(s.now()), Error: clear[string](), ExitCode: clear[int32](),
			})
			return nil
		}
		s.orphan(st, operationID)
		described := "pid " + itoa(st.Identity.PidValue())
		if st.Identity.IsDocker() && st.Identity.ContainerName != nil {
			described = "container " + *st.Identity.ContainerName
		}
		return Errorf("%s cannot be stopped: the process at %s is no longer owned by this manager", serviceID, described)
	}
	s.transition(serviceID, st.Generation, state.ActualStopping, st.Readiness, Changes{
		Desired: desiredPtr(state.DesiredStopped), OperationID: operationPatch(operationID),
	})
	if hasActive {
		s.active.Lock()
		if a := s.actives[serviceID]; a != nil {
			a.stopped = true
		}
		s.active.Unlock()
	}
	profile, err := profileFor(s.host.Catalog(), serviceID)
	if err != nil {
		return err
	}
	if err := s.terminate(st.Identity, &profile.Command); err != nil {
		return err
	}
	if hasActive {
		s.active.Lock()
		delete(s.actives, serviceID)
		s.active.Unlock()
	}
	stopped := state.DesiredStopped
	s.transition(serviceID, st.Generation, state.ActualStopped, state.ReadinessUnknown, Changes{
		Desired: &stopped, ExitedAt: setv(s.now()), Error: clear[string](), ExitCode: clear[int32](),
	})
	return nil
}

func (s *ProcessSupervisor) stopVerifiedPosix(serviceID string, st *state.ServiceLifecycleState, identity *state.ProcessIdentity, pgid int64, hasActive bool, operationID *string) error {
	s.transition(serviceID, st.Generation, state.ActualStopping, st.Readiness, Changes{
		Desired: desiredPtr(state.DesiredStopped), OperationID: operationPatch(operationID),
	})
	if hasActive {
		s.active.Lock()
		if a := s.actives[serviceID]; a != nil {
			a.stopped = true
		}
		s.active.Unlock()
	}
	s.detachOutput(serviceID)
	known := s.knownDescendants(serviceID, identity.PidValue())
	if err := s.terminatePosixTree(identity.PidValue(), identity.StartIdentityValue(), pgid, known); err != nil {
		return err
	}
	if hasActive {
		s.active.Lock()
		delete(s.actives, serviceID)
		s.active.Unlock()
	}
	stopped := state.DesiredStopped
	s.transition(serviceID, st.Generation, state.ActualStopped, state.ReadinessUnknown, Changes{
		Desired: &stopped, ExitedAt: setv(s.now()), Error: clear[string](), ExitCode: clear[int32](),
	})
	return nil
}

func (s *ProcessSupervisor) stopUnowned(serviceID string, st *state.ServiceLifecycleState, operationID *string, forShutdown bool) error {
	profile, err := profileFor(s.host.Catalog(), serviceID)
	if err != nil {
		return err
	}
	command := profile.Command
	if forShutdown && command.DockerStopCommand != nil {
		spec := shared.ShutdownStopCommand(command.DockerStopCommand)
		command.DockerStopCommand = &spec
	}
	if command.DockerStopCommand == nil {
		reason := "it is owned externally"
		if st.ActualState == state.ActualExternallyOwned {
			if st.Error != nil {
				reason = *st.Error
			} else {
				reason = "its port is held by a process this manager does not own"
			}
		}
		return Errorf("%s cannot be stopped: %s, and its catalog declares no `stop` command", serviceID, reason)
	}
	supported, stopErr := s.options.Process.StopContainer(&command, func(string) {})
	if !supported {
		return Errorf("%s cannot be stopped: this process adapter has no stop command support", serviceID)
	}
	if stopErr != nil {
		return asSupervisor(stopErr)
	}
	s.detachOutput(serviceID)
	stopped := state.DesiredStopped
	s.transition(serviceID, st.Generation, state.ActualStopped, state.ReadinessUnknown, Changes{
		Desired: &stopped, OperationID: operationPatch(operationID),
		ExitedAt: setv(s.now()), Error: clear[string](), ExitCode: clear[int32](),
	})
	return nil
}

func (s *ProcessSupervisor) spawnAndWait(serviceID string, profile *VerifiedProfile, generation, token uint64, operationID *string) error {
	running := state.DesiredRunning
	s.transition(serviceID, generation, state.ActualStarting, state.ReadinessNotReady, Changes{
		Desired: &running, OperationID: operationPatch(operationID), Identity: clear[state.ProcessIdentity](),
	})
	if !s.valid(serviceID, generation, token) || s.closing() {
		return nil
	}
	fingerprint := NormalizeCommandFingerprint(&profile.Command)
	s.matchersMu.Lock()
	delete(s.matchers, serviceID)
	delete(s.noLogMatcher, serviceID)
	s.matchersMu.Unlock()
	onOutput := func(data string) {
		if s.valid(serviceID, generation, token) {
			s.appendOutput(serviceID, data)
		}
	}
	input := SpawnInput{Command: profile.Command, CommandFingerprint: fingerprint, ServiceID: serviceID}
	isContainer := catalog.IsContainerCommand(&profile.Command)
	var managed *ManagedProcess
	var spawnErr error
	if isContainer {
		s.composeStart.Lock()
		managed, spawnErr = s.options.Process.Spawn(input, onOutput)
		s.composeStart.Unlock()
	} else {
		managed, spawnErr = s.options.Process.Spawn(input, onOutput)
	}
	if spawnErr != nil {
		if !s.valid(serviceID, generation, token) || s.closing() {
			return nil
		}
		s.transitionIfCurrent(serviceID, generation, token, state.ActualFailed, state.ReadinessFailed, Changes{
			Error: setv(spawnErr.Error()), OperationID: operationPatch(operationID), Identity: clear[state.ProcessIdentity](),
		})
		return asSupervisor(spawnErr)
	}
	definition := definitionFor(s.host.Catalog(), serviceID)
	if (isTaskCommand(&profile.Readiness) || isExternalTask(definition, profile)) && !managed.Record.IsDocker() {
		if managed.Record.Posix == nil {
			return Errorf("%s task did not spawn a process", serviceID)
		}
		rec := managed.Record.Posix
		taskID := posixIdentity(s.host.InstanceID(), serviceID, generation, s.now(), rec.Pid, rec.Pgid, rec.StartIdentity, rec.CommandFingerprint)
		s.transition(serviceID, generation, state.ActualRunningUnready, state.ReadinessNotReady, Changes{
			Identity: clear[state.ProcessIdentity](),
		})
		err := s.awaitExternalTask(serviceID, profile, generation, token, operationID, managed.Exited)
		if err != nil || !s.valid(serviceID, generation, token) {
			if terr := s.terminate(taskID, &profile.Command); terr != nil {
				s.reportTerminateFailure(serviceID, terr)
				if err != nil {
					return Errorf("%s; the task process could not be stopped: %s", err.Error(), terr.Error())
				}
				return terr
			}
		}
		return err
	}
	identity := identityFromRecord(s.host.InstanceID(), serviceID, generation, s.now(), managed.Record)
	if identity == nil {
		return Errorf("%s spawn did not report a process", serviceID)
	}
	if !s.valid(serviceID, generation, token) {
		s.reportTerminateFailure(serviceID, s.terminate(identity, &profile.Command))
		return nil
	}
	s.trackSpawned(serviceID, profile.Command, *identity, generation, token, managed.Exited)
	s.attachOutput(serviceID, outputSource(identity, false))
	exitJob := profile.Readiness.IsExit()
	actual := state.ActualRunningUnready
	readiness := state.ReadinessNotReady
	if exitJob {
		actual = state.ActualRunning
		readiness = state.ReadinessUnknown
	}
	s.transition(serviceID, generation, actual, readiness, Changes{Identity: setv(*identity)})
	var stored *state.ProcessIdentity
	if st := s.state(serviceID); st != nil {
		stored = st.Identity
	}
	return s.readiness(serviceID, profile, generation, stored, token, operationID, false)
}
