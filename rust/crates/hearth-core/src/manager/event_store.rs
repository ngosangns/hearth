//! `ManagerEventStore` — a fixed-capacity ring buffer of manager events with sequence/epoch-based
//! replay semantics. Deliberately simple and HTTP-agnostic: the bounded per-client queue /
//! drop-and-reset-on-overflow logic lives in the HTTP/SSE layer, built as a thin adapter on top of
//! this store's plain `subscribe`/`publish`/`replay`.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use uuid::Uuid;

use crate::state::ManagerEvent;
use crate::supervisor::types::format_iso8601_millis;

pub const DEFAULT_EVENT_CAPACITY: usize = 256;

fn now() -> String {
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    format_iso8601_millis(millis)
}

pub type EventListener = Arc<dyn Fn(&ManagerEvent) + Send + Sync>;

pub struct Replay {
    pub epoch: String,
    pub reset: bool,
    pub events: Vec<ManagerEvent>,
    pub latest_sequence: u64,
}

struct Inner {
    next_sequence: u64,
    events: VecDeque<ManagerEvent>,
    listeners: HashMap<u64, EventListener>,
    next_listener_id: u64,
}

pub struct ManagerEventStore {
    capacity: usize,
    pub epoch: String,
    inner: Mutex<Inner>,
}

impl ManagerEventStore {
    pub fn new(capacity: Option<usize>, epoch: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            capacity: capacity.unwrap_or(DEFAULT_EVENT_CAPACITY),
            epoch: epoch.unwrap_or_else(|| Uuid::new_v4().to_string()),
            inner: Mutex::new(Inner { next_sequence: 1, events: VecDeque::new(), listeners: HashMap::new(), next_listener_id: 0 }),
        })
    }

    pub fn publish(&self, event_type: &str, data: serde_json::Map<String, serde_json::Value>) -> ManagerEvent {
        let (event, listeners) = {
            let mut inner = self.inner.lock().unwrap();
            let event = ManagerEvent { sequence: inner.next_sequence, at: now(), event_type: event_type.to_string(), data };
            inner.next_sequence += 1;
            // A `VecDeque` so evicting the oldest event is O(1), not a shift of the whole buffer on
            // every publish once it is full.
            inner.events.push_back(event.clone());
            while inner.events.len() > self.capacity {
                inner.events.pop_front();
            }
            let listeners: Vec<_> = inner.listeners.values().cloned().collect();
            (event, listeners)
        };
        // Call out to listeners after releasing the lock — a listener enqueueing into an SSE stream
        // must never be able to deadlock against another thread trying to publish.
        for listener in listeners {
            listener(&event);
        }
        event
    }

    /// Returns an unsubscribe handle — call it (or drop it) to stop receiving events.
    pub fn subscribe(self: &Arc<Self>, listener: EventListener) -> Box<dyn FnOnce() + Send> {
        let (_, unsubscribe) = self.subscribe_and_replay(None, None, listener, |_| true);
        unsubscribe.expect("subscribe always registers")
    }

    /// Copies the replay and, when `subscribe_if` accepts it, registers `listener`, both under the
    /// same lock `publish` holds. Registering before the copy would deliver an event that is also
    /// in the snapshot. Registering after releasing the lock would drop an event that is in
    /// neither. A reset (or any rejected snapshot) does not subscribe — the caller resynchronizes
    /// from the returned buffer instead.
    pub fn subscribe_and_replay(self: &Arc<Self>, after_sequence: Option<u64>, epoch: Option<&str>, listener: EventListener, subscribe_if: impl FnOnce(&Replay) -> bool) -> (Replay, Option<Box<dyn FnOnce() + Send>>) {
        let mut inner = self.inner.lock().unwrap();
        let replay = Self::replay_locked(&self.epoch, &inner, after_sequence, epoch);
        if !subscribe_if(&replay) {
            return (replay, None);
        }
        let id = inner.next_listener_id;
        inner.next_listener_id += 1;
        inner.listeners.insert(id, listener);
        drop(inner);
        let store = self.clone();
        (replay, Some(Box::new(move || {
            store.inner.lock().unwrap().listeners.remove(&id);
        })))
    }

    #[cfg(test)]
    fn subscriber_count(&self) -> usize {
        self.inner.lock().unwrap().listeners.len()
    }

    pub fn replay(&self, after_sequence: Option<u64>, epoch: Option<&str>) -> Replay {
        let inner = self.inner.lock().unwrap();
        Self::replay_locked(&self.epoch, &inner, after_sequence, epoch)
    }

    fn replay_locked(epoch: &str, inner: &Inner, after_sequence: Option<u64>, cursor_epoch: Option<&str>) -> Replay {
        let oldest_sequence = inner.events.front().map(|e| e.sequence).unwrap_or(inner.next_sequence);
        let latest_sequence = inner.next_sequence.saturating_sub(1);
        let epoch_mismatch = cursor_epoch.is_some_and(|e| e != epoch);
        let out_of_range = after_sequence.is_some_and(|after| {
            (oldest_sequence > 0 && after < oldest_sequence - 1) || after > latest_sequence
        });
        let reset = epoch_mismatch || out_of_range;
        // A reset returns the WHOLE buffer, not nothing: `reset` tells the client its cursor is
        // unusable (wrong epoch, or a sequence the ring buffer has already evicted), and the reply
        // is the full snapshot it needs to resynchronize from. Returning an empty vec left a
        // client with a stale cursor no way to recover — it saw `reset: true` and no events.
        let events = match after_sequence {
            Some(after) if !reset => inner.events.iter().filter(|e| e.sequence > after).cloned().collect(),
            _ => inner.events.iter().cloned().collect(),
        };
        Replay { epoch: epoch.to_string(), reset, events, latest_sequence }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data() -> serde_json::Map<String, serde_json::Value> {
        json!({}).as_object().unwrap().clone()
    }

    #[test]
    fn publish_assigns_increasing_sequence_numbers() {
        let store = ManagerEventStore::new(None, None);
        let a = store.publish("a", data());
        let b = store.publish("b", data());
        assert_eq!(a.sequence, 1);
        assert_eq!(b.sequence, 2);
    }

    #[test]
    fn replay_with_no_cursor_returns_everything_buffered() {
        let store = ManagerEventStore::new(None, None);
        store.publish("a", data());
        store.publish("b", data());
        let replay = store.replay(None, None);
        assert!(!replay.reset);
        assert_eq!(replay.events.len(), 2);
        assert_eq!(replay.latest_sequence, 2);
    }

    #[test]
    fn replay_after_a_cursor_returns_only_newer_events() {
        let store = ManagerEventStore::new(None, None);
        store.publish("a", data());
        let b = store.publish("b", data());
        let replay = store.replay(Some(1), None);
        assert!(!replay.reset);
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.events[0].sequence, b.sequence);
    }

    /// A reset hands back the whole buffer, which is the point of the flag: the client's cursor is
    /// unusable, so it needs the full snapshot to resynchronize from. This test previously asserted
    /// `events.is_empty()`, pinning a real bug — a client with a stale cursor got `reset: true` and
    /// nothing to reset *to*, while the TS source (`src/core/manager.ts`'s `replay`) returns every
    /// buffered event in the same situation.
    #[test]
    fn replay_resets_on_epoch_mismatch_and_returns_the_whole_buffer() {
        let store = ManagerEventStore::new(None, Some("epoch-a".to_string()));
        store.publish("a", data());
        store.publish("b", data());
        let replay = store.replay(None, Some("epoch-b"));
        assert!(replay.reset);
        assert_eq!(replay.events.len(), 2, "a reset must return the full snapshot, not an empty list");
    }

    #[test]
    fn replay_returns_the_whole_buffer_when_the_cursor_is_out_of_range() {
        let store = ManagerEventStore::new(Some(2), None);
        for _ in 0..5 {
            store.publish("a", data());
        }
        let replay = store.replay(Some(1), None);
        assert!(replay.reset);
        assert_eq!(replay.events.len(), 2, "everything still buffered, so the client can resynchronize");
    }

    #[test]
    fn replay_resets_when_cursor_is_out_of_range_after_the_buffer_wraps() {
        let store = ManagerEventStore::new(Some(2), None);
        for _ in 0..5 {
            store.publish("a", data());
        }
        // Capacity 2 keeps only the last 2 events (sequences 4, 5); a cursor from before that window
        // can never be satisfied incrementally.
        let replay = store.replay(Some(1), None);
        assert!(replay.reset);
    }

    #[test]
    fn replay_resets_when_cursor_is_beyond_the_latest_sequence() {
        let store = ManagerEventStore::new(None, None);
        store.publish("a", data());
        let replay = store.replay(Some(100), None);
        assert!(replay.reset);
    }

    #[test]
    fn ring_buffer_respects_capacity() {
        let store = ManagerEventStore::new(Some(3), None);
        for _ in 0..10 {
            store.publish("a", data());
        }
        let replay = store.replay(None, None);
        assert_eq!(replay.events.len(), 3);
        assert_eq!(replay.events.last().unwrap().sequence, 10);
    }

    #[test]
    fn subscribe_receives_published_events_and_unsubscribe_stops_delivery() {
        let store = ManagerEventStore::new(None, None);
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();
        let unsubscribe = store.subscribe(Arc::new(move |event: &ManagerEvent| {
            received_clone.lock().unwrap().push(event.sequence);
        }));
        store.publish("a", data());
        assert_eq!(store.subscriber_count(), 1);
        unsubscribe();
        store.publish("b", data());
        assert_eq!(*received.lock().unwrap(), vec![1]);
        assert_eq!(store.subscriber_count(), 0);
    }

    #[test]
    fn subscribe_and_replay_includes_the_snapshot_once_and_live_events_after_it() {
        let store = ManagerEventStore::new(None, None);
        store.publish("a", data());
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();
        let (replay, unsubscribe) = store.subscribe_and_replay(None, None, Arc::new(move |event: &ManagerEvent| {
            received_clone.lock().unwrap().push(event.sequence);
        }), |replay| !replay.reset);
        let unsubscribe = unsubscribe.expect("an in-range snapshot subscribes");
        assert_eq!(replay.events.iter().map(|event| event.sequence).collect::<Vec<_>>(), vec![1]);
        store.publish("b", data());
        assert_eq!(*received.lock().unwrap(), vec![2], "the snapshot event must not be delivered again");
        unsubscribe();
        store.publish("c", data());
        assert_eq!(*received.lock().unwrap(), vec![2]);
    }

    #[test]
    fn subscribe_and_replay_does_not_subscribe_when_the_snapshot_is_rejected() {
        let store = ManagerEventStore::new(None, Some("epoch-a".to_string()));
        store.publish("a", data());
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();
        let (replay, unsubscribe) = store.subscribe_and_replay(None, Some("epoch-b"), Arc::new(move |event: &ManagerEvent| {
            received_clone.lock().unwrap().push(event.sequence);
        }), |replay| !replay.reset);
        assert!(replay.reset);
        assert!(unsubscribe.is_none());
        assert_eq!(replay.events.len(), 1, "a reset still returns the whole buffer");
        store.publish("b", data());
        assert!(received.lock().unwrap().is_empty());
        assert_eq!(store.subscriber_count(), 0);
    }
}
