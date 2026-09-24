//! `hearth-core` — originally a Rust port of this project's `src/core` TypeScript subpath (that
//! TypeScript implementation has since been fully dropped; see `AGENTS.md`). Catalog/state types,
//! path helpers, platform primitives, the guarded/plain file-IO layer, daemon base-environment
//! resolution, the declarative config-file loader, the doctor check engine, `ProcessSupervisor`,
//! and the daemon's own manager live here.

pub mod catalog;
pub mod config_file;
pub mod daemon;
pub mod doctor;
pub mod env;
pub mod file_io;
pub mod manager;
pub mod paths;
pub mod platform;
pub mod shared;
pub mod state;
pub mod supervisor;

pub use catalog::{
    is_container_command, validate_catalog, CatalogValidation, CommandSpec, ReadinessProbeContext,
    ReadinessSpec, ServiceBuildProfile, ServiceCatalog, ServiceCommand, ServiceDefinition,
    ServiceId, ServiceKind, ServiceOwnership, ServicePort, ServiceProfiles, ServiceRunProfile,
    StartFailurePolicy,
};
