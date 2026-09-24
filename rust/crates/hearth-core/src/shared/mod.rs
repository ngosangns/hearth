//! Shared services — the machine-global `smp` (service manager) side of the feature documented in
//! `docs/shared-services.md`. Common infrastructure (postgres, redis, …) is installed from prebuilt
//! tarballs into an isolated prefix under `~/.hearth/shared` and run as one singleton per
//! `name@version`, shared by every project that registers the same key in its `hearth.yaml`.
//!
//! This module is the state/IO layer: the remote registry client (`remote`), the local instance
//! registry (`registry`), port allocation (`ports`), the tarball installer (`install`), recipe
//! template rendering (`render`), and smp's in-memory catalog synthesis (`synthesize`). The HTTP
//! orchestration that drives these lives in `crate::manager::shared`.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::Digest;

use crate::file_io::{create_file_io, FileIo};

pub mod install;
pub mod ports;
pub mod registry;
pub mod remote;
pub mod render;
pub mod synthesize;

pub use registry::{InstallState, SharedAttachment, SharedInstance, SharedRegistry};
pub use remote::{RemoteCatalog, SharedArtifact, SharedCatalogDocument, SharedConnection, SharedRecipe, SharedServiceFamily};

/// The smp daemon's root directory: `~/.hearth/shared`. Unlike project daemons this root contains
/// no `hearth.yaml` — the manager's catalog is synthesized from `registry.json` instead.
pub fn shared_root() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    home.join(".hearth").join("shared")
}

/// The smp runtime directory lives directly under its root (not the default
/// `.hearth/runtime-v1` nested inside it), because `~/.hearth/shared` already IS the
/// hearth-namespaced directory.
pub const SHARED_RUNTIME_DIRECTORY_NAME: &str = "runtime-v1";

/// Shared instances bind inside their own range, away from both well-known dev ports and macOS's
/// ephemeral range (49152–65535). `hash(name@version)` picks the base slot deterministically.
pub const SHARED_PORT_RANGE_START: u16 = 43100;
pub const SHARED_PORT_RANGE_SIZE: u16 = 900;

/// The pinned remote registry. HTTPS is the trust boundary — tarball sha256s live in this file,
/// so a signed/forked copy would defeat them anyway.
pub const SHARED_CATALOG_URL: &str = "https://raw.githubusercontent.com/ngosangns/hearth/main/catalog.json";

/// v1 supports exactly one artifact platform.
pub const SHARED_ARTIFACT_PLATFORM: &str = "darwin-arm64";

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SharedError(pub String);

impl From<&str> for SharedError {
    fn from(value: &str) -> Self {
        SharedError(value.to_string())
    }
}
impl From<String> for SharedError {
    fn from(value: String) -> Self {
        SharedError(value)
    }
}

/// `name@version` — the instance id everywhere: registry key, smp catalog service id, CLI arg.
pub fn instance_id(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

/// Stable identity of a registering project: sha256 of the canonicalized project root, truncated.
/// A moved/renamed project gets a new id — its old provisioned resources are orphaned (documented
/// trade-off; `shared remove` is the cleanup path).
pub fn project_id(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let digest = sha2::Sha256::digest(canonical.to_string_lossy().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Per-project logical resource names handed to a recipe's `provision`/`connection` templates.
/// The `h_`/`u_` prefixes keep the values valid as database identifiers (leading letter, <=63
/// bytes, lowercase).
pub fn project_db_name(project: &str) -> String {
    format!("h_{project}")
}
pub fn project_user_name(project: &str) -> String {
    format!("u_{project}")
}

/// Everything the smp daemon's HTTP handlers need that isn't already on `HearthManager`: the
/// instance registry, the remote catalog client, and per-instance serialization for
/// install/attach so two projects racing the same `name@version` can't double-install.
pub struct SharedContext {
    pub root: PathBuf,
    pub io: Arc<dyn FileIo>,
    pub registry: Arc<SharedRegistry>,
    pub remote: Arc<RemoteCatalog>,
    pub http: reqwest::Client,
    instance_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl SharedContext {
    pub fn open(root: PathBuf, catalog_url: Option<String>) -> Result<Arc<Self>, SharedError> {
        let io: Arc<dyn FileIo> = Arc::from(create_file_io(true));
        io.ensure_directory(&root).map_err(|e| SharedError(e.to_string()))?;
        let registry = SharedRegistry::load(io.clone(), &root)?;
        Ok(Arc::new(Self {
            remote: Arc::new(RemoteCatalog::new(&root, catalog_url)),
            registry: Arc::new(registry),
            io,
            root,
            http: reqwest::Client::new(),
            instance_locks: std::sync::Mutex::new(HashMap::new()),
        }))
    }

    pub fn runtime_directory(&self) -> PathBuf {
        self.root.join(SHARED_RUNTIME_DIRECTORY_NAME)
    }

    pub fn installs_dir(&self) -> PathBuf {
        self.root.join("installs")
    }

    pub fn instances_dir(&self) -> PathBuf {
        self.root.join("instances")
    }

    pub fn downloads_dir(&self) -> PathBuf {
        self.root.join("downloads")
    }

    /// Serializes install/start/provision for one instance across concurrent attaches. Locks are
    /// keyed by instance id and kept forever — the map is bounded by the number of distinct
    /// `name@version` keys a machine ever registers, which is tiny.
    pub fn instance_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.instance_locks
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}
