// OperationScheduler — schedules and serializes operations per target service
// (serviceId, or every id in targetServiceIds for a bulk start), publishing
// operation.accepted/operation.updated events as they progress. Manager-wide
// work such as shutdown still serializes on "__manager__"; ordinary
// multi-service starts do not.
package manager

import (
	"sort"
	"sync"
	"sync/atomic"
	"time"

	"github.com/google/uuid"
	"github.com/ngosangns/hearth/go/internal/iso8601"
	"github.com/ngosangns/hearth/go/internal/state"
	"github.com/ngosangns/hearth/go/internal/syncx"
)

// Settled operations stay readable (GET /v1/operations/:id) and idempotent
// for the TTL; a burst evicts oldest-first rather than waiting it out.
const settledOperationTTL = 10 * time.Minute
const maxSettledOperations = 1024

func opNow() string { return iso8601.FormatMillis(time.Now().UnixMilli()) }

type OperationInput struct {
	RequestID        string
	Kind             state.OperationKind
	ServiceID        *string
	TargetServiceIDs *[]string
	Action           *state.ServiceOperationKind
}

type RequestIDConflict struct{}

func (RequestIDConflict) Error() string { return "requestId is already used by a different operation" }

// OperationHandle is the shared mutable operation record.
type OperationHandle struct {
	mu  sync.Mutex
	Op  state.Operation
}

func (h *OperationHandle) Snapshot() state.Operation {
	h.mu.Lock()
	defer h.mu.Unlock()
	return h.Op
}

// OperationExecute runs the operation; OperationRejected runs when the op is
// abandoned before starting (release dropped, or manager closing).
type OperationExecute func(*OperationHandle) *state.OperationError
type OperationRejected func(*OperationHandle)

type OperationScheduler struct {
	events        *ManagerEventStore
	closing       atomic.Bool
	mu            sync.Mutex // guards operations/requestIDs/activeTargets/done/settled
	operations    map[string]*OperationHandle
	requestIDs    map[string]*OperationHandle
	activeTargets map[string]string
	queues        *syncx.KeyedLock[string]
	// done[id] closes when the operation's worker finishes — waiters key off
	// this rather than racing to (re-)acquire the target's KeyedLock: spawning
	// the worker only schedules it, and a naive wait could return before the
	// operation even began running.
	done    map[string]chan struct{}
	settled []settledEntry
}

type settledEntry struct {
	at time.Time
	id string
}

func NewOperationScheduler(events *ManagerEventStore) *OperationScheduler {
	return &OperationScheduler{
		events:        events,
		operations:    map[string]*OperationHandle{},
		requestIDs:    map[string]*OperationHandle{},
		activeTargets: map[string]string{},
		queues:        syncx.NewKeyedLock[string](),
		done:          map[string]chan struct{}{},
	}
}

// targetsFor returns lock keys for an operation, sorted and deduped so
// overlapping multi-target ops cannot deadlock. A single-service op locks
// that service; a bulk op locks every target; manager-wide work with neither
// still uses __manager__.
func targetsFor(serviceID *string, targetServiceIDs *[]string) []string {
	if serviceID != nil {
		return []string{*serviceID}
	}
	if targetServiceIDs != nil && len(*targetServiceIDs) > 0 {
		keys := append([]string{}, (*targetServiceIDs)...)
		sort.Strings(keys)
		out := keys[:0]
		for i, k := range keys {
			if i == 0 || keys[i-1] != k {
				out = append(out, k)
			}
		}
		return out
	}
	return []string{state.ManagerTargetServiceID}
}

func targetsOf(op *state.Operation) []string {
	return targetsFor(op.ServiceID, op.TargetServiceIDs)
}

func sameServiceIDs(l, r *[]string) bool {
	if l == nil && r == nil {
		return true
	}
	if l == nil || r == nil || len(*l) != len(*r) {
		return false
	}
	for i := range *l {
		if (*l)[i] != (*r)[i] {
			return false
		}
	}
	return true
}

// settle records id as settled and evicts every settled operation past the
// TTL or the cap.
func (s *OperationScheduler) settle(id string) {
	now := time.Now()
	s.mu.Lock()
	defer s.mu.Unlock()
	s.settled = append(s.settled, settledEntry{now, id})
	var evicted []string
	for len(s.settled) > 0 {
		front := s.settled[0]
		if now.Sub(front.at) <= settledOperationTTL && len(s.settled) <= maxSettledOperations {
			break
		}
		evicted = append(evicted, front.id)
		s.settled = s.settled[1:]
	}
	for _, eid := range evicted {
		delete(s.done, eid)
		handle, ok := s.operations[eid]
		if !ok {
			continue
		}
		delete(s.operations, eid)
		snapshot := handle.Snapshot()
		// Only if the request id still maps to this operation — never evict a
		// newer one.
		if current, ok := s.requestIDs[snapshot.RequestID]; ok && current == handle {
			delete(s.requestIDs, snapshot.RequestID)
		}
	}
}

func (s *OperationScheduler) Get(id string) *state.Operation {
	s.mu.Lock()
	handle, ok := s.operations[id]
	s.mu.Unlock()
	if !ok {
		return nil
	}
	snap := handle.Snapshot()
	return &snap
}

func (s *OperationScheduler) IsQueued(op *state.Operation) bool {
	targets := targetsOf(op)
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, target := range targets {
		if s.activeTargets[target] == op.ID {
			return true
		}
	}
	return false
}

func (s *OperationScheduler) CloseMutations() { s.closing.Store(true) }

// DrainServices waits for every currently in-flight service-targeted
// operation (excludes the manager-wide target) — used by shutdown to let
// in-flight work settle before stopping services.
func (s *OperationScheduler) DrainServices() {
	s.mu.Lock()
	seen := map[string]bool{}
	var chans []chan struct{}
	for target, id := range s.activeTargets {
		if target != state.ManagerTargetServiceID && !seen[id] {
			seen[id] = true
			if ch, ok := s.done[id]; ok {
				chans = append(chans, ch)
			}
		}
	}
	s.mu.Unlock()
	for _, ch := range chans {
		<-ch
	}
}

func (s *OperationScheduler) Wait(op *state.Operation) {
	s.mu.Lock()
	ch, ok := s.done[op.ID]
	s.mu.Unlock()
	if ok {
		<-ch
	}
}

// ResolveRequest returns the existing operation for a requestID, or a
// conflict when the id maps to a different operation.
func (s *OperationScheduler) ResolveRequest(input *OperationInput) (*state.Operation, error) {
	s.mu.Lock()
	handle, ok := s.requestIDs[input.RequestID]
	s.mu.Unlock()
	if !ok {
		return nil, nil
	}
	snapshot := handle.Snapshot()
	if snapshot.Kind != input.Kind ||
		!sameOptStr(snapshot.ServiceID, input.ServiceID) ||
		!sameOptAction(snapshot.Action, input.Action) ||
		!sameServiceIDs(snapshot.TargetServiceIDs, input.TargetServiceIDs) {
		return nil, RequestIDConflict{}
	}
	return &snapshot, nil
}

func sameOptStr(a, b *string) bool {
	if a == nil && b == nil {
		return true
	}
	if a == nil || b == nil {
		return false
	}
	return *a == *b
}

func sameOptAction(a, b *state.ServiceOperationKind) bool {
	if a == nil && b == nil {
		return true
	}
	if a == nil || b == nil {
		return false
	}
	return *a == *b
}

// Schedule creates+runs the operation immediately.
func (s *OperationScheduler) Schedule(input OperationInput, execute OperationExecute, rejected OperationRejected) (*state.Operation, error) {
	op, _, err := s.scheduleInner(input, execute, rejected, false)
	return op, err
}

// ScheduleWhenReleased is like Schedule, but when this call *creates* the
// operation the worker does not observe state until release receives a value.
// Callers that publish `desired: running` before the start worker runs hold
// that channel across the publish: `release <- struct{}{}` releases it;
// `close(release)` without a send abandons it — the rejected callback runs and
// the operation fails, so a row queued for it is not left behind. An
// idempotent replay of an existing request returns nil — that worker is
// already released.
func (s *OperationScheduler) ScheduleWhenReleased(input OperationInput, execute OperationExecute, rejected OperationRejected) (*state.Operation, chan struct{}, error) {
	return s.scheduleInner(input, execute, rejected, true)
}

func (s *OperationScheduler) scheduleInner(input OperationInput, execute OperationExecute, rejected OperationRejected, deferRelease bool) (*state.Operation, chan struct{}, error) {
	if existing, err := s.ResolveRequest(&input); err != nil {
		return nil, nil, err
	} else if existing != nil {
		return existing, nil, nil
	}
	createdAt := opNow()
	operation := state.Operation{
		ID:               uuid.NewString(),
		RequestID:        input.RequestID,
		Kind:             input.Kind,
		ServiceID:        input.ServiceID,
		TargetServiceIDs: input.TargetServiceIDs,
		Action:           input.Action,
		Status:           state.OpStatusQueued,
		CreatedAt:        createdAt,
		UpdatedAt:        createdAt,
		Trace: []state.OperationTraceEntry{{
			At:      createdAt,
			Message: "Operation accepted",
		}},
	}
	handle := &OperationHandle{Op: operation}
	doneCh := make(chan struct{})
	s.mu.Lock()
	s.operations[operation.ID] = handle
	s.requestIDs[input.RequestID] = handle
	s.done[operation.ID] = doneCh
	s.mu.Unlock()

	eventData := map[string]any{
		"operationId": operation.ID,
		"requestId":   operation.RequestID,
		"serviceId":   nil,
	}
	if operation.ServiceID != nil {
		eventData["serviceId"] = *operation.ServiceID
	}
	s.events.Publish("operation.accepted", eventData)

	targets := targetsFor(operation.ServiceID, operation.TargetServiceIDs)
	s.mu.Lock()
	for _, target := range targets {
		s.activeTargets[target] = operation.ID
	}
	s.mu.Unlock()

	// A buffered channel so the releaser's send never blocks: send = released,
	// close-without-send = abandoned (Rust's oneshot: send → released, drop →
	// abandoned).
	var release chan struct{}
	if deferRelease {
		release = make(chan struct{}, 1)
	}
	scheduler := s
	kind := operation.Kind
	operationID := operation.ID
	go func() {
		defer close(doneCh)
		defer scheduler.settle(operationID)
		// Only clear slots that are still OURS. A later operation on a shared
		// target overwrites that entry, and removing it unconditionally made
		// drain see an empty map and return while the newer operation was
		// still running — so shutdown raced a start in flight.
		defer func() {
			scheduler.mu.Lock()
			for _, target := range targets {
				if scheduler.activeTargets[target] == operationID {
					delete(scheduler.activeTargets, target)
				}
			}
			scheduler.mu.Unlock()
		}()

		if release != nil {
			if _, ok := <-release; !ok {
				if rejected != nil {
					rejected(handle)
				}
				scheduler.transition(handle, state.OpStatusFailed, "Operation abandoned before it started")
				return
			}
		}
		// Acquire every target lock in sorted order so overlapping
		// multi-service ops cannot deadlock. Disjoint sets run in parallel.
		var unlocks []func()
		for _, target := range targets {
			unlocks = append(unlocks, scheduler.queues.Lock(target))
		}
		defer func() {
			for _, u := range unlocks {
				u()
			}
		}()
		isClosing := scheduler.closing.Load()
		if isClosing && (kind == state.OperationKindService || kind == state.OperationKindBulkStart) {
			if rejected != nil {
				rejected(handle)
			}
			handle.mu.Lock()
			handle.Op.Error = &state.OperationError{Code: "manager_closing", Message: "Manager is shutting down"}
			handle.mu.Unlock()
			scheduler.transition(handle, state.OpStatusFailed, "Operation rejected because manager is shutting down")
			return
		}
		scheduler.transition(handle, state.OpStatusRunning, "Operation started")
		if opErr := execute(handle); opErr == nil {
			scheduler.transition(handle, state.OpStatusSucceeded, "Operation completed")
		} else {
			message := "Operation failed: " + opErr.Message
			handle.mu.Lock()
			handle.Op.Error = opErr
			handle.mu.Unlock()
			scheduler.transition(handle, state.OpStatusFailed, message)
		}
	}()
	return &operation, release, nil
}

func (s *OperationScheduler) Trace(handle *OperationHandle, message string) {
	handle.mu.Lock()
	handle.Op.UpdatedAt = opNow()
	at := handle.Op.UpdatedAt
	handle.Op.Trace = append(handle.Op.Trace, state.OperationTraceEntry{At: at, Message: message})
	id := handle.Op.ID
	serviceID := handle.Op.ServiceID
	status := handle.Op.Status
	handle.mu.Unlock()
	// Publish the operation's REAL status — a trace entry appended while an
	// operation is queued or settled must not tell subscribers it is running.
	s.publishUpdated(id, status, serviceID)
}

func (s *OperationScheduler) transition(handle *OperationHandle, status state.OperationStatus, message string) {
	handle.mu.Lock()
	handle.Op.Status = status
	handle.Op.UpdatedAt = opNow()
	at := handle.Op.UpdatedAt
	handle.Op.Trace = append(handle.Op.Trace, state.OperationTraceEntry{At: at, Message: message})
	id := handle.Op.ID
	serviceID := handle.Op.ServiceID
	handle.mu.Unlock()
	s.publishUpdated(id, status, serviceID)
}

func (s *OperationScheduler) publishUpdated(id string, status state.OperationStatus, serviceID *string) {
	var sid any
	if serviceID != nil {
		sid = *serviceID
	}
	s.events.Publish("operation.updated", map[string]any{
		"operationId": id,
		"status":      string(status),
		"serviceId":   sid,
	})
}
