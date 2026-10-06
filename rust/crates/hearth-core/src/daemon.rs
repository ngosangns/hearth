//! The daemon-*process* glue around a `HearthManager`: diagnostics logging, the losing-side
//! lock-takeover watch, and `run_daemon`'s signal and panic wiring.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;

use crate::manager::{
    bootstrap, BootstrapError, ClaimLockError, HearthManager, HearthManagerOptions, ShutdownMode,
};
use crate::paths::metadata_path;

const DAEMON_LOG_MAX_BYTES: u64 = 512 * 1024;
pub(crate) const DAEMON_LOG_NAME: &str = "daemon.log";
const LOCK_WATCH_INTERVAL: Duration = Duration::from_secs(2);

pub type DaemonLog = Arc<dyn Fn(&str) + Send + Sync>;

fn now() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    crate::supervisor::types::format_iso8601_millis(millis)
}

/// Daemon-level diagnostics sink. Whoever spawns a daemon usually detaches it with stdio ignored, so
/// without this file a crash (or a refused duplicate) leaves no trace at all. Keeps one rotated copy.
pub fn create_daemon_log(runtime_directory: &Path) -> DaemonLog {
    let path = runtime_directory.join(DAEMON_LOG_NAME);
    let rotated = runtime_directory.join(format!("{DAEMON_LOG_NAME}.1"));
    let runtime_directory = runtime_directory.to_path_buf();
    Arc::new(move |message: &str| {
        let _: Option<()> = (|| {
            std::fs::create_dir_all(&runtime_directory).ok()?;
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size > DAEMON_LOG_MAX_BYTES {
                let _ = std::fs::remove_file(&rotated);
                std::fs::rename(&path, &rotated).ok()?;
            }
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
            }
            writeln!(file, "{} {}", now(), message).ok()
        })();
    })
}

/// The lock's owner instance id: `Some("")` when the lock file is gone (quarantined or released),
/// `None` when it exists but could not be read/parsed — a transient read failure must not be
/// mistaken for losing the lock — and `Some(id)` otherwise.
pub fn read_lock_instance_id(runtime_directory: &Path) -> Option<String> {
    let raw = match std::fs::read_to_string(metadata_path(runtime_directory)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(String::new()),
        Err(_) => return None,
    };
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(value) => Some(
            value
                .get("instanceId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        Err(_) => None,
    }
}

/// Keeps the "one daemon per runtime directory" invariant enforced from the losing side. A daemon
/// whose lock was taken over must stop managing services: two live daemons would otherwise fight
/// over one state file. It never touches the winner's lock — it just leaves the field.
pub struct LockOwnershipWatch {
    runtime_directory: PathBuf,
    instance_id: String,
    on_lock_lost: Arc<dyn Fn() + Send + Sync>,
    interval: Duration,
    stopped: AtomicBool,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl LockOwnershipWatch {
    pub fn new(
        runtime_directory: PathBuf,
        instance_id: String,
        on_lock_lost: Arc<dyn Fn() + Send + Sync>,
        interval: Option<Duration>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime_directory,
            instance_id,
            on_lock_lost,
            interval: interval.unwrap_or(LOCK_WATCH_INTERVAL),
            stopped: AtomicBool::new(false),
            task: Mutex::new(None),
        })
    }

    pub fn start(self: &Arc<Self>) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let mut task = self.task.lock().unwrap();
        if task.is_some() {
            return;
        }
        let watch = self.clone();
        *task = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(watch.interval).await;
                watch.check();
            }
        }));
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(handle) = self.task.lock().unwrap().take() {
            handle.abort();
        }
    }

    pub fn check(&self) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        match read_lock_instance_id(&self.runtime_directory) {
            None => {} // unreadable — unknown, not lost
            Some(owner) if owner == self.instance_id => {}
            Some(_) => {
                self.stop();
                (self.on_lock_lost)();
            }
        }
    }
}

#[async_trait]
pub trait ShutdownManager: Send + Sync {
    async fn shutdown(self: Arc<Self>, mode: ShutdownMode);
    fn shutdown_completion(&self) -> watch::Receiver<bool>;
}

#[async_trait]
impl ShutdownManager for HearthManager {
    async fn shutdown(self: Arc<Self>, mode: ShutdownMode) {
        HearthManager::shutdown(&self, mode).await
    }
    fn shutdown_completion(&self) -> watch::Receiver<bool> {
        HearthManager::shutdown_completion(self)
    }
}

pub struct DaemonLifecycle<M: ShutdownManager + 'static> {
    manager: Arc<M>,
    mode: ShutdownMode,
    once: tokio::sync::OnceCell<()>,
}

impl<M: ShutdownManager + 'static> DaemonLifecycle<M> {
    pub fn new(manager: Arc<M>, mode: ShutdownMode) -> Self {
        Self {
            manager,
            mode,
            once: tokio::sync::OnceCell::new(),
        }
    }

    /// Triggered by SIGINT/SIGTERM. Default [`ShutdownMode::LeaveServices`] leaves already-running
    /// services alone — they're detached processes that outlive this daemon and get reconciled/
    /// re-adopted by the next one — so interrupting the daemon never kills a developer's in-flight
    /// work.
    pub async fn shutdown(&self) {
        let manager = self.manager.clone();
        let mode = self.mode;
        self.once
            .get_or_init(|| async move { manager.shutdown(mode).await })
            .await;
    }

    pub async fn wait_for_manager_shutdown(&self) {
        let mut rx = self.manager.shutdown_completion();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Routes every panic's message and location into `daemon.log` before the default hook runs.
///
/// A panic in a spawned task only unwinds that task, and almost every `JoinHandle` here is dropped
/// (readiness exit watchers, log forwarders, the `axum::serve` task) — so without this, a detached
/// daemon (stdio ignored) loses the panic entirely, and the daemon keeps running with whatever that
/// task was responsible for silently gone. Installed once per process.
pub fn install_panic_log(log: DaemonLog) {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            log(&format!(
                "panic in thread '{}': {info}",
                thread.name().unwrap_or("<unnamed>")
            ));
            previous(info);
        }));
    });
}

/// How a [`run_daemon`] call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonOutcome {
    /// Served until shutdown, then closed cleanly.
    Stopped,
    /// Another daemon already holds this root's lock and answers its health check, so this one
    /// never started. The caller exits non-zero and says so: it used to exit 0 with only a
    /// `daemon.log` line, which read as a successful start.
    AlreadyRunning { pid: i64, port: u16 },
    /// Bootstrap failed; the reason is in `daemon.log`.
    Failed,
}

/// Boots a `HearthManager`, wires SIGINT/SIGTERM to a graceful `DaemonLifecycle` shutdown,
/// and resolves once the manager has fully closed. A raced `ClaimLockError::AlreadyRunning`
/// (another daemon won the lock claim first) is [`DaemonOutcome::AlreadyRunning`].
#[cfg(unix)]
pub async fn run_daemon(options: HearthManagerOptions, mode: ShutdownMode) -> DaemonOutcome {
    let root = options
        .root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    let runtime_directory = options.runtime_directory.clone().unwrap_or_else(|| {
        crate::paths::resolve_runtime_directory(&root, options.catalog.runtime_directory.as_deref())
    });
    let log = create_daemon_log(&runtime_directory);
    install_panic_log(log.clone());

    let manager = match bootstrap(options).await {
        Ok(manager) => manager,
        Err(BootstrapError::ClaimLock(ClaimLockError::AlreadyRunning { metadata, port })) => {
            log(&format!(
                "bootstrap raced another daemon: pid {} already serves this root on port {port}",
                metadata.pid
            ));
            return DaemonOutcome::AlreadyRunning {
                pid: metadata.pid,
                port,
            };
        }
        Err(error) => {
            log(&format!("bootstrap failed: {error}"));
            return DaemonOutcome::Failed;
        }
    };
    log(&format!(
        "listening on 127.0.0.1:{}, root={}",
        manager.info().port,
        root.display()
    ));

    let lifecycle = Arc::new(DaemonLifecycle::new(manager.clone(), mode));

    let lock_watch = (read_lock_instance_id(&runtime_directory).as_deref()
        == Some(manager.instance_id.as_str()))
    .then(|| {
        let lifecycle_for_watch = lifecycle.clone();
        let log_for_watch = log.clone();
        let watch = LockOwnershipWatch::new(
            runtime_directory.clone(),
            manager.instance_id.clone(),
            Arc::new(move || {
                log_for_watch("shutdown: manager lock is owned by another daemon");
                let lifecycle = lifecycle_for_watch.clone();
                tokio::spawn(async move { lifecycle.shutdown().await });
            }),
            None,
        );
        watch.start();
        watch
    });

    {
        let lifecycle_for_signal = lifecycle.clone();
        let lock_watch_for_signal = lock_watch.clone();
        let log_for_signal = log.clone();
        tokio::spawn(async move {
            let mut sigint =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .expect("failed to install SIGINT handler");
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler");
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
            if let Some(watch) = &lock_watch_for_signal {
                watch.stop();
            }
            log_for_signal("shutdown: signal");
            lifecycle_for_signal.shutdown().await;
        });
    }
    // A detached daemon already runs in its own session with no controlling terminal, so a terminal
    // hangup should never reach it — but an explicit ignore means a stray SIGHUP can never fall back
    // to the default terminate-the-process behavior either.
    tokio::spawn(async move {
        let Ok(mut sighup) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        else {
            return;
        };
        loop {
            sighup.recv().await;
        }
    });

    lifecycle.wait_for_manager_shutdown().await;
    DaemonOutcome::Stopped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_lock_instance_id_is_empty_string_for_a_missing_lock() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_lock_instance_id(dir.path()), Some(String::new()));
    }

    #[test]
    fn read_lock_instance_id_is_none_for_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(metadata_path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(metadata_path(dir.path()), "not json {{{").unwrap();
        assert_eq!(read_lock_instance_id(dir.path()), None);
    }

    #[test]
    fn read_lock_instance_id_reads_the_real_instance_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(metadata_path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(metadata_path(dir.path()), r#"{"instanceId":"abc-123"}"#).unwrap();
        assert_eq!(
            read_lock_instance_id(dir.path()),
            Some("abc-123".to_string())
        );
    }

    #[test]
    fn daemon_log_writes_and_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let log = create_daemon_log(dir.path());
        log("hello");
        let content = std::fs::read_to_string(dir.path().join("daemon.log")).unwrap();
        assert!(content.contains("hello"));

        // Force rotation by writing well past the max size directly, then logging once more.
        std::fs::write(dir.path().join("daemon.log"), "x".repeat(600 * 1024)).unwrap();
        log("after rotation");
        assert!(dir.path().join("daemon.log.1").exists());
        let content = std::fs::read_to_string(dir.path().join("daemon.log")).unwrap();
        assert!(content.contains("after rotation"));
        assert!(
            !content.contains('x'),
            "the rotated content should not still be in the active log"
        );
    }

    #[test]
    fn lock_ownership_watch_reports_once_then_stops_on_takeover() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(metadata_path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(metadata_path(dir.path()), r#"{"instanceId":"me"}"#).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_clone = calls.clone();
        let watch = LockOwnershipWatch::new(
            dir.path().to_path_buf(),
            "me".to_string(),
            Arc::new(move || {
                calls_clone.fetch_add(1, Ordering::SeqCst);
            }),
            None,
        );

        watch.check();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "still owns the lock — should not report"
        );

        std::fs::write(
            metadata_path(dir.path()),
            r#"{"instanceId":"someone-else"}"#,
        )
        .unwrap();
        watch.check();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // A second check after the takeover was already reported must not fire again (the watch
        // stops itself on the first loss).
        watch.check();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn lock_ownership_watch_reports_on_lock_file_disappearing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(metadata_path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(metadata_path(dir.path()), r#"{"instanceId":"me"}"#).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_clone = calls.clone();
        let watch = LockOwnershipWatch::new(
            dir.path().to_path_buf(),
            "me".to_string(),
            Arc::new(move || {
                calls_clone.fetch_add(1, Ordering::SeqCst);
            }),
            None,
        );

        std::fs::remove_file(metadata_path(dir.path())).unwrap();
        watch.check();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn lock_ownership_watch_does_not_mistake_an_unreadable_lock_for_a_lost_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(metadata_path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(metadata_path(dir.path()), r#"{"instanceId":"me"}"#).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_clone = calls.clone();
        let watch = LockOwnershipWatch::new(
            dir.path().to_path_buf(),
            "me".to_string(),
            Arc::new(move || {
                calls_clone.fetch_add(1, Ordering::SeqCst);
            }),
            None,
        );

        std::fs::write(metadata_path(dir.path()), "not json at all").unwrap();
        watch.check();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "an unreadable (not missing) lock file must not be treated as lost"
        );
    }

    /// A second daemon for a root that already has a live one must report the loser, not a start.
    #[tokio::test]
    async fn a_daemon_that_loses_the_lock_race_reports_already_running() {
        let dir = tempfile::tempdir().unwrap();
        let options = || HearthManagerOptions {
            runtime_directory: Some(dir.path().join("runtime")),
            root: Some(dir.path().to_path_buf()),
            catalog: crate::catalog::ServiceCatalog {
                services: Vec::new(),
                groups: std::collections::HashMap::new(),
                group_tree: Vec::new(),
                compose_file: None,
                runtime_directory: None,
                start_failure_policy:
                    crate::catalog::StartFailurePolicy::StopOnFirstFailureKeepStarted,
                private_file_guard: Some(false),
            },
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
            shared: None,
        };
        let winner = bootstrap(options()).await.unwrap();
        let outcome = run_daemon(options(), ShutdownMode::LeaveServices).await;
        assert_eq!(
            outcome,
            DaemonOutcome::AlreadyRunning {
                pid: std::process::id() as i64,
                port: winner.info().port,
            }
        );
        winner.close().await;
    }

    #[tokio::test]
    async fn daemon_lifecycle_shutdown_is_memoized() {
        struct CountingManager {
            calls: std::sync::atomic::AtomicU32,
            done: watch::Sender<bool>,
        }
        #[async_trait]
        impl ShutdownManager for CountingManager {
            async fn shutdown(self: Arc<Self>, _mode: ShutdownMode) {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let _ = self.done.send(true);
            }
            fn shutdown_completion(&self) -> watch::Receiver<bool> {
                self.done.subscribe()
            }
        }
        let (tx, _rx) = watch::channel(false);
        let manager = Arc::new(CountingManager {
            calls: std::sync::atomic::AtomicU32::new(0),
            done: tx,
        });
        let lifecycle = DaemonLifecycle::new(manager.clone(), ShutdownMode::LeaveServices);
        lifecycle.shutdown().await;
        lifecycle.shutdown().await;
        assert_eq!(
            manager.calls.load(Ordering::SeqCst),
            1,
            "concurrent/repeated shutdown calls must only actually shut down once"
        );
        lifecycle.wait_for_manager_shutdown().await;
    }
}
