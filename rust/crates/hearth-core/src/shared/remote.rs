//! The remote service registry (`catalog.json`) — types, fetch, and on-disk cache.
//!
//! Trust model (see `docs/shared-services.md`): the document is fetched over HTTPS from a URL
//! pinned in the binary. Tarball sha256s live inside it, so the file's own integrity is exactly
//! TLS's. A cached copy under `~/.hearth/shared/catalog.json` keeps installs working offline after
//! the first successful fetch.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::catalog::{CommandSpec, ReadinessSpec};

use super::{SharedError, SHARED_CATALOG_URL};

const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const CACHE_FILE_NAME: &str = "catalog.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedArtifact {
    pub url: String,
    pub sha256: String,
}

/// How a connection to a provisioned instance is described back to an attaching project. `url` and
/// every `env` value are templates rendered per-attachment (`{port}`, `{projectDb}`, …; `{url}`
/// inside `env` refers to the rendered `url`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedConnection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

/// Everything needed to install+run+provision one `name@version`, snapshotted into `registry.json`
/// at registration time — an installed instance keeps working even if the version is later removed
/// from the remote registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedRecipe {
    /// Platform key → artifact. v1 only ships `darwin-arm64` (`SHARED_ARTIFACT_PLATFORM`).
    pub artifacts: HashMap<String, SharedArtifact>,
    /// Long-running command template — `{installDir}`, `{dataDir}`, `{port}`.
    pub run: CommandSpec,
    /// Graceful stop template, optional (SIGTERM to the process tree is the default path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<CommandSpec>,
    pub readiness: ReadinessSpec,
    /// Per-project provisioning, run once per attaching project — `{projectId}`, `{projectDb}`,
    /// `{projectUser}` templates. Commands must be idempotent (re-attach reuses the cached result).
    #[serde(default)]
    pub provision: Vec<CommandSpec>,
    #[serde(default)]
    pub deprovision: Vec<CommandSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<SharedConnection>,
    /// Extra environment for the run command, rendered with the same templates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedServiceFamily {
    pub versions: HashMap<String, SharedRecipe>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedCatalogDocument {
    pub version: u32,
    pub services: HashMap<String, SharedServiceFamily>,
}

impl SharedCatalogDocument {
    pub fn recipe(&self, name: &str, version: &str) -> Option<&SharedRecipe> {
        self.services.get(name)?.versions.get(version)
    }
}

fn parse_document(text: &str) -> Result<SharedCatalogDocument, SharedError> {
    let doc: SharedCatalogDocument = serde_json::from_str(text).map_err(|e| SharedError(format!("shared catalog is not valid JSON: {e}")))?;
    if doc.version != 1 {
        return Err(SharedError(format!("shared catalog version must be 1, got {}", doc.version)));
    }
    Ok(doc)
}

/// Fetches `catalog.json` with a write-through file cache. The in-process memo is deliberate: the
/// remote registry should change rarely, and every `attach` would otherwise pay a fetch.
pub struct RemoteCatalog {
    url: String,
    cache_path: PathBuf,
    http: reqwest::Client,
    memo: Mutex<Option<Arc<SharedCatalogDocument>>>,
}

impl RemoteCatalog {
    pub fn new(root: &Path, url: Option<String>) -> Self {
        Self {
            url: url.unwrap_or_else(|| SHARED_CATALOG_URL.to_string()),
            cache_path: root.join(CACHE_FILE_NAME),
            http: reqwest::Client::new(),
            memo: Mutex::new(None),
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The last successfully fetched/parsed document: memo, then disk cache, then remote.
    /// `refresh: true` forces a network attempt first, falling back to the cache on failure.
    pub async fn load(&self, refresh: bool) -> Result<Arc<SharedCatalogDocument>, SharedError> {
        if !refresh {
            if let Some(doc) = self.memo.lock().await.clone() {
                return Ok(doc);
            }
            if let Some(doc) = self.load_cached() {
                return Ok(doc);
            }
        }
        match self.fetch().await {
            Ok(doc) => {
                *self.memo.lock().await = Some(doc.clone());
                Ok(doc)
            }
            Err(remote_error) => {
                if let Some(doc) = self.load_cached() {
                    return Ok(doc);
                }
                Err(remote_error)
            }
        }
    }

    fn load_cached(&self) -> Option<Arc<SharedCatalogDocument>> {
        let text = std::fs::read_to_string(&self.cache_path).ok()?;
        let doc = parse_document(&text).ok()?;
        let doc = Arc::new(doc);
        // Populate the memo without blocking — callers here are sync on the fallback path.
        if let Ok(mut memo) = self.memo.try_lock() {
            *memo = Some(doc.clone());
        }
        Some(doc)
    }

    async fn fetch(&self) -> Result<Arc<SharedCatalogDocument>, SharedError> {
        // file:// URLs exist for tests/local development of the whole attach path without
        // publishing a real registry — the cache write-through still applies.
        if let Some(rest) = self.url.strip_prefix("file://") {
            let text = std::fs::read_to_string(rest).map_err(|e| SharedError(format!("failed to read shared catalog {rest}: {e}")))?;
            let doc = Arc::new(parse_document(&text)?);
            let _ = std::fs::write(&self.cache_path, &text);
            return Ok(doc);
        }
        let response = self
            .http
            .get(&self.url)
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| SharedError(format!("failed to fetch shared catalog from {}: {e}", self.url)))?;
        if !response.status().is_success() {
            return Err(SharedError(format!("shared catalog fetch failed: HTTP {}", response.status())));
        }
        let text = response.text().await.map_err(|e| SharedError(format!("failed to read shared catalog body: {e}")))?;
        let doc = Arc::new(parse_document(&text)?);
        // Write-through cache; a cache write failure must not fail the fetch itself.
        if let Some(parent) = self.cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&self.cache_path, &text);
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_doc() -> &'static str {
        r#"{
          "version": 1,
          "services": {
            "postgres": {
              "versions": {
                "16.4": {
                  "artifacts": { "darwin-arm64": { "url": "file:///tmp/pg.tgz", "sha256": "abc" } },
                  "run": { "argv": ["{installDir}/bin/postgres", "-D", "{dataDir}", "-p", "{port}"] },
                  "readiness": { "kind": "tcp", "port": 5432 },
                  "provision": [ { "argv": ["{installDir}/bin/psql", "-c", "CREATE DATABASE {projectDb}"] } ],
                  "connection": { "url": "postgres://{projectUser}@127.0.0.1:{port}/{projectDb}", "env": { "DATABASE_URL": "{url}" } }
                }
              }
            }
          }
        }"#
    }

    #[test]
    fn parses_a_catalog_document() {
        let doc = parse_document(sample_doc()).unwrap();
        let recipe = doc.recipe("postgres", "16.4").unwrap();
        assert_eq!(recipe.artifacts["darwin-arm64"].sha256, "abc");
        assert_eq!(recipe.provision.len(), 1);
        assert!(doc.recipe("postgres", "15").is_none());
        assert!(doc.recipe("redis", "7").is_none());
    }

    #[test]
    fn rejects_wrong_version() {
        let err = parse_document(r#"{"version": 2, "services": {}}"#).unwrap_err();
        assert!(err.0.contains("version must be 1"), "{err}");
    }

    #[tokio::test]
    async fn falls_back_to_cache_when_remote_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CACHE_FILE_NAME), sample_doc()).unwrap();
        let remote = RemoteCatalog::new(dir.path(), Some("http://127.0.0.1:1/unreachable".to_string()));
        let doc = remote.load(true).await.unwrap();
        assert!(doc.recipe("postgres", "16.4").is_some());
    }
}
