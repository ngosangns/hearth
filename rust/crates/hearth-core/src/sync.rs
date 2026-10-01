//! Small async synchronization helpers shared across the crate.
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard};

/// One `tokio` mutex per key, created on first use — "run at most one operation at a time per key,
/// queue the rest". Callers key by something with a small, bounded domain (service ids, instance
/// ids, serialization keys).
///
/// An entry taken through `lock`/`run` is dropped again once its last holder or waiter is gone, so
/// a stream of one-off keys (operation targets for since-removed services) does not accumulate.
/// `get` hands out the raw mutex for callers that manage the guard themselves; that entry is kept.
pub struct KeyedLock<K: Eq + Hash + Clone> {
    locks: Mutex<HashMap<K, Arc<tokio::sync::Mutex<()>>>>,
}

impl<K: Eq + Hash + Clone> Default for KeyedLock<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash + Clone> KeyedLock<K> {
    pub fn new() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// The map only ever holds `Arc`s, so a panic elsewhere while it was locked cannot have left it
    /// half-updated — recover the guard rather than turning one panic into a panic on every later
    /// lock of every key.
    fn map(&self) -> MutexGuard<'_, HashMap<K, Arc<tokio::sync::Mutex<()>>>> {
        self.locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The lock for `key`; hold its guard across the critical section.
    pub fn get(&self, key: &K) -> Arc<tokio::sync::Mutex<()>> {
        self.map()
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Waits for `key`'s lock. Dropping the guard releases it and prunes the entry when nobody else
    /// is holding or waiting on it.
    pub async fn lock(&self, key: &K) -> KeyedLockGuard<'_, K> {
        let mutex = self.get(key);
        let guard = mutex.clone().lock_owned().await;
        KeyedLockGuard {
            owner: self,
            key: key.clone(),
            mutex,
            guard: Some(guard),
        }
    }

    /// Runs `work` while holding `key`'s lock.
    pub async fn run<F, Fut, T>(&self, key: &K, work: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let _guard = self.lock(key).await;
        work().await
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map().len()
    }
}

pub struct KeyedLockGuard<'a, K: Eq + Hash + Clone> {
    owner: &'a KeyedLock<K>,
    key: K,
    mutex: Arc<tokio::sync::Mutex<()>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl<K: Eq + Hash + Clone> Drop for KeyedLockGuard<'_, K> {
    fn drop(&mut self) {
        drop(self.guard.take());
        let mut map = self.owner.map();
        // Two references left means the map's and ours: no holder, no waiter (a waiter holds its own
        // clone from `get`, and `get` needs this same map lock to take one).
        if map
            .get(&self.key)
            .is_some_and(|entry| Arc::ptr_eq(entry, &self.mutex))
            && Arc::strong_count(&self.mutex) == 2
        {
            map.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_uncontended_key_is_pruned_once_released() {
        let locks: KeyedLock<String> = KeyedLock::new();
        locks.run(&"a".to_string(), || async {}).await;
        assert_eq!(locks.len(), 0);
    }

    #[tokio::test]
    async fn a_waiting_key_is_kept_until_the_last_waiter_is_done() {
        let locks: Arc<KeyedLock<String>> = Arc::new(KeyedLock::new());
        let key = "a".to_string();
        let first = locks.lock(&key).await;
        let waiter = {
            let locks = locks.clone();
            let key = key.clone();
            tokio::spawn(async move { locks.run(&key, || async {}).await })
        };
        // Let the waiter queue up behind `first`.
        while Arc::strong_count(&first.mutex) < 4 {
            tokio::task::yield_now().await;
        }
        drop(first);
        assert_eq!(locks.len(), 1, "a queued waiter keeps the entry alive");
        waiter.await.unwrap();
        assert_eq!(locks.len(), 0);
    }

    #[tokio::test]
    async fn the_same_key_is_mutually_exclusive() {
        let locks: Arc<KeyedLock<String>> = Arc::new(KeyedLock::new());
        let key = "a".to_string();
        let guard = locks.lock(&key).await;
        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(20), locks.lock(&key)).await;
        assert!(blocked.is_err(), "a second lock of a held key must wait");
        drop(guard);
        let _second = locks.lock(&key).await;
    }
}
