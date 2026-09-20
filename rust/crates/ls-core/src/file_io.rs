//! Port of `src/core/file-io.ts` — two interchangeable strategies for every lock/state/log file
//! operation the manager performs, selected by `ServiceCatalog.private_file_guard` (default on):
//!
//!  - `guarded`: O_NOFOLLOW + dev/ino identity-tracking, defending against a symlink swapped in
//!    between a check and a use on a multi-user machine.
//!  - `plain`: straightforward filesystem calls — safe to use on a single-user dev machine that
//!    doesn't need symlink-attack defense.
//!
//! Both implementations still write atomically (temp file + rename) and quarantine (rename aside,
//! never delete) anything that fails validation, so state is never silently lost either way.
//!
//! This module is Unix-only (darwin/linux), matching the rest of the package (see `platform.rs`).

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, thiserror::Error)]
pub enum FileIoError {
    #[error("Unsafe private file: {0}")]
    Unsafe(String),
    #[error("Refusing unsafe private directory: {0}")]
    UnsafeDirectory(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, FileIoError>;

fn unsafe_file(path: &Path) -> FileIoError {
    FileIoError::Unsafe(path.display().to_string())
}

pub trait FileIo: Send + Sync {
    fn guarded(&self) -> bool;
    fn ensure_directory(&self, path: &Path) -> Result<()>;
    fn is_private_directory(&self, path: &Path) -> bool;
    fn read_file(&self, path: &Path) -> Result<Option<String>>;
    /// Atomic write via temp file + rename.
    fn write_file(&self, path: &Path, content: &str) -> Result<()>;
    /// Create-if-absent (O_CREAT|O_EXCL semantics); returns true if this call created it.
    fn create_exclusive(&self, path: &Path, content: &str) -> Result<bool>;
    fn remove_file(&self, path: &Path) -> Result<()>;
    fn quarantine(&self, path: &Path, suffix: &str) -> Result<()>;
    fn age_ms(&self, path: &Path) -> u128;
}

fn quarantine_suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{suffix}-{now_ms}-{}", uuid::Uuid::new_v4()));
    PathBuf::from(name)
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".tmp-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
    PathBuf::from(name)
}

fn age_ms_of(path: &Path) -> u128 {
    fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// -------------------------------------------------------------------------------------------
// Plain strategy
// -------------------------------------------------------------------------------------------

pub struct PlainFileIo;

fn plain_ensure_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

impl FileIo for PlainFileIo {
    fn guarded(&self) -> bool {
        false
    }

    fn ensure_directory(&self, path: &Path) -> Result<()> {
        plain_ensure_directory(path)
    }

    fn is_private_directory(&self, path: &Path) -> bool {
        fs::symlink_metadata(path).map(|m| m.is_dir()).unwrap_or(false)
    }

    fn read_file(&self, path: &Path) -> Result<Option<String>> {
        match fs::read_to_string(path) {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn write_file(&self, path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            plain_ensure_directory(parent)?;
        }
        let temp = temp_path(path);
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(content.as_bytes())?;
        }
        fs::rename(&temp, path)?;
        Ok(())
    }

    fn create_exclusive(&self, path: &Path, content: &str) -> Result<bool> {
        if let Some(parent) = path.parent() {
            plain_ensure_directory(parent)?;
        }
        use std::io::Write;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(content.as_bytes())?;
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn quarantine(&self, path: &Path, suffix: &str) -> Result<()> {
        let _ = fs::rename(path, quarantine_suffix_path(path, suffix));
        Ok(())
    }

    fn age_ms(&self, path: &Path) -> u128 {
        age_ms_of(path)
    }
}

// -------------------------------------------------------------------------------------------
// Guarded strategy — O_NOFOLLOW + dev/ino identity tracking
// -------------------------------------------------------------------------------------------

pub struct GuardedFileIo;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
}

fn is_private_mode(mode: u32) -> bool {
    (mode & 0o077) == 0
}

fn is_private_regular(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && !metadata.file_type().is_symlink() && is_private_mode(metadata.mode()) && metadata.nlink() == 1
}

fn identity(path: &Path) -> Result<Option<Identity>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !is_private_regular(&metadata) {
                return Err(unsafe_file(path));
            }
            Ok(Some(Identity { dev: metadata.dev(), ino: metadata.ino() }))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn validate_handle(path: &Path, file: &fs::File, expected: Option<Identity>) -> Result<Identity> {
    let metadata = file.metadata()?;
    let found = Identity { dev: metadata.dev(), ino: metadata.ino() };
    let ok = is_private_regular(&metadata) && expected.map(|e| e == found).unwrap_or(true);
    if !ok {
        return Err(unsafe_file(path));
    }
    Ok(found)
}

fn open_existing(path: &Path, write: bool) -> Result<fs::File> {
    let before = identity(path)?.ok_or_else(|| unsafe_file(path))?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(write).custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path)?;
    validate_handle(path, &file, Some(before))?;
    Ok(file)
}

fn create_exclusive_handle(path: &Path) -> Result<Option<fs::File>> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => {
            validate_handle(path, &file, None)?;
            Ok(Some(file))
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn open_regular(path: &Path, write: bool, create: bool) -> Result<fs::File> {
    if identity(path)?.is_some() {
        return open_existing(path, write);
    }
    if !create {
        return Err(unsafe_file(path));
    }
    match create_exclusive_handle(path)? {
        Some(file) => Ok(file),
        None => open_existing(path, write),
    }
}

fn is_private_directory_guarded(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.is_dir() && !metadata.file_type().is_symlink() && is_private_mode(metadata.mode()),
        Err(_) => false,
    }
}

fn guarded_ensure_directory(path: &Path) -> Result<()> {
    if is_private_directory_guarded(path) {
        return Ok(());
    }
    match fs::create_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    if !is_private_directory_guarded(path) {
        return Err(FileIoError::UnsafeDirectory(path.display().to_string()));
    }
    Ok(())
}

impl FileIo for GuardedFileIo {
    fn guarded(&self) -> bool {
        true
    }

    fn ensure_directory(&self, path: &Path) -> Result<()> {
        guarded_ensure_directory(path)
    }

    fn is_private_directory(&self, path: &Path) -> bool {
        is_private_directory_guarded(path)
    }

    fn read_file(&self, path: &Path) -> Result<Option<String>> {
        if identity(path)?.is_none() {
            return Ok(None);
        }
        use std::io::Read;
        let mut file = open_regular(path, false, false)?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        Ok(Some(content))
    }

    fn write_file(&self, path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            guarded_ensure_directory(parent)?;
        }
        let temp = temp_path(path);
        {
            use std::io::Write;
            let mut file = open_regular(&temp, true, true)?;
            file.write_all(content.as_bytes())?;
        }
        let result = fs::rename(&temp, path).map_err(FileIoError::from).and_then(|()| {
            if identity(path)?.is_none() {
                Err(unsafe_file(path))
            } else {
                Ok(())
            }
        });
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn create_exclusive(&self, path: &Path, content: &str) -> Result<bool> {
        if let Some(parent) = path.parent() {
            guarded_ensure_directory(parent)?;
        }
        match create_exclusive_handle(path)? {
            Some(mut file) => {
                use std::io::Write;
                file.write_all(content.as_bytes())?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        if identity(path)?.is_none() {
            return Ok(());
        }
        fs::remove_file(path)?;
        Ok(())
    }

    fn quarantine(&self, path: &Path, suffix: &str) -> Result<()> {
        if identity(path)?.is_none() {
            return Ok(());
        }
        fs::rename(path, quarantine_suffix_path(path, suffix))?;
        Ok(())
    }

    fn age_ms(&self, path: &Path) -> u128 {
        age_ms_of(path)
    }
}

pub fn create_file_io(guarded: bool) -> Box<dyn FileIo> {
    if guarded {
        Box::new(GuardedFileIo)
    } else {
        Box::new(PlainFileIo)
    }
}

pub fn remove_directory(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(io: &dyn FileIo) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/file.json");

        assert_eq!(io.read_file(&path).unwrap(), None);

        io.write_file(&path, "hello").unwrap();
        assert_eq!(io.read_file(&path).unwrap(), Some("hello".to_string()));

        assert!(!io.create_exclusive(&path, "again").unwrap());
        io.remove_file(&path).unwrap();
        assert!(io.create_exclusive(&path, "created").unwrap());
        assert_eq!(io.read_file(&path).unwrap(), Some("created".to_string()));

        io.quarantine(&path, "stale").unwrap();
        assert_eq!(io.read_file(&path).unwrap(), None);
    }

    #[test]
    fn plain_file_io_roundtrip() {
        roundtrip(&PlainFileIo);
    }

    #[test]
    fn guarded_file_io_roundtrip() {
        roundtrip(&GuardedFileIo);
    }

    #[test]
    fn guarded_file_io_rejects_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        fs::write(&target, "secret").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let io = GuardedFileIo;
        assert!(io.read_file(&link).is_err());
    }

    #[test]
    fn ensure_directory_creates_private_mode() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("private");
        GuardedFileIo.ensure_directory(&sub).unwrap();
        let mode = fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
