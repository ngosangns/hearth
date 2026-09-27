//! Tarball installer — used by shared services (`hearth.yaml shared:` → smp instances under
//! `~/.hearth/shared`) and by per-service `artifact:` blocks (`<runtimeDir>/installs/…`). The flow
//! is identical for both: materialize the archive (`url` download or `script` packager), verify
//! sha256 for `url` artifacts only, extract under `downloads/` (a staging/quarantine area), then
//! atomically rename the payload into the install dir. A `.hearth-installed` marker file is the
//! source of truth for "done" — a partial install is never mistaken for a complete one because the
//! marker is written last.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::Digest;
use tokio::io::AsyncWriteExt;

use super::registry::SharedInstance;
use super::remote::{SharedArtifact, SharedRecipe};
use super::{blocking, hex, output_with_timeout, SharedContext, SharedError, SHARED_ARTIFACT_PLATFORM};

const INSTALLED_MARKER: &str = ".hearth-installed";
const EXTRACT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const PACK_SCRIPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45 * 60);

/// The normalized artifact both callers lower into: the shared catalog's per-platform
/// `SharedArtifact`, and a `hearth.yaml` service's `artifact:` block.
pub struct ArtifactSpec {
    pub url: Option<String>,
    pub script: Option<String>,
    pub script_args: Vec<String>,
    /// sha256 is enforced only for downloaded `url` artifacts — the one place a hash can actually
    /// pin anything. `script` artifacts are built locally from the recipe's own packaging script:
    /// the output is never byte-reproducible (tar mtimes, compile variance), the catalog hash just
    /// records what the producer's build happened to yield, and the script — already executed — is
    /// the real trust boundary.
    pub sha256: Option<String>,
}

impl From<&SharedArtifact> for ArtifactSpec {
    fn from(a: &SharedArtifact) -> Self {
        Self {
            url: a.url.clone().filter(|s| !s.is_empty()),
            script: a.script.clone().filter(|s| !s.is_empty()),
            script_args: a.script_args.clone(),
            sha256: Some(a.sha256.clone()).filter(|s| !s.is_empty()),
        }
    }
}

impl From<&crate::catalog::ServiceArtifact> for ArtifactSpec {
    fn from(a: &crate::catalog::ServiceArtifact) -> Self {
        Self { url: a.url.clone(), script: a.script.clone(), script_args: a.script_args.clone(), sha256: a.sha256.clone() }
    }
}

/// How a relative `script` source resolves — next to a catalog URL for shared recipes, or under
/// the project root for a service `artifact:`.
#[derive(Debug, Clone)]
pub enum ScriptOrigin {
    CatalogUrl(String),
    ProjectDir(PathBuf),
}

/// Everything a tarball install needs besides the artifact itself.
pub struct TarballInstaller {
    pub http: reqwest::Client,
    pub downloads_dir: PathBuf,
    pub script_origin: ScriptOrigin,
}

impl TarballInstaller {
    pub fn shared(ctx: &SharedContext) -> Self {
        Self {
            http: ctx.http.clone(),
            downloads_dir: ctx.downloads_dir(),
            script_origin: ScriptOrigin::CatalogUrl(ctx.remote.url().to_string()),
        }
    }

    /// Project-scoped installs: relative `script` paths resolve under `project_root`, and
    /// `HEARTH_CATALOG_ORIGIN` handed to the script is the root as a `file://` URL.
    pub fn project(project_root: &Path, downloads_dir: PathBuf) -> Self {
        Self { http: reqwest::Client::new(), downloads_dir, script_origin: ScriptOrigin::ProjectDir(project_root.to_path_buf()) }
    }

    /// Idempotent install into `install_dir` — early-returns when the marker is already present.
    pub async fn install(
        &self,
        name: &str,
        install_dir: &Path,
        artifact: &ArtifactSpec,
        on_progress: &impl Fn(&str),
    ) -> Result<PathBuf, SharedError> {
        if install_dir.join(INSTALLED_MARKER).is_file() {
            return Ok(install_dir.to_path_buf());
        }

        let downloads_dir = self.downloads_dir.clone();
        blocking(move || {
            std::fs::create_dir_all(&downloads_dir)
                .map_err(|e| SharedError(format!("cannot create downloads dir: {e}")))
        })
        .await?;
        let archive_path = self.downloads_dir.join(format!("{name}.tar.gz"));
        let digest = self.materialize_archive(name, artifact, &archive_path, on_progress).await?;
        if artifact.url.is_some() {
            // The digest was computed while the bytes were written — no second read of the file.
            let expected = artifact.sha256.as_deref().unwrap_or_default();
            if !digest.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(expected)) {
                let _ = std::fs::remove_file(&archive_path);
                return Err(SharedError(format!(
                    "sha256 mismatch: expected {expected}, got {}",
                    digest.unwrap_or_default()
                )));
            }
        }

        let staging = self.downloads_dir.join(format!("{name}.extract"));
        {
            let staging = staging.clone();
            blocking(move || {
                let _ = std::fs::remove_dir_all(&staging);
                std::fs::create_dir_all(&staging)
                    .map_err(|e| SharedError(format!("cannot create extract dir: {e}")))
            })
            .await?;
        }
        extract(&archive_path, &staging).await?;
        let installed = {
            let install_dir = install_dir.to_path_buf();
            blocking(move || finish_install(&staging, &archive_path, &install_dir)).await?
        };
        on_progress(&format!("installed {name}"));
        Ok(installed)
    }

    /// Download a published tarball, or run the packaging script into `dest`. Returns the
    /// download's sha256 (hex) for a `url` artifact; script output is never hashed.
    async fn materialize_archive(
        &self,
        name: &str,
        artifact: &ArtifactSpec,
        dest: &Path,
        on_progress: &impl Fn(&str),
    ) -> Result<Option<String>, SharedError> {
        if let Some(url) = artifact.url.as_deref() {
            return self.download(url, dest, on_progress).await.map(Some);
        }
        let script = artifact
            .script
            .as_deref()
            .ok_or_else(|| SharedError(format!("{name}: artifact needs a url or a script")))?;
        self.run_pack_script(name, script, &artifact.script_args, dest, on_progress)
            .await
            .map(|()| None)
    }

    async fn run_pack_script(
        &self,
        name: &str,
        script: &str,
        args: &[String],
        dest: &Path,
        on_progress: &impl Fn(&str),
    ) -> Result<(), SharedError> {
        let script_path = self.stage_script(name, script).await?;
        let origin = match &self.script_origin {
            ScriptOrigin::CatalogUrl(url) => catalog_origin(url),
            ScriptOrigin::ProjectDir(root) => format!("file://{}", root.display()),
        };
        on_progress(&format!("packaging {name} with {script}"));
        let mut command = tokio::process::Command::new("bash");
        command.arg(&script_path).args(args).arg(dest);
        command.env("HEARTH_CATALOG_ORIGIN", &origin);
        if let Some(dir) = script_path.parent() {
            command.current_dir(dir);
        }
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = output_with_timeout(command, PACK_SCRIPT_TIMEOUT)
            .await
            .map_err(|e| {
                SharedError(format!(
                    "{name}: failed to run packaging script: {e}"
                ))
            })?
            .ok_or_else(|| SharedError(format!("{name}: packaging script timed out")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(SharedError(format!(
                "{name}: packaging script failed: {stderr}{stdout}"
            )));
        }
        if !dest.is_file() {
            return Err(SharedError(format!(
                "{name}: packaging script did not write {}",
                dest.display()
            )));
        }
        Ok(())
    }

    /// Shared: a `file://` catalog runs the script that sits next to it, an `https://` catalog
    /// downloads the script from the same directory as `catalog.json`. Project: relative paths are
    /// project files; `https://`/`file://` scripts download into the staging area.
    async fn stage_script(&self, name: &str, script: &str) -> Result<PathBuf, SharedError> {
        match &self.script_origin {
            ScriptOrigin::CatalogUrl(catalog_url) => {
                if let Some(path) = local_catalog_script(catalog_url, script) {
                    return Ok(path);
                }
                let url = if script.contains("://") {
                    script.to_string()
                } else {
                    format!("{}/{}", catalog_origin(catalog_url), script)
                };
                let dest = self.downloads_dir.join(format!("{name}.pack.sh"));
                self.download(&url, &dest, &|_| {}).await?;
                Ok(dest)
            }
            ScriptOrigin::ProjectDir(root) => {
                if script.contains("://") {
                    let dest = self.downloads_dir.join(format!("{name}.pack.sh"));
                    self.download(script, &dest, &|_| {}).await?;
                    return Ok(dest);
                }
                let path = root.join(script);
                if path.is_file() {
                    return Ok(path);
                }
                Err(SharedError(format!("{name}: packaging script not found: {}", path.display())))
            }
        }
    }

    /// Streams `url` into `dest`, hashing in the same pass — a 300 MB tarball is never held in
    /// memory or read back. Returns the lowercase-hex sha256 of what was written.
    async fn download(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &impl Fn(&str),
    ) -> Result<String, SharedError> {
        on_progress(&format!("downloading {url}"));
        if let Some(rest) = url.strip_prefix("file://") {
            // file:// artifacts exist for tests and local fixtures — still hashed for the sha check.
            let (src, dest, url) = (PathBuf::from(rest), dest.to_path_buf(), url.to_string());
            return blocking(move || {
                copy_hashing(&src, &dest).map_err(|e| SharedError(format!("failed to copy {url}: {e}")))
            })
            .await;
        }
        let mut response = self
            .http
            .get(url)
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .map_err(|e| SharedError(format!("download failed for {url}: {e}")))?;
        if !response.status().is_success() {
            return Err(SharedError(format!(
                "download failed for {url}: HTTP {}",
                response.status()
            )));
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| SharedError(format!("cannot create {}: {e}", dest.display())))?;
        let mut hasher = sha2::Sha256::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| SharedError(format!("download failed for {url}: {e}")))?
        {
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| SharedError(format!("cannot write {}: {e}", dest.display())))?;
        }
        file.flush()
            .await
            .map_err(|e| SharedError(format!("cannot write {}: {e}", dest.display())))?;
        Ok(hex(&hasher.finalize()))
    }
}

/// `std::fs::copy` that also hashes what it copies.
fn copy_hashing(src: &Path, dest: &Path) -> std::io::Result<String> {
    let mut input = std::fs::File::open(src)?;
    let mut output = std::fs::File::create(dest)?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    Ok(hex(&hasher.finalize()))
}

/// Moves the extracted payload into `install_dir` and writes the marker. Rename into place only
/// once the payload is complete; a leftover target from a previous failed attempt is quarantined
/// aside first, never deleted.
fn finish_install(staging: &Path, archive_path: &Path, install_dir: &Path) -> Result<PathBuf, SharedError> {
    let payload = payload_dir(staging)?;
    if install_dir.exists() {
        let quarantined =
            install_dir.with_extension(format!("quarantine-{}", uuid::Uuid::new_v4()));
        std::fs::rename(install_dir, &quarantined)
            .map_err(|e| SharedError(format!("cannot quarantine previous install: {e}")))?;
    }
    if let Some(parent) = install_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SharedError(e.to_string()))?;
    }
    std::fs::rename(&payload, install_dir)
        .map_err(|e| SharedError(format!("cannot move install into place: {e}")))?;
    std::fs::write(install_dir.join(INSTALLED_MARKER), "ok\n")
        .map_err(|e| SharedError(e.to_string()))?;
    let _ = std::fs::remove_file(archive_path);
    let _ = std::fs::remove_dir_all(staging);
    Ok(install_dir.to_path_buf())
}

fn artifact_for<'a>(
    instance_id: &str,
    recipe: &'a SharedRecipe,
) -> Result<&'a super::remote::SharedArtifact, SharedError> {
    recipe
        .artifacts
        .get(SHARED_ARTIFACT_PLATFORM)
        .ok_or_else(|| {
            SharedError(format!(
                "{instance_id}: no artifact for {SHARED_ARTIFACT_PLATFORM}"
            ))
        })
}

/// `installs/<name>/<version>` is ready — marker present. The marker only exists after a fully
/// extracted payload was renamed into place, so its presence implies the payload too.
pub fn is_installed(ctx: &SharedContext, instance: &SharedInstance) -> bool {
    instance
        .install_dir(&ctx.root)
        .join(INSTALLED_MARKER)
        .is_file()
}

/// Idempotent install. `on_progress` receives human-readable lines the caller can stream into a
/// service log / attach response trace.
pub async fn ensure_installed(
    ctx: &SharedContext,
    instance: &SharedInstance,
    on_progress: impl Fn(&str),
) -> Result<PathBuf, SharedError> {
    let install_dir = instance.install_dir(&ctx.root);
    if is_installed(ctx, instance) {
        return Ok(install_dir);
    }
    let artifact = artifact_for(&instance.id(), &instance.recipe)?;
    TarballInstaller::shared(ctx)
        .install(&instance.id(), &install_dir, &ArtifactSpec::from(artifact), &on_progress)
        .await
}

/// Install for a per-service `artifact:` block: `<installDir>`/`<dataDir>` come from the resolved
/// definition (or fall back to the runtime-dir convention for programmatically built catalogs),
/// and the data dir is created on every start — wiping it manually must not wedge the service.
pub async fn install_service_artifact(
    project_root: &Path,
    runtime_directory: &Path,
    service: &crate::catalog::ServiceDefinition,
    artifact: &crate::catalog::ServiceArtifact,
    on_progress: &impl Fn(&str),
) -> Result<PathBuf, SharedError> {
    let install_dir = artifact
        .install_dir
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::install_dir(runtime_directory, &service.id, &artifact.version));
    let data_dir = artifact
        .data_dir
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::service_data_dir(runtime_directory, &service.id));
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| SharedError(format!("cannot create data dir {}: {e}", data_dir.display())))?;
    TarballInstaller::project(project_root, crate::paths::downloads_dir(runtime_directory))
        .install(&service.id, &install_dir, &ArtifactSpec::from(artifact), on_progress)
        .await
}

fn catalog_origin(catalog_url: &str) -> String {
    match catalog_url.rfind('/') {
        Some(index) => catalog_url[..index].to_string(),
        None => catalog_url.to_string(),
    }
}

fn local_catalog_script(catalog_url: &str, script: &str) -> Option<PathBuf> {
    let rest = catalog_url.strip_prefix("file://")?;
    let catalog = Path::new(rest);
    let path = catalog.parent()?.join(script);
    path.is_file().then_some(path)
}

/// `tar -xzf` via the system tar — present on every supported macOS, no crate needed.
async fn extract(archive: &Path, dest: &Path) -> Result<(), SharedError> {
    let mut command = tokio::process::Command::new("tar");
    command
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let output = output_with_timeout(command, EXTRACT_TIMEOUT)
        .await
        .map_err(|e| SharedError(format!("failed to spawn tar: {e}")))?
        .ok_or_else(|| SharedError("tar extract timed out".to_string()))?;
    if !output.status.success() {
        return Err(SharedError(format!(
            "tar extract failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

/// What inside the staging dir becomes the install dir: the single top-level directory if there is
/// exactly one (the usual tarball shape), otherwise the staging dir itself.
fn payload_dir(staging: &Path) -> Result<PathBuf, SharedError> {
    let entries: Vec<PathBuf> = std::fs::read_dir(staging)
        .map_err(|e| SharedError(e.to_string()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            !p.file_name()
                .map(|n| n.to_string_lossy().starts_with('.'))
                .unwrap_or(false)
        })
        .collect();
    if entries.len() == 1 && entries[0].is_dir() {
        return Ok(entries[0].clone());
    }
    Ok(staging.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CommandSpec, ReadinessSpec};
    use crate::shared::registry::InstallState;
    use std::collections::{BTreeMap, HashMap};

    fn make_tarball(dir: &Path) -> (PathBuf, String) {
        // Build a real tarball: payload dir with one file inside.
        let payload = dir.join("pkg/bin");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(payload.join("hello"), "world").unwrap();
        let archive = dir.join("pkg.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(dir)
            .arg("pkg")
            .status()
            .unwrap();
        assert!(status.success());
        let sha = hex(&sha2::Sha256::digest(std::fs::read(&archive).unwrap()));
        (archive, sha)
    }

    fn ctx(root: &Path) -> Arc<SharedContext> {
        SharedContext::open(root.to_path_buf(), Some("file:///nonexistent".to_string())).unwrap()
    }

    fn instance_with_url(url: String, sha256: String) -> SharedInstance {
        SharedInstance {
            name: "redis".to_string(),
            version: "7.2".to_string(),
            port: 43100,
            extra_ports: vec![],
            install_state: InstallState::Pending,
            install_error: None,
            recipe: SharedRecipe {
                artifacts: HashMap::from([(
                    "darwin-arm64".to_string(),
                    crate::shared::remote::SharedArtifact {
                        url: Some(url),
                        sha256,
                        script: None,
                        script_args: vec![],
                    },
                )]),
                run: CommandSpec::Argv { argv: vec![] },
                stop: None,
                readiness: ReadinessSpec::Process,
                provision: vec![],
                deprovision: vec![],
                connection: None,
                env: None,
                prepare: None,
                additional_ports: 0,
                ports: Vec::new(),
                extra_port_labels: Vec::new(),
            },
            attachments: BTreeMap::new(),
        }
    }

    use std::sync::Arc;

    #[tokio::test]
    async fn installs_from_a_file_url_and_marks_done() {
        let src = tempfile::tempdir().unwrap();
        let (archive, sha) = make_tarball(src.path());
        let root = tempfile::tempdir().unwrap();
        let ctx = ctx(root.path());
        let instance = instance_with_url(format!("file://{}", archive.display()), sha);

        let dir = ensure_installed(&ctx, &instance, |_| {}).await.unwrap();
        assert!(
            dir.join("bin/hello").exists(),
            "payload top-level dir should be unwrapped"
        );
        assert!(is_installed(&ctx, &instance));
        // Idempotent second call.
        ensure_installed(&ctx, &instance, |_| {}).await.unwrap();
    }

    #[tokio::test]
    async fn installs_from_a_packaging_script() {
        let src = tempfile::tempdir().unwrap();
        let (archive, sha) = make_tarball(src.path());
        let catalog = tempfile::tempdir().unwrap();
        let script = catalog.path().join("pack.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\nset -euo pipefail\ncp '{}' \"$1\"\n",
                archive.display()
            ),
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let ctx = SharedContext::open(
            root.path().to_path_buf(),
            Some(format!("file://{}/catalog.json", catalog.path().display())),
        )
        .unwrap();
        let mut instance = instance_with_url("unused".to_string(), sha);
        instance
            .recipe
            .artifacts
            .get_mut("darwin-arm64")
            .unwrap()
            .url = None;
        instance
            .recipe
            .artifacts
            .get_mut("darwin-arm64")
            .unwrap()
            .script = Some("pack.sh".to_string());

        let dir = ensure_installed(&ctx, &instance, |_| {}).await.unwrap();
        assert!(dir.join("bin/hello").exists());
        assert!(is_installed(&ctx, &instance));
    }

    #[tokio::test]
    async fn rejects_a_sha256_mismatch() {
        let src = tempfile::tempdir().unwrap();
        let (archive, _) = make_tarball(src.path());
        let root = tempfile::tempdir().unwrap();
        let ctx = ctx(root.path());
        let instance = instance_with_url(format!("file://{}", archive.display()), "0".repeat(64));
        let err = ensure_installed(&ctx, &instance, |_| {}).await.unwrap_err();
        assert!(err.0.contains("sha256 mismatch"), "{err}");
        assert!(!is_installed(&ctx, &instance));
    }

    #[tokio::test]
    async fn installs_a_project_service_artifact_with_project_relative_script() {
        let src = tempfile::tempdir().unwrap();
        let (archive, _sha) = make_tarball(src.path());
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("pack.sh"),
            format!("#!/bin/bash\nset -euo pipefail\ncp '{}' \"$1\"\n", archive.display()),
        )
        .unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let service = crate::catalog::ServiceDefinition {
            id: "db".to_string(),
            label: None,
            kind: None,
            ownership: None,
            disabled: false,
            profiles: crate::catalog::ServiceProfiles {
                run: crate::catalog::ServiceRunProfile::Unresolved { readiness: ReadinessSpec::Process, readiness_timeout_ms: None, preparation: None, preparation_command: None },
                build: None,
            },
            ports: None,
            urls: None,
            artifact: None,
        };
        let artifact = crate::catalog::ServiceArtifact {
            version: "1.0".to_string(),
            url: None,
            script: Some("pack.sh".to_string()),
            script_args: vec![],
            sha256: None,
            install_dir: None,
            data_dir: None,
        };
        let install_dir = install_service_artifact(project.path(), runtime.path(), &service, &artifact, &|_| {}).await.unwrap();
        assert_eq!(install_dir, crate::paths::install_dir(runtime.path(), "db", "1.0"));
        assert!(install_dir.join("bin/hello").exists());
        assert!(crate::paths::service_data_dir(runtime.path(), "db").is_dir());
        // Idempotent.
        install_service_artifact(project.path(), runtime.path(), &service, &artifact, &|_| {}).await.unwrap();
    }
}
