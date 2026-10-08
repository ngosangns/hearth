// ManagerEventStore — a fixed-capacity ring buffer of manager events with
// sequence/epoch-based replay semantics. HTTP-agnostic: the bounded
// per-client queue / drop-and-reset-on-overflow logic lives in the SSE layer.
package manager

import (
	"sync"
	"time"

	"github.com/google/uuid"
	"github.com/ngosangns/hearth/go/internal/iso8601"
	"github.com/ngosangns/hearth/go/internal/state"
)

const DefaultEventCapacity = 256

func eventNow() string {
	return iso8601.FormatMillis(time.Now().UnixMilli())
}

type EventListener func(*state.ManagerEvent)

type Replay struct {
	Epoch          string
	Reset          bool
	Events         []state.ManagerEvent
	LatestSequence uint64
}

type inner struct {
	nextSequence   uint64
	ring           []state.ManagerEvent // circular buffer of `capacity`
	head           int                  // index of oldest event
	count          int
	listeners      map[uint64]EventListener
	nextListenerID uint64
}

type ManagerEventStore struct {
	capacity int
	Epoch    string
	mu       sync.Mutex
	inner    inner
}

func NewEventStore(capacity int, epoch string) *ManagerEventStore {
	if capacity <= 0 {
		capacity = DefaultEventCapacity
	}
	if epoch == "" {
		epoch = uuid.NewString()
	}
	return &ManagerEventStore{
		capacity: capacity,
		Epoch:    epoch,
		inner: inner{
			nextSequence: 1,
			ring:         make([]state.ManagerEvent, capacity),
			listeners:    map[uint64]EventListener{},
		},
	}
}

func (s *ManagerEventStore) Publish(eventType string, data map[string]any) state.ManagerEvent {
	s.mu.Lock()
	event := state.ManagerEvent{
		Sequence: s.inner.nextSequence,
		At:       eventNow(),
		Type:     eventType,
		Data:     data,
	}
	s.inner.nextSequence++
	// O(1) ring eviction: overwrite the oldest slot once full.
	idx := (s.inner.head + s.inner.count) % s.capacity
	if s.inner.count == s.capacity {
		idx = s.inner.head
		s.inner.head = (s.inner.head + 1) % s.capacity
	} else {
		s.inner.count++
	}
	s.inner.ring[idx] = event
	listeners := make([]EventListener, 0, len(s.inner.listeners))
	for _, l := range s.inner.listeners {
		listeners = append(listeners, l)
	}
	s.mu.Unlock()
	// Call listeners after releasing the lock — a listener enqueueing into an
	// SSE stream must never be able to deadlock against another publish.
	for _, l := range listeners {
		l(&event)
	}
	return event
}

// iterLocked yields buffered events oldest→newest.
func (s *ManagerEventStore) iterLocked() []state.ManagerEvent {
	out := make([]state.ManagerEvent, 0, s.inner.count)
	for i := 0; i < s.inner.count; i++ {
		out = append(out, s.inner.ring[(s.inner.head+i)%s.capacity])
	}
	return out
}

// Subscribe registers a listener; returns an unsubscribe func.
func (s *ManagerEventStore) Subscribe(listener EventListener) func() {
	_, unsub := s.SubscribeAndReplay(nil, "", listener, func(*Replay) bool { return true })
	return unsub
}

// SubscribeAndReplay copies the replay and, when subscribeIf accepts it,
// registers listener — both under the same lock Publish holds. Registering
// before the copy would deliver an event also in the snapshot; registering
// after the lock would drop one in neither. A rejected snapshot does not
// subscribe — the caller resynchronizes from the returned buffer instead.
func (s *ManagerEventStore) SubscribeAndReplay(afterSequence *uint64, epoch string, listener EventListener, subscribeIf func(*Replay) bool) (*Replay, func()) {
	s.mu.Lock()
	replay := s.replayLocked(afterSequence, epoch)
	if !subscribeIf(replay) {
		s.mu.Unlock()
		return replay, nil
	}
	id := s.inner.nextListenerID
	s.inner.nextListenerID++
	s.inner.listeners[id] = listener
	s.mu.Unlock()
	return replay, func() {
		s.mu.Lock()
		delete(s.inner.listeners, id)
		s.mu.Unlock()
	}
}

func (s *ManagerEventStore) ReplayEvents(afterSequence *uint64, epoch string) *Replay {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.replayLocked(afterSequence, epoch)
}

func (s *ManagerEventStore) replayLocked(afterSequence *uint64, cursorEpoch string) *Replay {
	buffered := s.iterLocked()
	var oldestSequence uint64
	if len(buffered) > 0 {
		oldestSequence = buffered[0].Sequence
	} else {
		oldestSequence = s.inner.nextSequence
	}
	latestSequence := uint64(0)
	if s.inner.nextSequence > 0 {
		latestSequence = s.inner.nextSequence - 1
	}
	epochMismatch := cursorEpoch != "" && cursorEpoch != s.Epoch
	outOfRange := false
	if afterSequence != nil {
		after := *afterSequence
		outOfRange = (oldestSequence > 0 && after+1 < oldestSequence) || after > latestSequence
	}
	reset := epochMismatch || outOfRange
	// A reset returns the WHOLE buffer, not nothing: `reset` tells the client
	// its cursor is unusable, and the reply is the full snapshot it needs to
	// resynchronize from.
	var events []state.ManagerEvent
	if afterSequence != nil && !reset {
		for _, e := range buffered {
			if e.Sequence > *afterSequence {
				events = append(events, e)
			}
		}
	} else {
		events = buffered
	}
	return &Replay{
		Epoch:          s.Epoch,
		Reset:          reset,
		Events:         events,
		LatestSequence: latestSequence,
	}
}
