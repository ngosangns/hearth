//! Port of `src/core/manager.ts` — the HTTP+SSE daemon (`LocalServicesManager`), its lock-claim
//! protocol, log rotation/tailing, and state persistence. Phase 3 of the Rust-rewrite plan. Built
//! bottom-up: the isolated stores first (testable without an HTTP server), then the HTTP layer
//! itself wiring them together — see AGENTS.md for current status.

pub mod event_store;
pub mod http;
pub mod lock;
pub mod log_store;
pub mod operations;
pub mod state_store;

pub use event_store::{ManagerEventStore, Replay, DEFAULT_EVENT_CAPACITY};
pub use http::{bootstrap, router, BootstrapError, LocalServicesManager, LocalServicesManagerOptions, ManagerHttpError, ReloadOutcome};
pub use lock::{claim_lock, ClaimLockError, LockHandle, OwnedLockArtifacts};
pub use log_store::{CursorLogStore, LogStoreError, DEFAULT_LOG_MAX_BYTES, DEFAULT_LOG_ROTATION_COUNT, DEFAULT_LOG_TAIL_BYTES};
pub use operations::{OperationExecute, OperationHandle, OperationInput, OperationRejected, OperationScheduler, RequestIdConflict};
pub use state_store::AtomicStateStore;
