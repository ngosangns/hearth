//! `ProcessSupervisor` — the daemon's process/container lifecycle engine. `engine` is the
//! start/stop/restart/readiness/adoption state machine, driven entirely through the adapter traits
//! in `types`; `default_adapters` is the production implementation of those traits (the real
//! `ps`/`lsof`/`docker`/`tailscale` shell-outs and spawning). `fingerprint` and `process_tree` are the
//! pure identity and whole-process-tree logic both sides share.

pub mod default_adapters;
pub mod engine;
pub mod fingerprint;
pub mod process_tree;
pub mod types;

pub use default_adapters::default_supervisor_options;
pub use engine::ProcessSupervisor;
pub use fingerprint::{command_argv, normalize_command_fingerprint, normalize_observed_command_fingerprint};
pub use process_tree::ProcessTreeEntry;
pub use types::{
    Host, ManagedProcess, OnOutput, OutputSource, PreparationAdapter, ProbeAdapter, ProcessAdapter, ProcessRecord,
    ProcessSignal, RunBuild, SpawnInput, SupervisorClock, SupervisorError, SupervisorOptions,
};
