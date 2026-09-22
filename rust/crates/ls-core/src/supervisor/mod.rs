//! Port of `src/core/supervisor.ts` — `ProcessSupervisor`, the daemon's process/container
//! lifecycle engine. **Work in progress (Phase 2 of the Rust-rewrite plan)**: the two most
//! sharp-edge-dense, highest-risk pieces (command fingerprinting and whole-process-tree signalling)
//! are ported and tested first, including a real OS-level regression test, before the stateful
//! `ProcessSupervisor` struct itself (start/stop/restart/readiness-probing/adoption state machine)
//! is built on top of them — see AGENTS.md's "Rust rewrite" section for current status.

pub mod default_adapters;
pub mod engine;
pub mod fingerprint;
pub mod process_tree;
pub mod types;

pub use default_adapters::default_supervisor_options;
pub use engine::ProcessSupervisor;
pub use fingerprint::{command_argv, normalize_command_fingerprint, normalize_observed_command_fingerprint};
pub use process_tree::{
    build_process_tree, parse_ps_alive_rows, parse_ps_tree_rows, process_tree_alive, secondary_process_groups,
    ProcessTreeEntry, PsTreeRow,
};
pub use types::{
    Host, ManagedProcess, ObservedProcess, OnOutput, OutputSource, PreparationAdapter, ProbeAdapter, ProcessAdapter,
    ProcessRecord, ProcessSignal, RunBuild, SpawnInput, SupervisorClock, SupervisorError, SupervisorOptions,
    SystemClock,
};
