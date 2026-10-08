// HTTP+SSE route handlers for HearthManager — ported from
// rust/crates/hearth-core/src/manager/http/routes.rs.
package manager

import (
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"sync"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
	"github.com/ngosangns/hearth/go/internal/supervisor"
)

// daemonLogName mirrors `DAEMON_LOG_NAME` in rust/crates/hearth-core/src/daemon.rs.
const daemonLogName = "daemon.log"

func requestIDConflictError() *ManagerHttpError {
	return newHTTPError(http.StatusConflict, "request_id_conflict", "requestId is already used by a different operation")
}

func isServiceID(cat *catalog.ServiceCatalog, v any) (string, bool) {
	s, ok := v.(string)
	if !ok {
		return "", false
	}
	for i := range cat.Services {
		if cat.Services[i].ID == s {
			return s, true
		}
	}
	return "", false
}

// ---------------------------------------------------------------------------------------------
// Read endpoints
// ---------------------------------------------------------------------------------------------

func (m *HearthManager) getHealthz(w http.ResponseWriter, r *http.Request, _ map[string]string) {
	body := struct {
		Status          string  `json:"status"`
		ProtocolVersion uint32  `json:"protocolVersion"`
		InstanceID      *string `json:"instanceId,omitempty"`
	}{Status: "ok", ProtocolVersion: state.ProtocolVersion}
	if m.isAuthorized(r) {
		id := m.instanceID
		body.InstanceID = &id
	}
	writeJSON(w, http.StatusOK, body)
}

func (m *HearthManager) getManagerInfo(w http.ResponseWriter, _ *http.Request, _ map[string]string) {
	writeJSON(w, http.StatusOK, m.Info())
}

func (m *HearthManager) getCatalog(w http.ResponseWriter, _ *http.Request, _ map[string]string) {
	writeJSON(w, http.StatusOK, catalogResponse{Catalog: m.Catalog()})
}

// getUrls returns every service URL in the current catalog with its
// placeholders resolved. Resolution may spawn `tailscale` (cached).
func (m *HearthManager) getUrls(w http.ResponseWriter, _ *http.Request, _ map[string]string) {
	urls, unresolved := catalog.ResolveServiceURLs(m.Catalog(), LookupPlaceholder)
	if urls == nil {
		urls = []catalog.ResolvedServiceURL{}
	}
	if unresolved == nil {
		unresolved = []catalog.UnresolvedServiceURL{}
	}
	writeJSON(w, http.StatusOK, struct {
		URLs       []catalog.ResolvedServiceURL   `json:"urls"`
		Unresolved []catalog.UnresolvedServiceURL `json:"unresolved"`
	}{URLs: urls, Unresolved: unresolved})
}

func (m *HearthManager) getServices(w http.ResponseWriter, _ *http.Request, _ map[string]string) {
	writeJSON(w, http.StatusOK, servicesResponse{Services: m.ServiceStates()})
}

// ---------------------------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------------------------

func (m *HearthManager) postOperation(w http.ResponseWriter, r *http.Request, _ map[string]string) {
	if herr := m.ensureNotClosing(); herr != nil {
		writeHTTPError(w, herr)
		return
	}
	body, herr := strictBody(r, []string{"requestId", "serviceId", "action", "killUnowned"}, []string{"requestId", "serviceId", "action"})
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	requestID, herr := requireRequestID(body)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	cat := m.Catalog()
	serviceID, ok := isServiceID(cat, body["serviceId"])
	if !ok {
		writeHTTPError(w, newHTTPError(http.StatusBadRequest, "invalid_service", "serviceId must be a catalog service"))
		return
	}
	var action state.ServiceOperationKind
	switch body["action"] {
	case "start":
		action = state.OpStart
	case "stop":
		action = state.OpStop
	case "restart":
		action = state.OpRestart
	case "status":
		action = state.OpStatus
	default:
		writeHTTPError(w, newHTTPError(http.StatusBadRequest, "invalid_action", "action must be start, stop, restart, or status"))
		return
	}
	// A disabled service accepts reads (`status`) but no lifecycle action —
	// `groups:` expands past it too, so the only way to land here is an
	// explicit serviceId.
	if action != state.OpStatus {
		for i := range cat.Services {
			if cat.Services[i].ID == serviceID && cat.Services[i].Disabled {
				writeHTTPError(w, newHTTPError(http.StatusConflict, "service_disabled", "service is disabled"))
				return
			}
		}
	}
	killUnowned, herr := parseBoolFlag(body, "killUnowned")
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	if killUnowned && action != state.OpStart {
		writeHTTPError(w, newHTTPError(http.StatusBadRequest, "invalid_request", "killUnowned only applies to a start action"))
		return
	}
	var startServices []string
	if action == state.OpStart {
		startServices = []string{serviceID}
	}

	input := OperationInput{
		RequestID: requestID,
		Kind:      state.OperationKindService,
		ServiceID: &serviceID,
		Action:    &action,
	}
	if existing, err := m.operations.ResolveRequest(&input); err != nil {
		writeHTTPError(w, requestIDConflictError())
		return
	} else if existing != nil {
		writeJSON(w, http.StatusAccepted, operationResponse{Operation: existing})
		return
	}

	execute := func(handle *OperationHandle) *state.OperationError {
		return m.runServiceOperation(handle, serviceID, action, startServices, killUnowned)
	}
	rejected := func(handle *OperationHandle) {
		if len(startServices) > 0 {
			m.clearQueuedStarts(startServices, handle.Snapshot().ID)
		}
	}
	// The worker is spawned inside ScheduleWhenReleased and skips a start whose
	// desired state is not yet `running` ("start cancelled"), then reports
	// success. Hold it until the desired state is published, or a stopped
	// service is left in `queued-start` after the operation already finished.
	operation, release, err := m.operations.ScheduleWhenReleased(input, execute, rejected)
	if err != nil {
		writeHTTPError(w, requestIDConflictError())
		return
	}
	if release != nil {
		if action == state.OpStart {
			m.queueStoppedServicesForStart(startServices, operation.ID)
		}
		release <- struct{}{}
	} else if action == state.OpStart &&
		(operation.Status == state.OpStatusSucceeded || operation.Status == state.OpStatusFailed) {
		// A duplicate request id that already settled between resolve and
		// schedule. This call did not queue, but clearing the finished
		// operation's id drops a row it left in `queued-start`.
		m.clearQueuedStarts(startServices, operation.ID)
	}
	writeJSON(w, http.StatusAccepted, operationResponse{Operation: operation})
}

func (m *HearthManager) runServiceOperation(handle *OperationHandle, serviceID string, action state.ServiceOperationKind, startServices []string, killUnowned bool) *state.OperationError {
	var err error
	switch action {
	case state.OpStart:
		// `startServices` is `[serviceID]` for a `start` action — routed through
		// the same startSelectedDag as bulk-start so `hearth start <id>`, the
		// app's Start button and MCP `manage start` all land on one code path.
		err = m.startSelectedDag(startServices, handle, killUnowned)
	case state.OpStop:
		err = m.supervisor.Stop(serviceID, nil)
	case state.OpRestart:
		err = m.supervisor.Restart(serviceID, nil)
	case state.OpStatus:
		err = m.supervisor.Status(serviceID)
	}
	if action == state.OpStart {
		// The real operation id, never an empty string: clearQueuedStarts skips
		// its ownership check when the id is empty, which would let this
		// operation reset `queued-start` services belonging to a concurrent
		// bulk-start and cancel it.
		m.clearQueuedStarts(startServices, handle.Snapshot().ID)
	}
	if err != nil {
		return &state.OperationError{Code: "operation_failed", Message: err.Error()}
	}
	return nil
}

func (m *HearthManager) postBulkStart(w http.ResponseWriter, r *http.Request, _ map[string]string) {
	if herr := m.ensureNotClosing(); herr != nil {
		writeHTTPError(w, herr)
		return
	}
	body, herr := strictBody(r, []string{"requestId", "targets", "killUnowned"}, []string{"requestId", "targets"})
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	requestID, herr := requireRequestID(body)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	cat := m.Catalog()
	targetsValue, _ := body["targets"].([]any)
	var targets []string
	seen := map[string]bool{}
	ok := len(targetsValue) > 0
	for _, t := range targetsValue {
		id, isID := isServiceID(cat, t)
		if isID && !seen[id] {
			seen[id] = true
			targets = append(targets, id)
			continue
		}
		ok = false
		break
	}
	if !ok {
		writeHTTPError(w, newHTTPError(http.StatusBadRequest, "invalid_targets", "targets must be a non-empty set of catalog services"))
		return
	}
	// Disabled services are silent skips in a bulk start — the same way
	// `groups:` expansion already drops them. When nothing runnable remains,
	// the request itself is what's wrong.
	enabled := targets[:0]
	for _, id := range targets {
		disabled := false
		for i := range cat.Services {
			if cat.Services[i].ID == id && cat.Services[i].Disabled {
				disabled = true
				break
			}
		}
		if !disabled {
			enabled = append(enabled, id)
		}
	}
	targets = enabled
	if len(targets) == 0 {
		writeHTTPError(w, newHTTPError(http.StatusBadRequest, "invalid_targets", "targets must include at least one enabled catalog service"))
		return
	}
	killUnowned, herr := parseBoolFlag(body, "killUnowned")
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	for _, t := range targets {
		verified := false
		for i := range cat.Services {
			if cat.Services[i].ID == t {
				verified = cat.Services[i].Profiles.Run.IsVerified()
				break
			}
		}
		if !verified {
			writeHTTPError(w, newHTTPError(http.StatusConflict, "unsupported_service", "The requested catalog service is not executable"))
			return
		}
	}
	selected := targets

	input := OperationInput{
		RequestID:        requestID,
		Kind:             state.OperationKindBulkStart,
		TargetServiceIDs: &targets,
	}
	if existing, err := m.operations.ResolveRequest(&input); err != nil {
		writeHTTPError(w, requestIDConflictError())
		return
	} else if existing != nil {
		writeJSON(w, http.StatusAccepted, operationResponse{Operation: existing})
		return
	}
	execute := func(handle *OperationHandle) *state.OperationError {
		operationID := handle.Snapshot().ID
		err := m.startSelectedDag(selected, handle, killUnowned)
		m.clearQueuedStarts(selected, operationID)
		if err != nil {
			return &state.OperationError{Code: "operation_failed", Message: err.Error()}
		}
		return nil
	}
	rejected := func(handle *OperationHandle) {
		m.clearQueuedStarts(selected, handle.Snapshot().ID)
	}
	operation, release, err := m.operations.ScheduleWhenReleased(input, execute, rejected)
	if err != nil {
		writeHTTPError(w, requestIDConflictError())
		return
	}
	if release != nil {
		m.queueStoppedServicesForStart(selected, operation.ID)
		release <- struct{}{}
	} else if operation.Status == state.OpStatusSucceeded || operation.Status == state.OpStatusFailed {
		m.clearQueuedStarts(selected, operation.ID)
	}
	writeJSON(w, http.StatusAccepted, operationResponse{Operation: operation})
}

// startOutcome distinguishes the two non-ready outcomes. Only `failed` makes
// the operation fail: `skipped` because a start was cancelled mid-flight is a
// normal result the TS source reports as success.
type startOutcome int

const (
	outcomeReady startOutcome = iota
	outcomeSkipped
	outcomeFailed
)

type startResult struct {
	serviceID string
	outcome   startOutcome
	message   string
}

// startSelectedDag starts every selected service concurrently and
// independently — no service waits on another.
func (m *HearthManager) startSelectedDag(selected []string, operation *OperationHandle, killUnowned bool) error {
	start := func(serviceID string) startResult {
		m.stateMu.Lock()
		st := m.state.Services[serviceID]
		desiredRunning := st != nil && st.DesiredState == state.DesiredRunning
		m.stateMu.Unlock()
		if !desiredRunning {
			m.operations.Trace(operation, "Skipped: "+serviceID+" (start cancelled)")
			return startResult{serviceID: serviceID, outcome: outcomeSkipped}
		}
		m.stateMu.Lock()
		st = m.state.Services[serviceID]
		alreadyReady := st != nil && st.ActualState == state.ActualReady && st.Readiness == state.ReadinessReady
		m.stateMu.Unlock()
		if alreadyReady {
			m.operations.Trace(operation, "Ready: "+serviceID+" (already ready)")
			return startResult{serviceID: serviceID, outcome: outcomeReady}
		}
		m.operations.Trace(operation, "Starting: "+serviceID)
		operationID := operation.Snapshot().ID
		if err := m.supervisor.StartWithOptions(serviceID, &operationID, supervisor.StartOptions{KillUnowned: killUnowned}); err != nil {
			m.operations.Trace(operation, "Failed: "+serviceID+" ("+err.Error()+")")
			return startResult{serviceID: serviceID, outcome: outcomeFailed, message: err.Error()}
		}
		m.stateMu.Lock()
		st = m.state.Services[serviceID]
		label := ""
		if st != nil {
			switch st.ActualState {
			case state.ActualSucceeded:
				label = "Succeeded"
			case state.ActualReady:
				label = "Ready"
			case state.ActualRunningUnready:
				label = "Running"
			}
		}
		m.stateMu.Unlock()
		if label != "" {
			m.operations.Trace(operation, label+": "+serviceID)
			return startResult{serviceID: serviceID, outcome: outcomeReady}
		}
		m.operations.Trace(operation, "Failed: "+serviceID+" (did not become ready)")
		return startResult{serviceID: serviceID, outcome: outcomeFailed, message: "did not become ready"}
	}

	results := make([]startResult, len(selected))
	var wg sync.WaitGroup
	for i, id := range selected {
		wg.Add(1)
		go func(i int, id string) {
			defer wg.Done()
			results[i] = start(id)
		}(i, id)
	}
	wg.Wait()

	var failures []startResult
	for _, r := range results {
		if r.outcome == outcomeFailed {
			failures = append(failures, r)
		}
	}
	if len(failures) > 0 {
		ids := make([]string, len(failures))
		for i, f := range failures {
			ids[i] = f.serviceID
		}
		summary := strings.Join(ids, ", ")
		// The first failure's own cause is appended, as the TS source does —
		// without it the caller only learns *which* service failed, never why.
		cause := ""
		if failures[0].message != "" {
			cause = " (" + failures[0].message + ")"
		}
		return supervisor.NewError("Service startup failed: " + summary + cause)
	}
	return nil
}

func (m *HearthManager) queueStoppedServicesForStart(serviceIDs []string, operationID string) {
	m.lifecycle.Lock()
	defer m.lifecycle.Unlock()
	if m.closed.Load() {
		return
	}
	timestamp := now()
	m.stateMu.Lock()
	changed := false
	for _, serviceID := range serviceIDs {
		previous := m.state.Services[serviceID]
		prev := defaultStateOr(previous, serviceID, timestamp)
		if prev.ActualState != state.ActualStopped {
			// Already up, or mid-flight. Its actual state must not be disturbed
			// — but the INTENT still has to be recorded, because
			// startSelectedDag skips any node whose desired_state is not
			// `running` ("start cancelled") before it ever checks whether the
			// node is already ready.
			if prev.DesiredState != state.DesiredRunning {
				next := prev
				next.DesiredState = state.DesiredRunning
				next.UpdatedAt = timestamp
				m.state.Services[serviceID] = &next
				changed = true
			}
			continue
		}
		next := prev
		next.DesiredState = state.DesiredRunning
		next.ActualState = state.ActualQueuedStart
		next.Readiness = state.ReadinessUnknown
		next.UpdatedAt = timestamp
		opID := operationID
		next.CurrentOperationID = &opID
		m.events.Publish("service.lifecycle", lifecycleEvent(&next))
		m.state.Services[serviceID] = &next
		changed = true
	}
	m.stateMu.Unlock()
	if changed {
		m.persist()
	}
}

func (m *HearthManager) clearQueuedStarts(serviceIDs []string, operationID string) {
	m.lifecycle.Lock()
	defer m.lifecycle.Unlock()
	if m.closed.Load() {
		return
	}
	timestamp := now()
	m.stateMu.Lock()
	changed := false
	for _, serviceID := range serviceIDs {
		previous := m.state.Services[serviceID]
		if previous == nil {
			continue
		}
		if previous.ActualState != state.ActualQueuedStart {
			continue
		}
		// Always ownership-checked: only the operation that queued a service
		// may un-queue it. There is deliberately no "clear regardless" escape
		// hatch — one used to exist for the single-service start path and let
		// it cancel a concurrent bulk-start's queued services.
		if previous.CurrentOperationID == nil || *previous.CurrentOperationID != operationID {
			continue
		}
		next := *previous
		next.DesiredState = state.DesiredStopped
		next.ActualState = state.ActualStopped
		next.Readiness = state.ReadinessUnknown
		next.UpdatedAt = timestamp
		exitedAt := timestamp
		next.ExitedAt = &exitedAt
		next.CurrentOperationID = nil
		m.events.Publish("service.lifecycle", lifecycleEvent(&next))
		m.state.Services[serviceID] = &next
		changed = true
	}
	m.stateMu.Unlock()
	if changed {
		m.persist()
	}
}

func (m *HearthManager) getOperation(w http.ResponseWriter, _ *http.Request, params map[string]string) {
	if op := m.operations.Get(params["id"]); op != nil {
		writeJSON(w, http.StatusOK, operationResponse{Operation: op})
		return
	}
	writeHTTPError(w, newHTTPError(http.StatusNotFound, "operation_not_found", "Operation not found"))
}

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

func (m *HearthManager) getEvents(w http.ResponseWriter, r *http.Request, _ map[string]string) {
	q := r.URL.Query()
	after, herr := parseAfter(q)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	replay := m.events.ReplayEvents(after, q.Get("epoch"))
	events := replay.Events
	if events == nil {
		events = []state.ManagerEvent{}
	}
	writeJSON(w, http.StatusOK, eventsReplayResponse{
		Epoch:          replay.Epoch,
		Reset:          replay.Reset,
		Events:         events,
		LatestSequence: replay.LatestSequence,
	})
}

func (m *HearthManager) getEventsStream(w http.ResponseWriter, r *http.Request, _ map[string]string) {
	q := r.URL.Query()
	after, herr := parseAfter(q)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	flusher, ok := w.(http.Flusher)
	if !ok {
		writeHTTPError(w, newHTTPError(http.StatusInternalServerError, "internal_error", "streaming is not supported"))
		return
	}

	sink := newSSESink(sseMaxQueueFrames)
	gate := &ssePrefixGate{}
	epoch := q.Get("epoch")

	listener := func(event *state.ManagerEvent) {
		gate.mu.Lock()
		defer gate.mu.Unlock()
		if !gate.open {
			gate.pending = append(gate.pending, *event)
			return
		}
		sink.send(eventToSSE(event))
	}
	replay, unsubscribe := m.events.SubscribeAndReplay(after, epoch, listener, func(rep *Replay) bool {
		return !rep.Reset && len(rep.Events) < sseMaxQueueFrames
	})

	w.Header().Set("content-type", "text/event-stream")
	w.Header().Set("cache-control", "no-cache")
	w.WriteHeader(http.StatusOK)
	flusher.Flush()

	if unsubscribe == nil {
		// Reset, or a snapshot that would itself overflow the queue, is
		// terminal — no live subscription. The client refetches. Closing the
		// sink ends the stream after the one reset frame.
		sink.send(replayEvent(replay.Epoch, true, replay.LatestSequence))
		sink.close()
		writeSSEStream(w, flusher, sink.ch, r.Context())
		return
	}

	sink.send(replayEvent(replay.Epoch, false, replay.LatestSequence))
	for i := range replay.Events {
		sink.send(eventToSSE(&replay.Events[i]))
	}
	gate.mu.Lock()
	pending := gate.pending
	gate.pending = nil
	// Open before draining so a listener that acquires this lock next sends
	// *after* `pending`, which is written while the lock is still held.
	gate.open = true
	for i := range pending {
		sink.send(eventToSSE(&pending[i]))
	}
	gate.mu.Unlock()

	// On overflow the stream is CLOSED, not silently thinned. A client that
	// merely stops receiving some frames has no way to know it missed them.
	// Closing makes it reconnect with its cursor and take a `reset` replay.
	go func() {
		select {
		case <-r.Context().Done():
		case <-sink.overflow:
		}
		unsubscribe()
		sink.close()
	}()

	writeSSEStream(w, flusher, sink.ch, r.Context())
}

// ---------------------------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------------------------

func (m *HearthManager) getLogs(w http.ResponseWriter, r *http.Request, params map[string]string) {
	rawServiceID := params["id"]
	cat := m.Catalog()
	found := false
	for i := range cat.Services {
		if cat.Services[i].ID == rawServiceID {
			found = true
			break
		}
	}
	if !found {
		writeHTTPError(w, newHTTPError(http.StatusNotFound, "service_not_found", "Service is not in the catalog"))
		return
	}
	q := r.URL.Query()
	cursor, herr := optionalU64(q, "cursor", "invalid_cursor", "cursor must be a non-negative integer")
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	var limit *uint64
	if vals, ok := q["limit"]; ok {
		n, ok2 := parseU64(vals[0])
		if !ok2 || n < 1 {
			writeHTTPError(w, newHTTPError(http.StatusBadRequest, "invalid_limit", "limit must be a positive integer"))
			return
		}
		limit = &n
	}
	generation, herr := optionalU64(q, "generation", "invalid_generation", "generation must be a non-negative integer")
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	lifecycleGeneration := m.lifecycleGeneration(rawServiceID)
	slice := m.logs.Read(rawServiceID, cursor, limit, lifecycleGeneration, generation)
	writeJSON(w, http.StatusOK, slice)
}

// getDaemonLog — the daemon's own `daemon.log` is a plain rotating file, not
// the per-service CursorLogStore, so it gets its own route. Returns a
// LogSlice-shaped tail: `reset` is always true (the whole tail is returned
// each poll — a diagnostic pane, not an incremental cursor) and `nextCursor`
// carries the byte length so callers can detect rotation.
func (m *HearthManager) getDaemonLog(w http.ResponseWriter, r *http.Request, _ map[string]string) {
	q := r.URL.Query()
	bytesWanted := uint64(131_072)
	if vals, ok := q["bytes"]; ok {
		n, ok2 := parseU64(vals[0])
		if !ok2 || n < 1 || n > 1_048_576 {
			writeHTTPError(w, newHTTPError(http.StatusBadRequest, "invalid_bytes", "bytes must be an integer between 1 and 1048576"))
			return
		}
		bytesWanted = n
	}
	path := filepath.Join(m.runtimeDirectory, daemonLogName)
	size, tail, truncated, err := readLogTail(path, bytesWanted)
	if err != nil {
		writeHTTPError(w, newHTTPError(http.StatusInternalServerError, "daemon_log_unreadable", "could not read daemon.log: "+err.Error()))
		return
	}
	writeJSON(w, http.StatusOK, state.LogSlice{
		ServiceID:  "daemon",
		Generation: 0,
		Cursor:     0,
		NextCursor: size,
		Data:       tail,
		Reset:      true,
		Truncated:  truncated,
	})
}

// readLogTail returns the last `bytes` of the file at `path` as
// (file size, text, truncated) — seeks to the tail instead of reading the whole
// file on every poll. A missing file reads as empty.
func readLogTail(path string, bytesWanted uint64) (uint64, string, bool, error) {
	f, err := os.Open(path)
	if err != nil {
		if os.IsNotExist(err) {
			return 0, "", false, nil
		}
		return 0, "", false, err
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil {
		return 0, "", false, err
	}
	size := uint64(info.Size())
	start := uint64(0)
	if size > bytesWanted {
		start = size - bytesWanted
	}
	if _, err := f.Seek(int64(start), io.SeekStart); err != nil {
		return 0, "", false, err
	}
	raw, err := io.ReadAll(io.LimitReader(f, int64(bytesWanted)))
	if err != nil {
		return 0, "", false, err
	}
	// Snap the tail start to a UTF-8 boundary so a split codepoint is not
	// returned as text.
	skip := 0
	if start > 0 {
		for skip < len(raw) && raw[skip]&0xc0 == 0x80 {
			skip++
		}
	}
	return size, strings.ToValidUTF8(string(raw[skip:]), "\uFFFD"), start > 0 || skip > 0, nil
}

// ---------------------------------------------------------------------------------------------
// Reload
// ---------------------------------------------------------------------------------------------

// reloadRequestsKept is how many recent reload requestIds are remembered for
// replay.
const reloadRequestsKept = 32

func (m *HearthManager) postReload(w http.ResponseWriter, r *http.Request, _ map[string]string) {
	if herr := m.ensureNotClosing(); herr != nil {
		writeHTTPError(w, herr)
		return
	}
	body, herr := strictBody(r, []string{"requestId", "catalog"}, []string{"requestId", "catalog"})
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	requestID, herr := requireRequestID(body)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	catalogValue := body["catalog"]
	validShape := false
	if obj, ok := catalogValue.(map[string]any); ok {
		_, servicesOK := obj["services"].([]any)
		_, groupsOK := obj["groups"].(map[string]any)
		sfp, sfpPresent := obj["startFailurePolicy"]
		_, sfpIsString := sfp.(string)
		validShape = servicesOK && groupsOK && (!sfpPresent || sfpIsString)
	}
	if !validShape {
		writeHTTPError(w, newHTTPError(http.StatusBadRequest, "invalid_catalog", "catalog must be a ServiceCatalog: { services: [...], groups: {...}, startFailurePolicy? }"))
		return
	}
	catalogJSON, err := json.Marshal(catalogValue)
	if err != nil {
		writeHTTPError(w, newHTTPError(http.StatusBadRequest, "invalid_catalog", err.Error()))
		return
	}
	var next catalog.ServiceCatalog
	if err := json.Unmarshal(catalogJSON, &next); err != nil {
		writeHTTPError(w, newHTTPError(http.StatusBadRequest, "invalid_catalog", err.Error()))
		return
	}
	// Serialized with the reload itself so a retry arriving mid-reload waits
	// for, then replays, the original answer.
	m.reloadRequestsSerial.Lock()
	defer m.reloadRequestsSerial.Unlock()
	if replayed, herr := m.replayReload(requestID, catalogValue); herr != nil {
		writeHTTPError(w, herr)
		return
	} else if replayed != nil {
		writeJSON(w, http.StatusOK, replayed)
		return
	}
	outcome, err := m.ReloadCatalog(next)
	if err != nil {
		var re *ReloadError
		if errors.As(err, &re) {
			writeHTTPError(w, reloadErrorToHTTP(re))
		} else {
			writeHTTPError(w, newHTTPError(http.StatusInternalServerError, "internal_error", err.Error()))
		}
		return
	}
	response := &reloadResponse{Stopped: outcome.Stopped, Changed: outcome.Changed}
	m.reloadRequestsMu.Lock()
	if len(m.reloadRequests) >= reloadRequestsKept {
		m.reloadRequests = m.reloadRequests[1:]
	}
	m.reloadRequests = append(m.reloadRequests, reloadRequestEntry{requestID: requestID, catalog: catalogValue, response: response})
	m.reloadRequestsMu.Unlock()
	writeJSON(w, http.StatusOK, response)
}

// replayReload: a reload requestId seen before — the same catalog replays the
// original response, a different one is a conflict.
func (m *HearthManager) replayReload(requestID string, catalogValue any) (*reloadResponse, *ManagerHttpError) {
	m.reloadRequestsMu.Lock()
	defer m.reloadRequestsMu.Unlock()
	for _, e := range m.reloadRequests {
		if e.requestID != requestID {
			continue
		}
		if reflect.DeepEqual(e.catalog, catalogValue) {
			return e.response, nil
		}
		return nil, requestIDConflictError()
	}
	return nil, nil
}

// ---------------------------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------------------------

func (m *HearthManager) postShutdown(w http.ResponseWriter, r *http.Request, _ map[string]string) {
	body, herr := strictBody(r, []string{"requestId", "mode"}, []string{"requestId"})
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	requestID, herr := requireRequestID(body)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	mode := "refuse-if-active"
	if v, ok := body["mode"]; ok {
		s, isStr := v.(string)
		if !isStr || (s != "refuse-if-active" && s != "stop-services" && s != "leave-services") {
			writeHTTPError(w, newHTTPError(http.StatusBadRequest, "invalid_shutdown_mode", "mode must be refuse-if-active, stop-services, or leave-services"))
			return
		}
		mode = s
	}
	shutdownMode := LeaveServices
	if mode == "stop-services" {
		shutdownMode = StopServices
	}
	scheduleInput := OperationInput{
		RequestID: requestID,
		Kind:      state.OperationKindManagerShutdown,
	}
	if existing, err := m.operations.ResolveRequest(&scheduleInput); err != nil {
		writeHTTPError(w, requestIDConflictError())
		return
	} else if existing != nil {
		writeJSON(w, http.StatusAccepted, operationResponse{Operation: existing})
		return
	}
	if herr := m.ensureNotClosing(); herr != nil {
		writeHTTPError(w, herr)
		return
	}
	nonTerminal := map[state.ActualServiceState]bool{
		state.ActualStopped:         true,
		state.ActualQueuedStart:     true,
		state.ActualFailed:          true,
		state.ActualOrphaned:        true,
		state.ActualExternallyOwned: true,
	}
	cat := m.Catalog()
	active := false
	for _, s := range m.ServiceStates() {
		if defaultDaemonOwned(cat, s.ServiceID) && !nonTerminal[s.ActualState] {
			active = true
			break
		}
	}
	// Only `refuse-if-active` guards. `leave-services` is the deliberate
	// "restart the daemon, keep the services" path (`hearth manager restart`).
	if active && mode == "refuse-if-active" {
		writeHTTPError(w, newHTTPError(http.StatusConflict, "active_services", "Manager shutdown is refused while managed services are active"))
		return
	}
	go m.Shutdown(shutdownMode)
	operation, err := m.operations.Schedule(scheduleInput, func(*OperationHandle) *state.OperationError { return nil }, nil)
	if err != nil {
		writeHTTPError(w, requestIDConflictError())
		return
	}
	writeJSON(w, http.StatusAccepted, operationResponse{Operation: operation})
}
