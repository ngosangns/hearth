//! Port of `src/core/manager.ts` — the HTTP+SSE daemon (`LocalServicesManager`), its lock-claim
//! protocol, log rotation/tailing, and state persistence. Phase 3 of the Rust-rewrite plan. Built
//! bottom-up: the isolated stores first (testable without an HTTP server), then the HTTP layer
//! itself wiring them together — see AGENTS.md for current status.

pub mod event_store;
pub mod operations;

pub use event_store::{ManagerEventStore, Replay, DEFAULT_EVENT_CAPACITY};
pub use operations::{OperationExecute, OperationHandle, OperationInput, OperationRejected, OperationScheduler, RequestIdConflict};
