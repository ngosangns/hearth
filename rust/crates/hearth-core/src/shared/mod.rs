//! Shared services — the machine-global `smp` (service manager) side of the feature documented in
//! `docs/shared-services.md`. Common infrastructure (postgres, redis, …) is installed from prebuilt
//! tarballs into an isolated prefix under `~/.hearth/shared` and run as one singleton per
//! `name@version`, shared by every project that registers the same key in its `hearth.yaml`.
//!
//! This module is the state/IO layer: the remote registry client (`remote`), the local instance
//! registry (`registry`), port allocation (`ports`), the tarball installer (`install`), recipe
//! template rendering (`render`), and smp's in-memory catalog synthesis (`synthesize`). The HTTP
//! orchestration that drives these lives in `crate::manager::shared`.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::Digest;

use crate::file_io::{create_file_io, FileIo};
use crate::sync::KeyedLock;

pub mod install;
pub mod ports;
pub mod registry;
pub mod remote;
pub mod render;
pub mod synthesize;

pub use registry::{InstallState, SharedAttachment, SharedInstance, SharedRegistry};
pub use remote::{
    RemoteCatalog, SharedArtifact, SharedCatalogDocument, SharedConnection, SharedRecipe,
    SharedServiceFamily,
};

/// The smp daemon's root directory: `~/.hearth/shared`. Unlike project daemons this root contains
/// no `hearth.yaml` — the manager's catalog is synthesized from `registry.json` instead.
pub fn shared_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
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
pub const SHARED_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/ngosangns/hearth/main/catalog.json";

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
    hex(&digest[..8])
}

/// Lowercase hex of `bytes` — sha256 digests and project ids.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Spawns `command` in its own process group and waits up to `limit` for it. On timeout the whole
/// group — a packaging script's or recipe's grandchildren included — is SIGKILLed and `Ok(None)`
/// returned, but only while the leader is still unreaped: `child.id()` goes `None` once it is
/// reaped, and a group whose leader pid may have been recycled is never signalled. `kill_on_drop`
/// covers a caller whose future is itself dropped mid-wait.
pub(crate) async fn output_with_timeout(
    mut command: tokio::process::Command,
    limit: std::time::Duration,
) -> std::io::Result<Option<std::process::Output>> {
    command.kill_on_drop(true).process_group(0);
    let mut child = command.spawn()?;
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let wait = async {
        let read_out = async move {
            let mut buf = Vec::new();
            if let Some(mut pipe) = out_pipe {
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut buf).await;
            }
            buf
        };
        let read_err = async move {
            let mut buf = Vec::new();
            if let Some(mut pipe) = err_pipe {
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut buf).await;
            }
            buf
        };
        let (status, stdout, stderr) = tokio::join!(child.wait(), read_out, read_err);
        status.map(|status| std::process::Output { status, stdout, stderr })
    };
    match tokio::time::timeout(limit, wait).await {
        Ok(result) => result.map(Some),
        Err(_) => {
            if let Some(pid) = child.id() {
                // SAFETY: plain syscall; `pid` is our own unreaped child's process group.
                unsafe {
                    libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            Ok(None)
        }
    }
}

/// Runs blocking filesystem work on the blocking pool instead of a runtime worker.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, SharedError> + Send + 'static,
) -> Result<T, SharedError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| SharedError(format!("blocking task failed: {e}")))?
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
/// S3 bucket names reject `_`. Same identity as `{projectDb}`, with a hyphen so MinIO accepts it.
pub fn project_bucket_name(project: &str) -> String {
    format!("h-{project}")
}

/// Everything the smp daemon's HTTP handlers need that isn't already on `HearthManager`: the
/// instance registry, the remote catalog client, per-instance serialization for install/attach so
/// two projects racing the same `name@version` can't double-install, and one machine-wide port
/// allocation lock so two *different* new instances can't be handed the same port.
pub struct SharedContext {
    pub root: PathBuf,
    pub io: Arc<dyn FileIo>,
    pub registry: Arc<SharedRegistry>,
    pub remote: Arc<RemoteCatalog>,
    pub http: reqwest::Client,
    instance_locks: KeyedLock<String>,
    /// Held across read-taken → bind-probe → registry insert. Per-instance locks don't cover it:
    /// concurrent attaches of two new ids whose hash slots are close can both pass the bind probe
    /// (neither has bound yet) and be handed the same port.
    pub port_allocation: tokio::sync::Mutex<()>,
}

impl SharedContext {
    /// Catalog URL precedence: the explicit argument (`HEARTH_SHARED_CATALOG_URL`) first, then a
    /// `catalog-url` file under the shared root, then the pinned `SHARED_CATALOG_URL`. The file
    /// exists because a GUI-spawned smp daemon cannot reliably carry the env var — its callers
    /// (the app's `shared ensure`, a project daemon's `attach` task) all run it detached with
    /// whatever environment they happened to inherit.
    pub fn open(root: PathBuf, catalog_url: Option<String>) -> Result<Arc<Self>, SharedError> {
        let io: Arc<dyn FileIo> = Arc::from(create_file_io(true));
        io.ensure_directory(&root)
            .map_err(|e| SharedError(e.to_string()))?;
        let catalog_url = catalog_url.or_else(|| {
            std::fs::read_to_string(root.join("catalog-url"))
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
        });
        let registry = SharedRegistry::load(io.clone(), &root)?;
        Ok(Arc::new(Self {
            remote: Arc::new(RemoteCatalog::new(&root, catalog_url)),
            registry: Arc::new(registry),
            io,
            root,
            http: reqwest::Client::new(),
            instance_locks: KeyedLock::new(),
            port_allocation: tokio::sync::Mutex::new(()),
        }))
    }

    pub fn runtime_directory(&self) -> PathBuf {
        self.root.join(SHARED_RUNTIME_DIRECTORY_NAME)
    }

    pub fn downloads_dir(&self) -> PathBuf {
        self.root.join("downloads")
    }

    /// Serializes install/start/provision for one instance across concurrent attaches. Locks are
    /// keyed by instance id and kept forever — the map is bounded by the number of distinct
    /// `name@version` keys a machine ever registers, which is tiny.
    pub fn instance_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.instance_locks.get(&id.to_string())
    }
}
