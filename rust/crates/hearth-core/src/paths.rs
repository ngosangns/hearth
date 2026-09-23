//! Port of `src/core/paths.ts` — path-joining helpers for the runtime directory layout.
use std::path::{Path, PathBuf};

pub use crate::state::{
    LOCK_OWNERSHIP_PROOF_NAME, LOCK_RELEASE_MARKER_NAME, OWNERSHIP_KEY_NAME, STALE_LOCK_MARKER_NAME,
};

/// Default runtime directory, relative to a catalog's root, when `ServiceCatalog.runtime_directory`
/// is not set.
pub const DEFAULT_RUNTIME_DIRECTORY_NAME: &str = ".hearth/runtime-v1";

pub fn resolve_runtime_directory(root: &Path, catalog_runtime_directory: Option<&str>) -> PathBuf {
    root.join(catalog_runtime_directory.unwrap_or(DEFAULT_RUNTIME_DIRECTORY_NAME))
}

pub fn lock_dir(runtime_directory: &Path) -> PathBuf {
    runtime_directory.join("manager.lock")
}

pub fn token_path(runtime_directory: &Path) -> PathBuf {
    lock_dir(runtime_directory).join("token")
}

pub fn metadata_path(runtime_directory: &Path) -> PathBuf {
    lock_dir(runtime_directory).join("metadata.json")
}

pub fn proof_path(runtime_directory: &Path) -> PathBuf {
    lock_dir(runtime_directory).join(LOCK_OWNERSHIP_PROOF_NAME)
}

pub fn ownership_key_path(runtime_directory: &Path) -> PathBuf {
    runtime_directory.join(OWNERSHIP_KEY_NAME)
}

pub fn state_path(runtime_directory: &Path) -> PathBuf {
    runtime_directory.join("state.json")
}

pub fn logs_dir(runtime_directory: &Path) -> PathBuf {
    runtime_directory.join("logs")
}

pub fn log_path(runtime_directory: &Path, service_id: &str) -> PathBuf {
    logs_dir(runtime_directory).join(format!("{service_id}.log"))
}

pub fn raw_log_path(runtime_directory: &Path, service_id: &str) -> PathBuf {
    logs_dir(runtime_directory).join(format!("{service_id}.raw"))
}

pub fn log_stream_state_path(runtime_directory: &Path) -> PathBuf {
    logs_dir(runtime_directory).join("streams.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_default_runtime_directory() {
        let root = Path::new("/proj");
        assert_eq!(
            resolve_runtime_directory(root, None),
            PathBuf::from("/proj/.hearth/runtime-v1")
        );
    }

    #[test]
    fn resolves_catalog_overridden_runtime_directory() {
        let root = Path::new("/proj");
        assert_eq!(
            resolve_runtime_directory(root, Some(".ls/runtime")),
            PathBuf::from("/proj/.ls/runtime")
        );
    }

    #[test]
    fn path_layout_matches_ts() {
        let runtime = PathBuf::from("/proj/.hearth/runtime-v1");
        assert_eq!(lock_dir(&runtime), runtime.join("manager.lock"));
        assert_eq!(token_path(&runtime), runtime.join("manager.lock/token"));
        assert_eq!(metadata_path(&runtime), runtime.join("manager.lock/metadata.json"));
        assert_eq!(state_path(&runtime), runtime.join("state.json"));
        assert_eq!(logs_dir(&runtime), runtime.join("logs"));
        assert_eq!(log_path(&runtime, "api"), runtime.join("logs/api.log"));
        assert_eq!(raw_log_path(&runtime, "api"), runtime.join("logs/api.raw"));
        assert_eq!(log_stream_state_path(&runtime), runtime.join("logs/streams.json"));
    }
}
