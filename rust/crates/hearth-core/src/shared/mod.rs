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
    RecipeReadiness, RemoteCatalog, SharedArtifact, SharedCatalogDocument, SharedConnection,
    SharedRecipe, SharedServiceFamily,
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

/// How long a command's output may stay open after it exits before what holds it is killed.
const OUTPUT_DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

/// SIGKILLs a process group on drop unless disarmed.
struct KillGroupGuard(Option<u32>);

impl KillGroupGuard {
    fn fire(&mut self) {
        if let Some(pgid) = self.0.take().filter(|pgid| *pgid > 1) {
            // SAFETY: plain syscall on our own child's process group. Only fired while the leader
            // is unreaped or something it started still holds its output (see below).
            unsafe {
                libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

impl Drop for KillGroupGuard {
    fn drop(&mut self) {
        self.fire();
    }
}

/// Spawns `command` in its own process group and waits up to `limit` for it. On timeout the whole
/// group — a packaging script's or recipe's grandchildren included — is SIGKILLed and `Ok(None)`
/// returned. Once the leader exits, its output gets `OUTPUT_DRAIN` to close; a pipe still open
/// after that is held by something it left running in its group (`server &`), which is
/// SIGKILLed with the group, and the output read so far is returned. The wait used to sit on that
/// pipe until `limit` (49 minutes for a pack script) and then skip the kill, because `child.id()`
/// is `None` once the leader is reaped — the grandchild outlived the daemon. The group is only
/// signalled while the leader is unreaped or a pipe is open, so it is still populated and its pgid
/// cannot have been recycled. The same guard covers a caller whose future is dropped mid-wait.
pub(crate) async fn output_with_timeout(
    mut command: tokio::process::Command,
    limit: std::time::Duration,
) -> std::io::Result<Option<std::process::Output>> {
    command.kill_on_drop(true).process_group(0);
    let mut child = command.spawn()?;
    let mut group = KillGroupGuard(child.id());
    let mut read_out = AbortOnDrop(tokio::spawn(read_pipe(child.stdout.take())));
    let mut read_err = AbortOnDrop(tokio::spawn(read_pipe(child.stderr.take())));
    let wait = async {
        let status = child.wait().await?;
        let mut stdout = None;
        let mut stderr = None;
        let drained = tokio::time::timeout(OUTPUT_DRAIN, async {
            collect_read(&mut read_out, &mut stdout).await;
            collect_read(&mut read_err, &mut stderr).await;
        })
        .await;
        if drained.is_err() {
            group.fire();
            // The holders were just killed: collect what was read, briefly.
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
                collect_read(&mut read_out, &mut stdout).await;
                collect_read(&mut read_err, &mut stderr).await;
            })
            .await;
        }
        let stdout = stdout.unwrap_or_default();
        let stderr = stderr.unwrap_or_default();
        Ok::<_, std::io::Error>(std::process::Output {
            status,
            stdout,
            stderr,
        })
    };
    match tokio::time::timeout(limit, wait).await {
        Ok(result) => {
            group.0 = None;
            result.map(Some)
        }
        Err(_) => {
            group.fire();
            let _ = child.kill().await;
            Ok(None)
        }
    }
}

async fn read_pipe<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut buf).await;
    }
    buf
}

/// Awaits `reader` into `slot` unless it already completed: a finished `JoinHandle` must not be
/// polled again.
async fn collect_read(reader: &mut AbortOnDrop<Vec<u8>>, slot: &mut Option<Vec<u8>>) {
    if slot.is_none() {
        *slot = Some((&mut reader.0).await.unwrap_or_default());
    }
}

/// Aborts a reader task when dropped.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
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
    /// exists because a daemon started outside a login shell cannot reliably carry the env var —
    /// its callers (`hearth tui`, `hearth shared ensure`, a project daemon's `attach` task) all
    /// run it detached with whatever environment they happened to inherit.
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn alive(pid: i64) -> bool {
        crate::platform::is_pid_alive(pid)
    }

    /// A packaging script that leaves `server &` holding its stdout: the wait used to run out the
    /// whole limit and then skip the kill, because the leader was already reaped.
    #[tokio::test]
    async fn output_with_timeout_kills_a_child_left_holding_the_output() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("bg.pid");
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!(
                "echo packed; sleep 300 & echo $! > '{}'; exit 0",
                pid_file.display()
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let started = Instant::now();
        let output = output_with_timeout(command, Duration::from_secs(60))
            .await
            .unwrap()
            .expect("the command itself finished");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "packed");
        let bg: i64 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && alive(bg) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let survived = alive(bg);
        if survived {
            unsafe {
                libc::kill(bg as libc::pid_t, libc::SIGKILL);
            }
        }
        assert!(!survived, "the leftover pipe holder must be killed");
    }

    #[tokio::test]
    async fn output_with_timeout_kills_the_whole_group_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("bg.pid");
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(format!(
                "sleep 300 >/dev/null 2>&1 & echo $! > '{}'; sleep 300",
                pid_file.display()
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let result = output_with_timeout(command, Duration::from_millis(800))
            .await
            .unwrap();
        assert!(result.is_none(), "timed out");
        let bg: i64 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && alive(bg) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(!alive(bg), "the timeout must take the grandchild too");
    }
}
