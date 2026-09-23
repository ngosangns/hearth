//! Port of the lock-ownership-proof crypto and the `claimLock` protocol (`src/core/manager.ts`).
//!
//! **Sharp edge (AGENTS.md / the Rust-rewrite plan's top risk list)**: liveness is the one question
//! this protocol must never answer with a false negative. A production incident (263 concurrent
//! daemons under load) was caused by treating a health-check *timeout* as proof a manager was dead.
//! The rule this module encodes: only a **confirmed-dead PID** or an explicit, signed release marker
//! ever makes a lock stale — a slow or errored health check does not. Do not "simplify" this to a
//! timeout-based check.
use std::path::{Path, PathBuf};
use std::time::Duration;

use hmac::{Hmac, Mac};
use rand::RngCore;
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::file_io::FileIo;
use crate::paths::{lock_dir, metadata_path, ownership_key_path, proof_path, token_path};
use crate::platform::is_pid_alive;
use crate::state::{LockOwnershipProof, ManagerMetadata, StaleLockAction, StaleLockMarker, LOCK_OWNERSHIP_PROOF_NAME, LOCK_RELEASE_MARKER_NAME, STALE_LOCK_MARKER_NAME};

const MANAGER_STARTUP_GRACE_MS: u64 = 5_000;
const LIVE_MANAGER_HEALTHCHECK_ATTEMPTS: u32 = 2;
const LIVE_MANAGER_HEALTHCHECK_RETRY_DELAY_MS: u64 = 150;
const LIVE_MANAGER_HEALTHCHECK_TIMEOUT_MS: u64 = 1_500;

fn now_millis() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// Shared with `http.rs`'s bearer-token check — the daemon's token gates every `/v1` route and
/// `/healthz`'s `instanceId`, so it deserves the same treatment as the ownership proof next door.
pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn is_valid_manager_metadata(metadata: &ManagerMetadata) -> bool {
    metadata.version == 1 && metadata.protocol_version >= 1 && !metadata.instance_id.is_empty() && metadata.pid >= 1 && !metadata.started_at.is_empty()
}

fn is_valid_ownership_proof(proof: &LockOwnershipProof) -> bool {
    proof.version == 1 && is_valid_manager_metadata(&proof.metadata) && is_hex64(&proof.token_digest) && is_hex64(&proof.signature)
}

fn ownership_payload(metadata: &ManagerMetadata, token: &str) -> String {
    serde_json::json!({
        "metadata": {
            "instanceId": metadata.instance_id,
            "pid": metadata.pid,
            "port": metadata.port,
            "protocolVersion": metadata.protocol_version,
            "startedAt": metadata.started_at,
            "version": metadata.version,
        },
        "tokenDigest": sha256_hex(token.as_bytes()),
    })
    .to_string()
}

fn ownership_signature(key: &str, metadata: &ManagerMetadata, token: &str) -> String {
    hmac_sha256_hex(key.as_bytes(), ownership_payload(metadata, token).as_bytes())
}

pub fn create_lock_ownership_proof(key: &str, metadata: &ManagerMetadata, token: &str) -> LockOwnershipProof {
    LockOwnershipProof { version: 1, metadata: metadata.clone(), token_digest: sha256_hex(token.as_bytes()), signature: ownership_signature(key, metadata, token) }
}

pub fn verify_lock_ownership_proof(key: Option<&str>, metadata: &ManagerMetadata, token: &str, proof: &LockOwnershipProof) -> bool {
    let Some(key) = key else { return false };
    if !is_valid_ownership_proof(proof) || &proof.metadata != metadata {
        return false;
    }
    let expected_digest = sha256_hex(token.as_bytes());
    let expected_signature = ownership_signature(key, metadata, token);
    constant_time_eq(&proof.token_digest, &expected_digest) && constant_time_eq(&proof.signature, &expected_signature)
}

/// Port of `isStaleLockMarker` — used by `localctl`'s `cleanup` command to confirm a quarantined
/// `manager.lock.stale-*` directory is one this same ownership key actually produced (matching the
/// proof it was quarantined with) before deleting it, rather than deleting an unrelated lookalike.
pub fn is_stale_lock_marker(marker: &StaleLockMarker, key: &str, metadata: &ManagerMetadata, token: &str, expected_proof: &LockOwnershipProof) -> bool {
    marker.version == 1
        && matches!(marker.action, StaleLockAction::StaleLock)
        && &marker.original == metadata
        && &marker.proof == expected_proof
        && verify_lock_ownership_proof(Some(key), metadata, token, expected_proof)
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64_url_no_pad(&bytes)
}

fn base64_url_no_pad(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

async fn ownership_key(io: &dyn FileIo, runtime_directory: &Path) -> Result<String, String> {
    io.ensure_directory(runtime_directory).map_err(|e| e.to_string())?;
    let path = ownership_key_path(runtime_directory);
    if let Some(existing) = io.read_file(&path).map_err(|e| e.to_string())? {
        let trimmed = existing.trim();
        if trimmed.is_empty() {
            return Err("Refusing empty ownership key".to_string());
        }
        return Ok(trimmed.to_string());
    }
    let generated = random_token();
    if io.create_exclusive(&path, &generated).map_err(|e| e.to_string())? {
        return Ok(generated);
    }
    let concurrent = io.read_file(&path).map_err(|e| e.to_string())?.map(|s| s.trim().to_string());
    match concurrent {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err("Refusing unsafe ownership key".to_string()),
    }
}

pub fn read_lock_ownership_key(io: &dyn FileIo, runtime_directory: &Path) -> Option<String> {
    let key = io.read_file(&ownership_key_path(runtime_directory)).ok().flatten()?;
    let trimmed = key.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[derive(Clone)]
pub struct OwnedLockArtifacts {
    pub metadata: ManagerMetadata,
    pub token: String,
    pub proof: LockOwnershipProof,
}

pub fn read_owned_lock_artifacts(io: &dyn FileIo, path: &Path) -> Option<OwnedLockArtifacts> {
    let raw_metadata = io.read_file(&path.join("metadata.json")).ok().flatten()?;
    let raw_token = io.read_file(&path.join("token")).ok().flatten()?;
    let raw_proof = io.read_file(&path.join(LOCK_OWNERSHIP_PROOF_NAME)).ok().flatten()?;
    let metadata: ManagerMetadata = serde_json::from_str(&raw_metadata).ok()?;
    let proof: LockOwnershipProof = serde_json::from_str(&raw_proof).ok()?;
    let token = raw_token.trim().to_string();
    if is_valid_manager_metadata(&metadata) && !token.is_empty() && is_valid_ownership_proof(&proof) {
        Some(OwnedLockArtifacts { metadata, token, proof })
    } else {
        None
    }
}

async fn manager_answers_healthcheck(metadata: &ManagerMetadata, token: &str, client: &reqwest::Client) -> bool {
    let url = format!("http://127.0.0.1:{}/healthz", metadata.port);
    let attempt = async {
        let response = client.get(&url).bearer_auth(token).timeout(Duration::from_millis(LIVE_MANAGER_HEALTHCHECK_TIMEOUT_MS)).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: Value = response.json().await.ok()?;
        let instance_matches = body.get("instanceId").and_then(Value::as_str) == Some(metadata.instance_id.as_str());
        let protocol_matches = body.get("protocolVersion").and_then(Value::as_u64) == Some(metadata.protocol_version as u64);
        Some(instance_matches && protocol_matches)
    };
    attempt.await.unwrap_or(false)
}

/// Liveness is the one question this protocol must never answer with a false negative — see this
/// module's doc comment. The PID check in `claim_lock` is the primary guard; this retry keeps a
/// single slow/blocked response from being read as "no manager".
async fn is_live_manager(metadata: &ManagerMetadata, token: &str, client: &reqwest::Client) -> bool {
    if token.is_empty() || metadata.port == 0 {
        return false;
    }
    for attempt in 0..LIVE_MANAGER_HEALTHCHECK_ATTEMPTS {
        if manager_answers_healthcheck(metadata, token, client).await {
            return true;
        }
        if attempt + 1 < LIVE_MANAGER_HEALTHCHECK_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(LIVE_MANAGER_HEALTHCHECK_RETRY_DELAY_MS)).await;
        }
    }
    false
}

fn release_marker(instance_id: &str, token: &str) -> Value {
    serde_json::json!({ "version": 1, "action": "release-lock", "instanceId": instance_id, "tokenDigest": sha256_hex(token.as_bytes()) })
}

fn is_release_marker(raw: Option<&str>, instance_id: &str, token: &str) -> bool {
    let Some(raw) = raw else { return false };
    let Ok(value) = serde_json::from_str::<Value>(raw) else { return false };
    let Some(obj) = value.as_object() else { return false };
    obj.get("version").and_then(Value::as_u64) == Some(1)
        && obj.get("action").and_then(Value::as_str) == Some("release-lock")
        && obj.get("instanceId").and_then(Value::as_str) == Some(instance_id)
        && obj.get("tokenDigest").and_then(Value::as_str) == Some(sha256_hex(token.as_bytes()).as_str())
}

#[derive(Debug, Clone)]
pub struct LockHandle {
    pub path: PathBuf,
    pub metadata_path: PathBuf,
    pub token_path: PathBuf,
    pub proof_path: PathBuf,
    pub ownership_key_path: PathBuf,
    pub instance_id: String,
}

pub async fn prepare_owned_lock_release(io: &dyn FileIo, lock: &LockHandle, token: &str) -> bool {
    let Some(artifacts) = read_owned_lock_artifacts(io, &lock.path) else { return false };
    if artifacts.metadata.instance_id != lock.instance_id || artifacts.token != token {
        return false;
    }
    let marker = release_marker(&lock.instance_id, token);
    io.write_file(&lock.path.join(LOCK_RELEASE_MARKER_NAME), &marker.to_string()).is_ok()
}

async fn quarantine_stale_lock(io: &dyn FileIo, path: &Path, runtime_directory: &Path) -> Result<(), String> {
    let artifacts = read_owned_lock_artifacts(io, path);
    let key = read_lock_ownership_key(io, runtime_directory);
    let Some(artifacts) = artifacts else { return Err("Refusing unsafe or unowned manager lock".to_string()) };
    if !verify_lock_ownership_proof(key.as_deref(), &artifacts.metadata, &artifacts.token, &artifacts.proof) {
        return Err("Refusing unsafe or unowned manager lock".to_string());
    }
    let quarantined = path.with_file_name(format!("{}.stale-{}-{}", path.file_name().unwrap().to_string_lossy(), now_millis(), Uuid::new_v4()));
    std::fs::rename(path, &quarantined).map_err(|e| e.to_string())?;
    let marker = StaleLockMarker { version: 1, action: StaleLockAction::StaleLock, original: artifacts.metadata, proof: artifacts.proof };
    io.write_file(&quarantined.join(STALE_LOCK_MARKER_NAME), &serde_json::to_string(&marker).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
}

pub async fn release_owned_lock(io: &dyn FileIo, lock: &LockHandle, token: &str) {
    let Some(artifacts) = read_owned_lock_artifacts(io, &lock.path) else { return };
    let marker = io.read_file(&lock.path.join(LOCK_RELEASE_MARKER_NAME)).ok().flatten();
    if artifacts.metadata.instance_id != lock.instance_id || artifacts.token != token || !is_release_marker(marker.as_deref(), &lock.instance_id, token) {
        return;
    }
    let _ = crate::file_io::remove_directory(&lock.path);
}

#[derive(Debug, thiserror::Error)]
pub enum ClaimLockError {
    #[error("Hearth manager is already running on port {port}")]
    AlreadyRunning { metadata: Box<ManagerMetadata>, port: u16 },
    #[error("{0}")]
    Other(String),
}

/// The lock-claim protocol. Never steals a lock just because a health check timed out or errored —
/// only a confirmed-dead PID (or an explicit release marker) makes a lock stale.
pub async fn claim_lock(io: &dyn FileIo, runtime_directory: &Path, bootstrap_metadata: &ManagerMetadata, token: &str, client: &reqwest::Client) -> Result<LockHandle, ClaimLockError> {
    let path = lock_dir(runtime_directory);
    let managed_metadata_path = metadata_path(runtime_directory);
    let managed_token_path = token_path(runtime_directory);
    let managed_proof_path = proof_path(runtime_directory);
    let managed_ownership_key_path = ownership_key_path(runtime_directory);
    io.ensure_directory(runtime_directory).map_err(|e| ClaimLockError::Other(e.to_string()))?;

    loop {
        let bootstrap_json = serde_json::to_string(bootstrap_metadata).map_err(|e| ClaimLockError::Other(e.to_string()))?;
        if io.create_exclusive(&managed_metadata_path, &bootstrap_json).map_err(|e| ClaimLockError::Other(e.to_string()))? {
            let key = ownership_key(io, runtime_directory).await.map_err(ClaimLockError::Other)?;
            io.write_file(&managed_token_path, token).map_err(|e| ClaimLockError::Other(e.to_string()))?;
            let proof = create_lock_ownership_proof(&key, bootstrap_metadata, token);
            io.write_file(&managed_proof_path, &serde_json::to_string(&proof).map_err(|e| ClaimLockError::Other(e.to_string()))?).map_err(|e| ClaimLockError::Other(e.to_string()))?;
            return Ok(LockHandle {
                path,
                metadata_path: managed_metadata_path,
                token_path: managed_token_path,
                proof_path: managed_proof_path,
                ownership_key_path: managed_ownership_key_path,
                instance_id: bootstrap_metadata.instance_id.clone(),
            });
        }

        let artifacts = read_owned_lock_artifacts(io, &path);
        let age_ms = io.age_ms(&path) as u64;
        let Some(artifacts) = artifacts else {
            if io.is_private_directory(&path) && age_ms < MANAGER_STARTUP_GRACE_MS {
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
            return Err(ClaimLockError::Other("Refusing unsafe or malformed manager lock".to_string()));
        };

        if artifacts.metadata.port == 0 {
            // An UNPARSEABLE `startedAt` must not count as "started just now". Defaulting it to the
            // current time made `started_age_ms` zero, so the grace check was always true and this
            // loop spun every 25ms forever with no way out — `hearthd daemon`/`manager ensure` hanging
            // instead of erroring. Treat it as outside the grace window and let the liveness check
            // below decide, which is what `Date.parse` -> NaN makes the TS source do.
            let within_startup_grace = crate::supervisor::types::parse_iso8601_millis(&artifacts.metadata.started_at)
                .is_some_and(|started| ((now_millis() - started).max(0) as u64) < MANAGER_STARTUP_GRACE_MS);
            if within_startup_grace || is_pid_alive(artifacts.metadata.pid) {
                tokio::time::sleep(Duration::from_millis(25)).await;
                continue;
            }
        }
        if is_live_manager(&artifacts.metadata, &artifacts.token, client).await {
            let port = artifacts.metadata.port;
            return Err(ClaimLockError::AlreadyRunning { metadata: Box::new(artifacts.metadata), port });
        }
        if is_pid_alive(artifacts.metadata.pid) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        let release_marker_raw = io.read_file(&path.join(LOCK_RELEASE_MARKER_NAME)).ok().flatten();
        if is_release_marker(release_marker_raw.as_deref(), &artifacts.metadata.instance_id, &artifacts.token) && age_ms < MANAGER_STARTUP_GRACE_MS {
            tokio::time::sleep(Duration::from_millis(25)).await;
            continue;
        }
        quarantine_stale_lock(io, &path, runtime_directory).await.map_err(ClaimLockError::Other)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_io::create_file_io;

    fn metadata(instance_id: &str, pid: i64, port: u16) -> ManagerMetadata {
        ManagerMetadata { version: 1, protocol_version: 1, instance_id: instance_id.to_string(), pid, port, started_at: "2024-01-01T00:00:00.000Z".to_string() }
    }

    #[test]
    fn create_and_verify_ownership_proof_round_trips() {
        let metadata = metadata("instance-1", 100, 8080);
        let proof = create_lock_ownership_proof("secret-key", &metadata, "token-123");
        assert!(verify_lock_ownership_proof(Some("secret-key"), &metadata, "token-123", &proof));
    }

    #[test]
    fn verify_fails_with_the_wrong_key() {
        let metadata = metadata("instance-1", 100, 8080);
        let proof = create_lock_ownership_proof("secret-key", &metadata, "token-123");
        assert!(!verify_lock_ownership_proof(Some("wrong-key"), &metadata, "token-123", &proof));
    }

    #[test]
    fn verify_fails_with_a_tampered_metadata_field() {
        let metadata = metadata("instance-1", 100, 8080);
        let proof = create_lock_ownership_proof("secret-key", &metadata, "token-123");
        let tampered = metadata_with_port(&metadata, 9999);
        assert!(!verify_lock_ownership_proof(Some("secret-key"), &tampered, "token-123", &proof));
    }

    #[test]
    fn verify_fails_with_no_key() {
        let metadata = metadata("instance-1", 100, 8080);
        let proof = create_lock_ownership_proof("secret-key", &metadata, "token-123");
        assert!(!verify_lock_ownership_proof(None, &metadata, "token-123", &proof));
    }

    fn metadata_with_port(m: &ManagerMetadata, port: u16) -> ManagerMetadata {
        let mut m = m.clone();
        m.port = port;
        m
    }

    #[test]
    fn random_token_is_reasonably_unique_and_url_safe() {
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[tokio::test]
    async fn ownership_key_is_generated_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let io = create_file_io(false);
        let first = ownership_key(io.as_ref(), dir.path()).await.unwrap();
        let second = ownership_key(io.as_ref(), dir.path()).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(read_lock_ownership_key(io.as_ref(), dir.path()).unwrap(), first);
    }

    #[tokio::test]
    async fn claim_lock_succeeds_on_a_fresh_runtime_directory() {
        let dir = tempfile::tempdir().unwrap();
        let io = create_file_io(false);
        let client = reqwest::Client::new();
        let bootstrap = metadata("instance-1", std::process::id() as i64, 0);
        let lock = claim_lock(io.as_ref(), dir.path(), &bootstrap, "token-1", &client).await.unwrap();
        assert_eq!(lock.instance_id, "instance-1");
        assert!(lock.metadata_path.exists());
    }

    #[tokio::test]
    async fn claim_lock_refuses_a_live_manager_dead_pid_but_healthcheck_reachable() {
        // A real HTTP server pretending to be a live manager on a real port, with a real (fake but
        // alive-looking) pid that IS this test process's own pid so is_pid_alive is true — claim_lock
        // must refuse (AlreadyRunning) since the healthcheck also answers correctly.
        let dir = tempfile::tempdir().unwrap();
        let io = create_file_io(false);
        let client = reqwest::Client::new();

        // Bring up a tiny axum server that answers /healthz like a real daemon would.
        let existing_instance_id = "existing-instance".to_string();
        let app = axum::Router::new().route(
            "/healthz",
            axum::routing::get(move || {
                let instance_id = existing_instance_id.clone();
                async move { axum::Json(serde_json::json!({"status":"ok","protocolVersion":1,"instanceId":instance_id})) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let existing_metadata = metadata("existing-instance", std::process::id() as i64, port);
        let key = ownership_key(io.as_ref(), dir.path()).await.unwrap();
        let proof = create_lock_ownership_proof(&key, &existing_metadata, "existing-token");
        let lock_dir_path = lock_dir(dir.path());
        io.ensure_directory(&lock_dir_path).unwrap();
        io.write_file(&lock_dir_path.join("metadata.json"), &serde_json::to_string(&existing_metadata).unwrap()).unwrap();
        io.write_file(&lock_dir_path.join("token"), "existing-token").unwrap();
        io.write_file(&lock_dir_path.join(LOCK_OWNERSHIP_PROOF_NAME), &serde_json::to_string(&proof).unwrap()).unwrap();

        let bootstrap = metadata("new-instance", std::process::id() as i64, 0);
        let result = claim_lock(io.as_ref(), dir.path(), &bootstrap, "new-token", &client).await;
        match result {
            Err(ClaimLockError::AlreadyRunning { metadata, .. }) => assert_eq!(metadata.instance_id, "existing-instance"),
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn claim_lock_quarantines_a_lock_with_a_confirmed_dead_pid_and_unreachable_healthcheck() {
        let dir = tempfile::tempdir().unwrap();
        let io = create_file_io(false);
        let client = reqwest::Client::new();

        // An implausible pid stands in for "confirmed dead" (is_pid_alive false), and port 0 with an
        // old startedAt means it won't be read as "still booting" either — but claim_lock must still
        // separately confirm via is_live_manager before quarantining; give it a port nothing listens
        // on so the healthcheck fails fast.
        let dead_metadata = metadata("dead-instance", i32::MAX as i64, 59999);
        let key = ownership_key(io.as_ref(), dir.path()).await.unwrap();
        let proof = create_lock_ownership_proof(&key, &dead_metadata, "dead-token");
        let lock_dir_path = lock_dir(dir.path());
        io.ensure_directory(&lock_dir_path).unwrap();
        io.write_file(&lock_dir_path.join("metadata.json"), &serde_json::to_string(&dead_metadata).unwrap()).unwrap();
        io.write_file(&lock_dir_path.join("token"), "dead-token").unwrap();
        io.write_file(&lock_dir_path.join(LOCK_OWNERSHIP_PROOF_NAME), &serde_json::to_string(&proof).unwrap()).unwrap();

        let bootstrap = metadata("new-instance", std::process::id() as i64, 0);
        let lock = claim_lock(io.as_ref(), dir.path(), &bootstrap, "new-token", &client).await.unwrap();
        assert_eq!(lock.instance_id, "new-instance");
        // The old lock directory should have been quarantined (renamed aside), not left in place
        // under the winner.
        let winner_metadata: ManagerMetadata = serde_json::from_str(&io.read_file(&lock.metadata_path).unwrap().unwrap()).unwrap();
        assert_eq!(winner_metadata.instance_id, "new-instance");
    }
}
