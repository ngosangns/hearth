//! The HTTP+SSE daemon (`HearthManager`), its lock-claim protocol, log rotation/tailing, and state
//! persistence. The stores are isolated and testable without an HTTP server; `http` wires them
//! together behind the route table, and `shared` adds the smp-only `/v1/shared/*` surface.

pub mod event_store;
pub mod http;
pub mod lock;
pub mod log_store;
pub mod operations;
pub mod protocol;
pub mod service_urls;
pub mod shared;
pub mod state_store;

pub use event_store::{ManagerEventStore, Replay, DEFAULT_EVENT_CAPACITY};
pub use http::{
    bootstrap, router, BootstrapError, HearthManager, HearthManagerOptions, ManagerHttpError,
    ReloadError, ReloadOutcome, StopServicesOnDrop,
};
pub use lock::{
    claim_lock, create_lock_ownership_proof, is_stale_lock_marker, random_token,
    read_lock_ownership_key, read_owned_lock_artifacts, verify_lock_ownership_proof,
    ClaimLockError, LockHandle, OwnedLockArtifacts,
};
pub use log_store::{
    CursorLogStore, LogStoreError, DEFAULT_LOG_MAX_BYTES, DEFAULT_LOG_ROTATION_COUNT,
    DEFAULT_LOG_TAIL_BYTES,
};
pub use operations::{
    OperationExecute, OperationHandle, OperationInput, OperationRejected, OperationScheduler,
    RequestIdConflict,
};
pub use protocol::ShutdownMode;
pub use state_store::AtomicStateStore;
