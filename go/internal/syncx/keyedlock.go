// Package syncx ports Rust's KeyedLock: one mutex per key, created on first
// use — "run at most one operation at a time per key, queue the rest". Entries
// are pruned once no holder or waiter remains, so streams of one-off keys do
// not accumulate.
package syncx

import "sync"

type keyedEntry struct {
	mu   sync.Mutex
	refs int // holders + queued waiters, only touched under KeyedLock.mu
}

// KeyedLock serializes work per key.
type KeyedLock[K comparable] struct {
	mu    sync.Mutex
	locks map[K]*keyedEntry
}

func NewKeyedLock[K comparable]() *KeyedLock[K] {
	return &KeyedLock[K]{locks: map[K]*keyedEntry{}}
}

// Lock acquires key's lock. The returned func releases it and prunes the entry
// when nobody else holds or waits on it. refs is bumped under the map lock, so
// a waiter can never resurrect an entry mid-delete (the Rust version achieves
// the same with the Arc strong count).
func (k *KeyedLock[K]) Lock(key K) func() {
	k.mu.Lock()
	e, ok := k.locks[key]
	if !ok {
		e = &keyedEntry{}
		k.locks[key] = e
	}
	e.refs++
	k.mu.Unlock()

	e.mu.Lock()

	return func() {
		e.mu.Unlock()
		k.mu.Lock()
		e.refs--
		if e.refs == 0 {
			delete(k.locks, key)
		}
		k.mu.Unlock()
	}
}

func (k *KeyedLock[K]) Run(key K, work func()) {
	unlock := k.Lock(key)
	defer unlock()
	work()
}

func (k *KeyedLock[K]) Len() int {
	k.mu.Lock()
	defer k.mu.Unlock()
	return len(k.locks)
}
