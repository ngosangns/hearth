//! `ls-core` — Rust port of `@gnasdev/local-services`'s `src/core` subpath. Phase 1 of the
//! Rust-rewrite migration plan: catalog/state types, path helpers, platform primitives, the
//! guarded/plain file-IO layer, daemon base-environment resolution, the declarative config-file
//! loader, and the doctor check engine. `ProcessSupervisor` and `LocalServicesManager` (the daemon
//! itself) are Phase 2/3, not yet in this crate.

pub mod catalog;
pub mod config_file;
pub mod daemon;
pub mod doctor;
pub mod env;
pub mod file_io;
pub mod manager;
pub mod paths;
pub mod platform;
pub mod state;
pub mod supervisor;

pub use catalog::{
    is_container_command, validate_catalog, CatalogValidation, CommandSpec, ReadinessProbeContext,
    ReadinessSpec, ServiceBuildProfile, ServiceCatalog, ServiceCommand, ServiceDefinition,
    ServiceId, ServiceKind, ServiceOwnership, ServicePort, ServiceProfiles, ServiceRunProfile,
    StartFailurePolicy,
};
