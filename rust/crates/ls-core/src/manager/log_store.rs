//! Port of `CursorLogStore` (`src/core/manager.ts`) — per-service append-only log files with
//! crash-safe rotation and a cursor/generation tailing protocol for `GET /v1/logs/:id`.
//!
//! **Sharp edges (AGENTS.md)**:
//! 1. Log rotation is crash-safe via a two-phase journal (`pending` → renamed → `committed` →
//!    generation-counter updated → journal deleted), reconciled on next load by checking whether the
//!    rotated file already exists even if the journal never reached `committed`.
//! 2. Cursor/generation log-tailing must never split a UTF-8 sequence across a read boundary, and
//!    must signal `reset: true` on a stale generation or an invalid/out-of-range cursor.
use std::collections::HashMap;
use std::hash::Hash;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::ServiceId;
use crate::file_io::FileIo;
use crate::state::LogSlice;

pub const DEFAULT_LOG_TAIL_BYTES: u64 = 16 * 1024;
pub const DEFAULT_LOG_MAX_BYTES: u64 = 256 * 1024;
pub const DEFAULT_LOG_ROTATION_COUNT: usize = 2;
const LOG_STREAM_STATE_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum LogStoreError {
    #[error("Log append exceeds {0} byte limit")]
    TooLarge(u64),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    FileIo(#[from] crate::file_io::FileIoError),
}

#[derive(Debug, Serialize, Deserialize)]
struct LogStreamState {
    version: u32,
    generations: HashMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogRotationJournal {
    version: u32,
    service_id: String,
    from_generation: u64,
    to_generation: u64,
    phase: String,
}

struct KeyedLock<K: Eq + Hash + Clone> {
    locks: Mutex<HashMap<K, Arc<tokio::sync::Mutex<()>>>>,
}
impl<K: Eq + Hash + Clone> KeyedLock<K> {
    fn new() -> Self {
        Self { locks: Mutex::new(HashMap::new()) }
    }
    fn get(&self, key: &K) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().unwrap().entry(key.clone()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }
}

pub struct CursorLogStore {
    io: Arc<dyn FileIo>,
    directory: PathBuf,
    latest_tail_bytes: u64,
    max_bytes: u64,
    rotation_count: usize,
    stream_state_path: PathBuf,
    rotation_generations: Mutex<HashMap<ServiceId, u64>>,
    append_queues: KeyedLock<ServiceId>,
    metadata_serial: tokio::sync::Mutex<()>,
    loaded: tokio::sync::OnceCell<()>,
}

impl CursorLogStore {
    pub fn new(io: Arc<dyn FileIo>, directory: PathBuf, latest_tail_bytes: Option<u64>, max_bytes: Option<u64>, rotation_count: Option<usize>) -> Self {
        let stream_state_path = directory.join("streams.json");
        Self {
            io,
            directory,
            latest_tail_bytes: latest_tail_bytes.unwrap_or(DEFAULT_LOG_TAIL_BYTES),
            max_bytes: max_bytes.unwrap_or(DEFAULT_LOG_MAX_BYTES),
            rotation_count: rotation_count.unwrap_or(DEFAULT_LOG_ROTATION_COUNT),
            stream_state_path,
            rotation_generations: Mutex::new(HashMap::new()),
            append_queues: KeyedLock::new(),
            metadata_serial: tokio::sync::Mutex::new(()),
            loaded: tokio::sync::OnceCell::new(),
        }
    }

    pub async fn append(&self, service_id: &ServiceId, data: &str) -> Result<(), LogStoreError> {
        let lock = self.append_queues.get(service_id);
        let _guard = lock.lock().await;
        self.ensure_loaded().await;
        let encoded = data.as_bytes();
        if encoded.len() as u64 > self.max_bytes {
            return Err(LogStoreError::TooLarge(self.max_bytes));
        }
        self.io.ensure_directory(&self.directory)?;
        let path = self.path_for(service_id);
        let current_size = self.size(&path);
        if current_size > 0 && current_size + encoded.len() as u64 > self.max_bytes {
            self.rotate(service_id).await?;
        }
        // Appends bypass FileIo (which always rewrites atomically) — an append-in-place is safe here
        // because only this store's own serialized per-service queue ever writes this path.
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        file.write_all(encoded)?;
        Ok(())
    }

    pub async fn read(&self, service_id: &ServiceId, cursor: Option<u64>, limit: Option<u64>, lifecycle_generation: u64, requested_generation: Option<u64>) -> LogSlice {
        let lock = self.append_queues.get(service_id);
        let _guard = lock.lock().await;
        self.ensure_loaded().await;
        let path = self.path_for(service_id);
        let size = self.size(&path);
        let safe_limit = limit.unwrap_or(self.latest_tail_bytes).clamp(1, self.latest_tail_bytes);
        let stale_generation = requested_generation.map(|g| g != lifecycle_generation).unwrap_or(false);
        let invalid_cursor = cursor.map(|c| c > size || !self.is_utf8_boundary(&path, c, size)).unwrap_or(false);
        let reset = stale_generation || invalid_cursor;
        let start = match cursor {
            Some(c) if !reset => c,
            _ => self.tail_start(&path, size, safe_limit),
        };
        let (data, bytes_read) = if start >= size { (String::new(), 0) } else { self.read_framed(&path, start, size, safe_limit) };
        LogSlice {
            service_id: service_id.clone(),
            generation: lifecycle_generation,
            cursor: start,
            next_cursor: start + bytes_read,
            data,
            reset,
            truncated: start > 0,
        }
    }

    async fn ensure_loaded(&self) {
        self.loaded.get_or_init(|| self.load()).await;
    }

    async fn load(&self) {
        match self.io.read_file(&self.stream_state_path) {
            Ok(Some(raw)) => match serde_json::from_str::<Value>(&raw) {
                Ok(value) => {
                    if value.get("version").and_then(Value::as_u64) == Some(LOG_STREAM_STATE_VERSION as u64) {
                        if let Some(generations) = value.get("generations").and_then(Value::as_object) {
                            let mut map = self.rotation_generations.lock().unwrap();
                            for (k, v) in generations {
                                if let Some(n) = v.as_u64() {
                                    map.insert(k.clone(), n);
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    let _ = self.io.quarantine(&self.stream_state_path, "corrupt");
                }
            },
            Ok(None) => {}
            Err(_) => {}
        }
        self.reconcile_rotation_journals().await;
    }

    async fn reconcile_rotation_journals(&self) {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect::<Vec<_>>(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => return,
        };
        let mut changed = false;
        let mut journals_to_delete = Vec::new();
        for entry in entries.iter().filter(|e| e.ends_with(".log.rotation.json")) {
            let path = self.directory.join(entry);
            let journal: Option<LogRotationJournal> = self.io.read_file(&path).ok().flatten().and_then(|raw| serde_json::from_str(&raw).ok());
            match journal {
                Some(journal) if journal.version == LOG_STREAM_STATE_VERSION => {
                    let rotated_path = format!("{}.{}", self.path_for(&journal.service_id).display(), journal.from_generation);
                    let renamed = Path::new(&rotated_path).exists();
                    if journal.phase == "committed" || renamed {
                        let mut map = self.rotation_generations.lock().unwrap();
                        let current = *map.get(&journal.service_id).unwrap_or(&1);
                        if current < journal.to_generation {
                            map.insert(journal.service_id.clone(), journal.to_generation);
                            changed = true;
                        }
                    }
                    journals_to_delete.push(path);
                }
                _ => {
                    let _ = self.io.quarantine(&path, "corrupt");
                }
            }
        }
        if changed {
            self.write_stream_state();
        }
        for path in journals_to_delete {
            let _ = self.io.remove_file(&path);
        }
    }

    fn write_stream_state(&self) {
        let generations = self.rotation_generations.lock().unwrap().clone();
        if let Ok(json) = serde_json::to_string(&LogStreamState { version: LOG_STREAM_STATE_VERSION, generations }) {
            let _ = self.io.write_file(&self.stream_state_path, &json);
        }
    }

    async fn save_generation_after_rotation(&self, service_id: &ServiceId, generation: u64) {
        let _guard = self.metadata_serial.lock().await;
        self.rotation_generations.lock().unwrap().insert(service_id.clone(), generation);
        self.write_stream_state();
    }

    fn path_for(&self, service_id: &ServiceId) -> PathBuf {
        self.directory.join(format!("{service_id}.log"))
    }

    fn size(&self, path: &Path) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    fn read_byte_at(&self, path: &Path, offset: u64) -> Option<u8> {
        let mut file = std::fs::File::open(path).ok()?;
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = [0u8; 1];
        file.read_exact(&mut buf).ok()?;
        Some(buf[0])
    }

    fn is_utf8_boundary(&self, path: &Path, cursor: u64, size: u64) -> bool {
        if cursor == 0 || cursor == size {
            return true;
        }
        match self.read_byte_at(path, cursor) {
            Some(byte) => (byte & 0xc0) != 0x80,
            None => false,
        }
    }

    fn tail_start(&self, path: &Path, size: u64, limit: u64) -> u64 {
        let mut start = size.saturating_sub(limit);
        while start < size && !self.is_utf8_boundary(path, start, size) {
            start += 1;
        }
        start
    }

    /// Reads up to `limit` bytes starting at `start`, never splitting a multi-byte UTF-8 sequence
    /// across the end of the returned slice — port of `readFramed`.
    fn read_framed(&self, path: &Path, start: u64, size: u64, limit: u64) -> (String, u64) {
        let maximum = (size - start).min(limit + 3);
        let mut buffer = vec![0u8; maximum as usize];
        let bytes_read: u64 = (|| -> std::io::Result<usize> {
            let mut file = std::fs::File::open(path)?;
            file.seek(SeekFrom::Start(start))?;
            file.read(&mut buffer)
        })()
        .unwrap_or(0) as u64;
        let mut end = bytes_read.min(limit);
        while end < bytes_read && (buffer[end as usize] & 0xc0) == 0x80 {
            end += 1;
        }
        if end < bytes_read && end > 0 && (buffer[(end - 1) as usize] & 0xe0) == 0xc0 {
            end = bytes_read.min(end + 1);
        }
        if end < bytes_read && end > 0 && (buffer[(end - 1) as usize] & 0xf0) == 0xe0 {
            end = bytes_read.min(end + 2);
        }
        if end < bytes_read && end > 0 && (buffer[(end - 1) as usize] & 0xf8) == 0xf0 {
            end = bytes_read.min(end + 3);
        }
        while end < bytes_read && (buffer[end as usize] & 0xc0) == 0x80 {
            end += 1;
        }
        let data = String::from_utf8_lossy(&buffer[..end as usize]).to_string();
        (data, end)
    }

    async fn rotate(&self, service_id: &ServiceId) -> Result<(), LogStoreError> {
        let path = self.path_for(service_id);
        let from_generation = *self.rotation_generations.lock().unwrap().get(service_id).unwrap_or(&1);
        let journal_path = PathBuf::from(format!("{}.rotation.json", path.display()));
        let journal = LogRotationJournal { version: LOG_STREAM_STATE_VERSION, service_id: service_id.clone(), from_generation, to_generation: from_generation + 1, phase: "pending".to_string() };
        let rotated_path = PathBuf::from(format!("{}.{}", path.display(), from_generation));
        self.io.write_file(&journal_path, &serde_json::to_string(&journal).unwrap())?;
        // A second writer (another daemon sharing this runtime directory) may have rotated the same
        // file first; the log is simply already gone, so appending recreates it. Throwing here would
        // reject the append that triggered the rotation.
        match std::fs::rename(&path, &rotated_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let committed = LogRotationJournal { phase: "committed".to_string(), ..journal.clone() };
        self.io.write_file(&journal_path, &serde_json::to_string(&committed).unwrap())?;

        let prefix = format!("{}.", self.path_for(service_id).file_name().unwrap().to_string_lossy());
        let mut rotated: Vec<String> = std::fs::read_dir(&self.directory)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with(&prefix) && !name.ends_with(".rotation.json"))
            .collect();
        rotated.sort();
        let excess = rotated.len().saturating_sub(self.rotation_count);
        for name in &rotated[..excess] {
            let _ = self.io.remove_file(&self.directory.join(name));
        }
        self.save_generation_after_rotation(service_id, journal.to_generation).await;
        let _ = self.io.remove_file(&journal_path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_io::create_file_io;

    fn store(tail: u64, max: u64, rotation_count: usize) -> (CursorLogStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let io: Arc<dyn FileIo> = Arc::from(create_file_io(false));
        (CursorLogStore::new(io, dir.path().join("logs"), Some(tail), Some(max), Some(rotation_count)), dir)
    }

    #[tokio::test]
    async fn append_then_read_roundtrips() {
        let (store, _dir) = store(1024, 4096, 2);
        store.append(&"api".to_string(), "hello ").await.unwrap();
        store.append(&"api".to_string(), "world").await.unwrap();
        let slice = store.read(&"api".to_string(), None, None, 0, None).await;
        assert_eq!(slice.data, "hello world");
        assert!(!slice.reset);
        assert_eq!(slice.cursor, 0);
        assert_eq!(slice.next_cursor, 11);
    }

    #[tokio::test]
    async fn read_with_a_cursor_returns_only_new_data() {
        let (store, _dir) = store(1024, 4096, 2);
        store.append(&"api".to_string(), "hello ").await.unwrap();
        let first = store.read(&"api".to_string(), None, None, 0, None).await;
        store.append(&"api".to_string(), "world").await.unwrap();
        let second = store.read(&"api".to_string(), Some(first.next_cursor), None, 0, None).await;
        assert_eq!(second.data, "world");
        assert!(!second.reset);
    }

    #[tokio::test]
    async fn stale_generation_forces_a_reset() {
        let (store, _dir) = store(1024, 4096, 2);
        store.append(&"api".to_string(), "hello").await.unwrap();
        let slice = store.read(&"api".to_string(), Some(0), None, 5, Some(3)).await;
        assert!(slice.reset);
        assert_eq!(slice.data, "hello");
    }

    #[tokio::test]
    async fn out_of_range_cursor_forces_a_reset() {
        let (store, _dir) = store(1024, 4096, 2);
        store.append(&"api".to_string(), "hello").await.unwrap();
        let slice = store.read(&"api".to_string(), Some(9999), None, 0, None).await;
        assert!(slice.reset);
    }

    #[tokio::test]
    async fn never_a_new_service_echoes_generation_zero() {
        let (store, _dir) = store(1024, 4096, 2);
        let slice = store.read(&"never-started".to_string(), None, None, 0, None).await;
        assert_eq!(slice.generation, 0);
        assert_eq!(slice.data, "");
        assert!(!slice.reset);
    }

    #[tokio::test]
    async fn append_over_the_max_size_is_rejected() {
        let (store, _dir) = store(1024, 10, 2);
        let err = store.append(&"api".to_string(), "this is definitely longer than ten bytes").await.unwrap_err();
        assert!(matches!(err, LogStoreError::TooLarge(_)));
    }

    #[tokio::test]
    async fn rotation_triggers_when_appending_would_exceed_max_bytes() {
        let (store, dir) = store(1024, 20, 2);
        store.append(&"api".to_string(), "0123456789").await.unwrap(); // 10 bytes, under max
        store.append(&"api".to_string(), "0123456789").await.unwrap(); // would be 20, still fits exactly? 10+10=20 not > 20, no rotate yet
        store.append(&"api".to_string(), "x").await.unwrap(); // 21 > 20 now -> rotates first
        let logs_dir = dir.path().join("logs");
        let rotated = logs_dir.join("api.log.1");
        assert!(rotated.exists(), "expected a rotated generation-1 file to exist");
        let current = std::fs::read_to_string(logs_dir.join("api.log")).unwrap();
        assert_eq!(current, "x");
    }

    #[tokio::test]
    async fn rotation_keeps_only_the_configured_number_of_old_generations() {
        let (store, dir) = store(1024, 5, 1); // rotation_count = 1: keep only the newest old generation
        for chunk in ["aaaaa", "bbbbb", "ccccc", "ddddd"] {
            store.append(&"api".to_string(), chunk).await.unwrap();
        }
        let logs_dir = dir.path().join("logs");
        let entries: Vec<String> = std::fs::read_dir(&logs_dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
        let rotated_generations: Vec<&String> = entries.iter().filter(|e| e.starts_with("api.log.") && !e.ends_with(".rotation.json")).collect();
        assert!(rotated_generations.len() <= 1, "{rotated_generations:?}");
    }

    /// `lifecycle_generation`/`requested_generation` (the last two `read()` params) are the
    /// *service's* restart generation, unrelated to the log store's own internal rotation-generation
    /// counter under test here — both left at their "not stale" defaults (`None`) so the only thing
    /// under test is rotation-journal reconciliation, observed via its actual side effects: the
    /// journal file is cleaned up and `streams.json` reflects the recovered generation.
    #[tokio::test]
    async fn a_committed_rotation_journal_is_reconciled_on_next_load() {
        let (store, dir) = store(1024, 4096, 2);
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::write(logs_dir.join("api.log"), "current generation 2 content").unwrap();
        std::fs::write(logs_dir.join("api.log.1"), "old generation 1 content").unwrap();
        let journal = LogRotationJournal { version: 1, service_id: "api".to_string(), from_generation: 1, to_generation: 2, phase: "committed".to_string() };
        std::fs::write(logs_dir.join("api.log.rotation.json"), serde_json::to_string(&journal).unwrap()).unwrap();

        let slice = store.read(&"api".to_string(), None, None, 0, None).await;
        assert!(!slice.reset);
        assert_eq!(slice.data, "current generation 2 content");
        assert!(!logs_dir.join("api.log.rotation.json").exists(), "the journal should be cleaned up after reconciliation");
        let streams: Value = serde_json::from_str(&std::fs::read_to_string(logs_dir.join("streams.json")).unwrap()).unwrap();
        assert_eq!(streams["generations"]["api"], 2);
    }

    #[tokio::test]
    async fn a_pending_rotation_journal_whose_rename_already_happened_is_still_reconciled() {
        // Simulates a crash between the rename succeeding and the journal being marked "committed" —
        // reconciliation must trust the renamed file's existence, not just the journal's own phase.
        let (store, dir) = store(1024, 4096, 2);
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::write(logs_dir.join("api.log"), "current").unwrap();
        std::fs::write(logs_dir.join("api.log.1"), "old").unwrap();
        let journal = LogRotationJournal { version: 1, service_id: "api".to_string(), from_generation: 1, to_generation: 2, phase: "pending".to_string() };
        std::fs::write(logs_dir.join("api.log.rotation.json"), serde_json::to_string(&journal).unwrap()).unwrap();

        let _ = store.read(&"api".to_string(), None, None, 0, None).await;
        let streams: Value = serde_json::from_str(&std::fs::read_to_string(logs_dir.join("streams.json")).unwrap()).unwrap();
        assert_eq!(streams["generations"]["api"], 2, "the renamed file's existence alone should be enough to trust generation 2 even though the journal never reached committed");
        assert!(!logs_dir.join("api.log.rotation.json").exists());
    }

    #[tokio::test]
    async fn multi_byte_utf8_at_the_tail_boundary_is_never_split() {
        let (store, _dir) = store(5, 4096, 2); // a tiny tail window forces the boundary math to kick in
        // "héllo" — the 'é' is a 2-byte UTF-8 sequence; a naive byte-offset tail of the last 5 bytes
        // would land mid-character.
        store.append(&"api".to_string(), "héllo").await.unwrap();
        let slice = store.read(&"api".to_string(), None, None, 0, None).await;
        assert!(String::from_utf8(slice.data.clone().into_bytes()).is_ok());
        assert!(!slice.data.contains('\u{FFFD}'), "must not contain a UTF-8 replacement character: {:?}", slice.data);
    }

    #[tokio::test]
    async fn corrupt_streams_json_is_quarantined_without_losing_the_log_itself() {
        let (store, dir) = store(1024, 4096, 2);
        let logs_dir = dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::write(logs_dir.join("streams.json"), "not json {{{").unwrap();
        std::fs::write(logs_dir.join("api.log"), "hello").unwrap();
        let slice = store.read(&"api".to_string(), None, None, 0, None).await;
        assert_eq!(slice.data, "hello");
        assert!(!logs_dir.join("streams.json").exists());
    }
}
