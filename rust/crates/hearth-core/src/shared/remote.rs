//! The remote service registry (`catalog.json`) — types, fetch, and on-disk cache.
//!
//! Trust model (see `docs/shared-services.md`): the document is fetched over HTTPS from a URL
//! pinned in the binary. Tarball sha256s live inside it, so the file's own integrity is exactly
//! TLS's. A cached copy under `~/.hearth/shared/catalog.json` keeps installs working offline after
//! the first successful fetch.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::sync::Mutex;

use crate::catalog::{CommandSpec, ReadinessSpec};

use super::{SharedError, SHARED_CATALOG_URL};

const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const CACHE_FILE_NAME: &str = "catalog.json";
/// The repo-root `catalog.json` baked into the binary — the same document the pinned URL
/// serves. Last resort when the registry is unreachable and nothing is cached yet: the URL
/// 404s for a private repo (raw.githubusercontent.com needs auth), so without this the
/// catalog endpoint can never answer on a first run.
const EMBEDDED_CATALOG: &str = include_str!("../../../../../catalog.json");

/// Where the darwin-arm64 tarball comes from. Exactly one of `url` or `script`.
///
/// `url` is a tarball that already exists on the internet (or a `file://` fixture). `script` is a
/// path relative to this catalog file — or an absolute `http(s)`/`file` URL — run as
/// `bash <script> <scriptArgs...> <out.tar.gz>`. The script does the packaging; the daemon only
/// invokes it and checks `sha256`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedArtifact {
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// Arguments before the output path. `["redis"]` makes the call `bash pack.sh redis <out>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub script_args: Vec<String>,
}

impl SharedArtifact {
    pub fn validate(&self, what: &str) -> Result<(), SharedError> {
        let url = self.url.as_deref().filter(|s| !s.is_empty());
        let script = self.script.as_deref().filter(|s| !s.is_empty());
        match (url, script) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            (Some(_), Some(_)) => Err(SharedError(format!(
                "{what}: artifact must set url or script, not both"
            ))),
            (None, None) => Err(SharedError(format!(
                "{what}: artifact needs a url or a script"
            ))),
        }
    }
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

/// A recipe's readiness before the allocated port exists.
///
/// `kind: http` with no `url` becomes `GET http://127.0.0.1:{port}<path>` (`path` defaults to
/// `/health`). `kind: tcp` with no `port` stays [`RecipeReadiness::PrimaryTcp`] until
/// [`RecipeReadiness::resolve`] fills in the instance port. Every other shape is a normal
/// [`ReadinessSpec`]; `url` still carries `{port}` for [`crate::shared::render::render_readiness`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeReadiness {
    Spec(ReadinessSpec),
    /// `{"kind":"tcp"}` — the instance's allocated primary port.
    PrimaryTcp,
}

impl RecipeReadiness {
    pub fn resolve(&self, primary_port: u16) -> ReadinessSpec {
        match self {
            Self::PrimaryTcp => ReadinessSpec::Tcp { port: primary_port },
            Self::Spec(spec) => spec.clone(),
        }
    }
}

impl Serialize for RecipeReadiness {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Spec(spec) => spec.serialize(serializer),
            Self::PrimaryTcp => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("kind", "tcp")?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for RecipeReadiness {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        recipe_readiness_from_value(value).map_err(D::Error::custom)
    }
}

fn valid_health_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains(char::is_whitespace)
        && !path.contains("://")
        && !path.contains('{')
        && !path.contains('}')
}

fn recipe_readiness_from_value(value: serde_json::Value) -> Result<RecipeReadiness, String> {
    let kind = value
        .get("kind")
        .and_then(|kind| kind.as_str())
        .unwrap_or("")
        .to_string();
    match kind.as_str() {
        "tcp" if value.get("port").is_none() => Ok(RecipeReadiness::PrimaryTcp),
        "http" => {
            let has_path = value.get("path").is_some();
            let has_port = value.get("port").is_some();
            let has_url = value.get("url").is_some();
            if has_url && (has_path || has_port) {
                return Err(
                    "readiness http must not set url together with path or port".to_string()
                );
            }
            if has_port {
                return Err(
                    "readiness http on a shared recipe uses path or url, not a fixed port"
                        .to_string(),
                );
            }
            if has_url {
                let empty = value
                    .get("url")
                    .and_then(|url| url.as_str())
                    .filter(|url| !url.is_empty())
                    .is_none();
                if empty {
                    return Err("readiness url must be a non-empty string".to_string());
                }
                let spec = serde_json::from_value(value).map_err(|error| error.to_string())?;
                return Ok(RecipeReadiness::Spec(spec));
            }
            let path = match value.get("path") {
                None => "/health".to_string(),
                Some(serde_json::Value::String(path)) if valid_health_path(path) => path.clone(),
                Some(serde_json::Value::String(_)) => {
                    return Err("readiness path must be an absolute path".to_string())
                }
                Some(_) => return Err("readiness path must be a string".to_string()),
            };
            Ok(RecipeReadiness::Spec(ReadinessSpec::Http {
                url: format!("http://127.0.0.1:{{port}}{path}"),
            }))
        }
        _ => {
            let spec = serde_json::from_value(value).map_err(|error| error.to_string())?;
            Ok(RecipeReadiness::Spec(spec))
        }
    }
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
    pub readiness: RecipeReadiness,
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
    /// Idempotent setup run before every start (mapped onto the supervisor's
    /// `preparation_command`). Exit 0 when already initialized — this is not a one-shot hook.
    /// The supervised `run` argv stays the real server binary so its `ps` line matches the
    /// stored fingerprint; do the init here instead of in a wrapper that `exec`s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepare: Option<CommandSpec>,
    /// Ports reserved contiguously after `{port}` (`{port2}`, `{port3}`, …). `0` keeps the
    /// historical single-port allocation. Capped by `ports::MAX_SHARED_PORTS`.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub additional_ports: u16,
    /// Explicitly pinned ports — bypasses hash allocation entirely (`ports[0]` is `{port}`, the
    /// rest are `{port2}`…). For recipes whose port IS the feature (e.g. the shared nginx on
    /// 80/443). Mutually exclusive with `additionalPorts`; capped by `ports::MAX_SHARED_PORTS`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
    /// Display labels for `additional_ports`, in order (`"console"`, `"controller"`, …).
    /// Missing entries fall back to `port2`, `port3`, …
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_port_labels: Vec<String>,
}

impl SharedRecipe {
    /// Effective extra-port count driving `{portN}` availability — pinned ports count like
    /// `additionalPorts` does.
    pub fn extra_port_count(&self) -> usize {
        if !self.ports.is_empty() {
            self.ports.len() - 1
        } else {
            self.additional_ports as usize
        }
    }
}

fn is_zero_u16(value: &u16) -> bool {
    *value == 0
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
    let doc: SharedCatalogDocument = serde_json::from_str(text)
        .map_err(|e| SharedError(format!("shared catalog is not valid JSON: {e}")))?;
    if doc.version != 1 {
        return Err(SharedError(format!(
            "shared catalog version must be 1, got {}",
            doc.version
        )));
    }
    for (name, family) in &doc.services {
        for (version, recipe) in &family.versions {
            for (platform, artifact) in &recipe.artifacts {
                artifact.validate(&format!("{name}@{version} {platform}"))?;
            }
            if !recipe.ports.is_empty() && recipe.additional_ports > 0 {
                return Err(SharedError(format!(
                    "{name}@{version} declares both ports and additionalPorts"
                )));
            }
            if recipe.ports.len() > super::ports::MAX_SHARED_PORTS as usize {
                return Err(SharedError(format!(
                    "{name}@{version} pins {} ports; the maximum is {}",
                    recipe.ports.len(),
                    super::ports::MAX_SHARED_PORTS
                )));
            }
        }
    }
    Ok(doc)
}

/// Adds every `name@version` from `other` that `doc` doesn't already have.
fn merge_missing_recipes(doc: &mut SharedCatalogDocument, other: SharedCatalogDocument) {
    for (name, family) in other.services {
        let target = doc
            .services
            .entry(name)
            .or_insert_with(|| SharedServiceFamily {
                versions: HashMap::new(),
            });
        for (version, recipe) in family.versions {
            target.versions.entry(version).or_insert(recipe);
        }
    }
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
                if let Ok(doc) = parse_document(EMBEDDED_CATALOG) {
                    let doc = Arc::new(doc);
                    // Memoized like any other answer — otherwise every `load(false)` on a machine
                    // with no cache re-attempts the (failing) fetch.
                    *self.memo.lock().await = Some(doc.clone());
                    return Ok(doc);
                }
                Err(remote_error)
            }
        }
    }

    /// The disk cache, topped up with any recipe the embedded catalog has that the cache lacks.
    /// A cache written once (env / `catalog-url` / `file://` use) would otherwise hide every recipe
    /// added in a newer `hearth` build forever, because the private-repo fetch always fails.
    /// A recipe present in both keeps the cached (actually fetched) version.
    fn load_cached(&self) -> Option<Arc<SharedCatalogDocument>> {
        let text = std::fs::read_to_string(&self.cache_path).ok()?;
        let mut doc = parse_document(&text).ok()?;
        if let Ok(embedded) = parse_document(EMBEDDED_CATALOG) {
            merge_missing_recipes(&mut doc, embedded);
        }
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
            let text = std::fs::read_to_string(rest)
                .map_err(|e| SharedError(format!("failed to read shared catalog {rest}: {e}")))?;
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
            .map_err(|e| {
                SharedError(format!(
                    "failed to fetch shared catalog from {}: {e}",
                    self.url
                ))
            })?;
        if !response.status().is_success() {
            return Err(SharedError(format!(
                "shared catalog fetch failed: HTTP {}",
                response.status()
            )));
        }
        let text = response
            .text()
            .await
            .map_err(|e| SharedError(format!("failed to read shared catalog body: {e}")))?;
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
    fn parses_prepare_and_additional_ports() {
        let doc = parse_document(
            r#"{
              "version": 1,
              "services": {
                "kafka": {
                  "versions": {
                    "4.1.0": {
                      "artifacts": { "darwin-arm64": { "url": "file:///k.tgz", "sha256": "abc" } },
                      "additionalPorts": 1,
                      "extraPortLabels": ["controller"],
                      "prepare": { "argv": ["{installDir}/bin/hearth-prepare", "{dataDir}", "{port}", "{port2}"] },
                      "run": { "shell": "exec {installDir}/jre/bin/java @{dataDir}/jvm.args", "exec": true },
                      "readiness": { "kind": "command", "command": { "argv": ["{installDir}/bin/hearth-ready", "{port}"] } }
                    }
                  }
                }
              }
            }"#,
        )
        .unwrap();
        let recipe = doc.recipe("kafka", "4.1.0").unwrap();
        assert_eq!(recipe.additional_ports, 1);
        assert_eq!(recipe.extra_port_labels, vec!["controller".to_string()]);
        assert!(recipe.prepare.is_some());
        assert!(recipe.artifacts["darwin-arm64"].script.is_none());
    }

    #[test]
    fn parses_pinned_ports() {
        let doc = parse_document(
            r#"{
              "version": 1,
              "services": {
                "nginx": {
                  "versions": {
                    "1.30.5": {
                      "artifacts": { "darwin-arm64": { "url": "file:///n.tgz", "sha256": "abc" } },
                      "ports": [80, 443],
                      "extraPortLabels": ["https"],
                      "run": { "argv": ["{installDir}/bin/nginx", "-c", "{dataDir}/nginx.conf"] },
                      "readiness": { "kind": "http", "url": "http://127.0.0.1:{port}/healthz" }
                    }
                  }
                }
              }
            }"#,
        )
        .unwrap();
        let recipe = doc.recipe("nginx", "1.30.5").unwrap();
        assert_eq!(recipe.ports, vec![80, 443]);
        assert_eq!(recipe.extra_port_count(), 1);
    }

    #[test]
    fn rejects_ports_and_additional_ports_together() {
        let error = parse_document(
            r#"{
              "version": 1,
              "services": {
                "nginx": {
                  "versions": {
                    "1.30.5": {
                      "artifacts": { "darwin-arm64": { "url": "file:///n.tgz", "sha256": "abc" } },
                      "ports": [80],
                      "additionalPorts": 1,
                      "run": { "argv": ["x"] },
                      "readiness": { "kind": "process" }
                    }
                  }
                }
              }
            }"#,
        )
        .unwrap_err();
        assert!(
            error.0.contains("both ports and additionalPorts"),
            "{error:?}"
        );
    }

    #[test]
    fn rejects_an_artifact_that_sets_both_sources() {
        let err = parse_document(
            r#"{
              "version": 1,
              "services": {
                "redis": { "versions": { "1": {
                  "artifacts": { "darwin-arm64": { "url": "https://example.com/a.tgz", "script": "pack.sh", "sha256": "abc" } },
                  "run": { "argv": ["redis-server"] },
                  "readiness": { "kind": "process" }
                } } }
              }
            }"#,
        )
        .unwrap_err();
        assert!(err.0.contains("not both"), "{err}");
    }

    #[test]
    fn rejects_an_artifact_with_no_source() {
        let err = parse_document(
            r#"{
              "version": 1,
              "services": {
                "redis": { "versions": { "1": {
                  "artifacts": { "darwin-arm64": { "sha256": "abc" } },
                  "run": { "argv": ["redis-server"] },
                  "readiness": { "kind": "process" }
                } } }
              }
            }"#,
        )
        .unwrap_err();
        assert!(err.0.contains("needs a url or a script"), "{err}");
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
        let remote = RemoteCatalog::new(
            dir.path(),
            Some("http://127.0.0.1:1/unreachable".to_string()),
        );
        let doc = remote.load(true).await.unwrap();
        assert_eq!(
            doc.recipe("postgres", "16.4").unwrap().artifacts["darwin-arm64"].sha256,
            "abc",
            "a cached recipe wins over nothing"
        );
        // A stale cache doesn't hide recipes this build ships.
        assert!(doc.services.contains_key("redis"));
    }

    #[tokio::test]
    async fn falls_back_to_the_embedded_catalog_without_remote_or_cache() {
        // No cached document: a private repo makes the pinned raw URL answer 404 forever,
        // so the binary's own copy of catalog.json is what answers.
        let dir = tempfile::tempdir().unwrap();
        let remote = RemoteCatalog::new(
            dir.path(),
            Some("http://127.0.0.1:1/unreachable".to_string()),
        );
        let doc = remote.load(true).await.unwrap();
        assert!(doc.services.contains_key("redis"));
    }

    fn brace_names(text: &str) -> Vec<String> {
        crate::catalog::template_vars(text)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn walk_strings(value: &serde_json::Value, path: &str, out: &mut Vec<(String, String)>) {
        match value {
            serde_json::Value::String(text) => out.push((path.to_string(), text.clone())),
            serde_json::Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    walk_strings(item, &format!("{path}[{index}]"), out);
                }
            }
            serde_json::Value::Object(map) => {
                for (key, item) in map {
                    walk_strings(item, &format!("{path}.{key}"), out);
                }
            }
            _ => {}
        }
    }

    fn is_extra_port(name: &str) -> bool {
        name.starts_with("port") && name.len() > 4 && name[4..].chars().all(|c| c.is_ascii_digit())
    }

    #[test]
    fn shipped_catalog_templates_only_use_known_vars() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../catalog.json");
        let text = std::fs::read_to_string(&path).expect("catalog.json");
        let doc = parse_document(&text).expect("catalog.json parses");
        for name in ["redis", "mongodb", "minio", "nginx", "kafka"] {
            assert!(doc.services.contains_key(name), "{name} missing");
        }
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut strings = Vec::new();
        walk_strings(&value["services"], "services", &mut strings);
        let instance = [
            "installDir",
            "dataDir",
            "port",
            "name",
            "version",
            "instanceId",
        ];
        let project = [
            "projectId",
            "projectDb",
            "projectUser",
            "projectBucket",
            "projectRoot",
        ];
        for (path, text) in &strings {
            let in_attachment = path.contains(".provision")
                || path.contains(".deprovision")
                || path.contains(".connection");
            for name in brace_names(text) {
                let known = instance.contains(&name.as_str())
                    || is_extra_port(&name)
                    || (in_attachment && project.contains(&name.as_str()))
                    || (path.contains(".connection") && name == "url");
                assert!(known, "{path} uses unknown template {{{name}}}");
            }
        }
        for (service, family) in &doc.services {
            for (version, recipe) in &family.versions {
                let blob = serde_json::to_string(recipe).unwrap();
                if brace_names(&blob).iter().any(|name| name == "port2") {
                    assert!(
                        recipe.extra_port_count() >= 1,
                        "{service}@{version} uses {{port2}} without a second port (ports/additionalPorts)"
                    );
                }
                assert!(
                    !(recipe.additional_ports > 0 && !recipe.ports.is_empty()),
                    "{service}@{version} declares both ports and additionalPorts"
                );
                let sha = &recipe.artifacts["darwin-arm64"].sha256;
                assert!(
                    sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit()),
                    "{service}@{version} sha256"
                );
            }
        }
        assert!(doc.services["minio"]
            .versions
            .values()
            .all(|recipe| recipe.additional_ports == 1));
        assert!(doc.services["kafka"]
            .versions
            .values()
            .all(|recipe| recipe.additional_ports == 1));
        assert!(doc.services["redis"]
            .versions
            .values()
            .all(|recipe| recipe.additional_ports == 0));
        assert!(doc.services["nginx"]
            .versions
            .values()
            .all(|recipe| recipe.prepare.is_some()));
        let mongo = doc.recipe("mongodb", "8.0.32").unwrap();
        // mongodb is a `script` artifact too: the recipe repacks mongod+mongosh so provision can
        // initiate the single-node rs0 (consumers use transactions).
        assert!(mongo.artifacts["darwin-arm64"].script.is_some());
        assert!(!mongo.provision.is_empty());
        match &mongo.run {
            crate::catalog::CommandSpec::Argv { argv } => {
                assert!(argv.windows(2).any(|w| w == ["--replSet", "rs0"]))
            }
            _ => panic!("mongodb run must be argv"),
        }
        assert!(doc.services["redis"]
            .versions
            .values()
            .all(|recipe| recipe.artifacts["darwin-arm64"].script.is_some()));
        assert!(matches!(
            doc.recipe("mongodb", "8.0.32").unwrap().readiness,
            RecipeReadiness::PrimaryTcp
        ));
        match &doc.recipe("nginx", "1.30.5").unwrap().readiness {
            RecipeReadiness::Spec(ReadinessSpec::Http { url }) => {
                assert_eq!(url, "http://127.0.0.1:{port}/health")
            }
            other => panic!("nginx readiness {other:?}"),
        }
        match &doc.services["minio"]
            .versions
            .values()
            .next()
            .unwrap()
            .readiness
        {
            RecipeReadiness::Spec(ReadinessSpec::Http { url }) => {
                assert_eq!(url, "http://127.0.0.1:{port}/minio/health/live")
            }
            other => panic!("minio readiness {other:?}"),
        }
        for name in ["redis", "kafka"] {
            let readiness = &doc.services[name]
                .versions
                .values()
                .next()
                .unwrap()
                .readiness;
            let RecipeReadiness::Spec(ReadinessSpec::Command {
                command: CommandSpec::Argv { argv },
                ..
            }) = readiness
            else {
                panic!("{name} readiness {readiness:?}")
            };
            assert_eq!(argv.len(), 2, "{name}");
            assert!(argv[0].ends_with("/hearth-ready"), "{name} {argv:?}");
            assert_eq!(argv[1], "{port}");
        }
    }

    #[test]
    fn readiness_shorthand_normalizes_and_legacy_shapes_still_parse() {
        let doc = parse_document(
            r#"{
              "version": 1,
              "services": {
                "mongodb": { "versions": { "8.0.32": {
                  "artifacts": { "darwin-arm64": { "url": "file:///m.tgz", "sha256": "abc" } },
                  "run": { "argv": ["mongod"] },
                  "readiness": { "kind": "tcp" }
                } } },
                "nginx": { "versions": { "1.30.5": {
                  "artifacts": { "darwin-arm64": { "url": "file:///n.tgz", "sha256": "abc" } },
                  "run": { "argv": ["nginx"] },
                  "readiness": { "kind": "http", "path": "/health" }
                } } },
                "legacy": { "versions": { "1": {
                  "artifacts": { "darwin-arm64": { "url": "file:///l.tgz", "sha256": "abc" } },
                  "run": { "argv": ["mongod"] },
                  "readiness": { "kind": "command", "command": { "shell": "perl -e 'exit 0'" } }
                } } }
              }
            }"#,
        )
        .unwrap();
        assert!(matches!(
            doc.recipe("mongodb", "8.0.32").unwrap().readiness,
            RecipeReadiness::PrimaryTcp
        ));
        let encoded =
            serde_json::to_value(&doc.recipe("mongodb", "8.0.32").unwrap().readiness).unwrap();
        assert_eq!(encoded, serde_json::json!({"kind": "tcp"}));
        match &doc.recipe("nginx", "1.30.5").unwrap().readiness {
            RecipeReadiness::Spec(ReadinessSpec::Http { url }) => {
                assert_eq!(url, "http://127.0.0.1:{port}/health")
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            doc.recipe("legacy", "1").unwrap().readiness,
            RecipeReadiness::Spec(ReadinessSpec::Command {
                command: CommandSpec::Shell { .. },
                ..
            })
        ));
        let rejected = parse_document(
            r#"{
              "version": 1,
              "services": { "x": { "versions": { "1": {
                "artifacts": { "darwin-arm64": { "url": "file:///x.tgz", "sha256": "abc" } },
                "run": { "argv": ["x"] },
                "readiness": { "kind": "http", "url": "http://127.0.0.1/health", "path": "/health" }
              } } } }
            }"#,
        );
        let error = rejected.unwrap_err();
        assert!(error.0.contains("url"), "{error}");
    }
}
