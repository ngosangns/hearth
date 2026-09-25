//! Tarball installer for shared services. Downloads a recipe's artifact, verifies its sha256,
//! extracts under `downloads/` (a staging/quarantine area), then atomically renames the payload
//! into `installs/<name>/<version>`. A `.hearth-installed` marker file is the source of truth for
//! "done" — a partial install is never mistaken for a complete one because the marker is written
//! last.
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::Digest;

use super::registry::SharedInstance;
use super::remote::SharedRecipe;
use super::{SharedContext, SharedError, SHARED_ARTIFACT_PLATFORM};

const INSTALLED_MARKER: &str = ".hearth-installed";
const EXTRACT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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

    std::fs::create_dir_all(ctx.downloads_dir())
        .map_err(|e| SharedError(format!("cannot create downloads dir: {e}")))?;
    let archive_path = ctx
        .downloads_dir()
        .join(format!("{}.tar.gz", instance.id()));
    materialize_archive(ctx, &instance.id(), artifact, &archive_path, &on_progress).await?;
    verify_sha256(&archive_path, &artifact.sha256)?;

    let staging = ctx
        .downloads_dir()
        .join(format!("{}.extract", instance.id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|e| SharedError(format!("cannot create extract dir: {e}")))?;
    extract(&archive_path, &staging).await?;
    let payload = payload_dir(&staging)?;

    // Rename into place only once the payload is complete; a leftover target from a previous
    // failed attempt is quarantined aside first, never deleted.
    if install_dir.exists() {
        let quarantined =
            install_dir.with_extension(format!("quarantine-{}", uuid::Uuid::new_v4()));
        std::fs::rename(&install_dir, &quarantined)
            .map_err(|e| SharedError(format!("cannot quarantine previous install: {e}")))?;
    }
    if let Some(parent) = install_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| SharedError(e.to_string()))?;
    }
    std::fs::rename(&payload, &install_dir)
        .map_err(|e| SharedError(format!("cannot move install into place: {e}")))?;
    std::fs::write(install_dir.join(INSTALLED_MARKER), "ok\n")
        .map_err(|e| SharedError(e.to_string()))?;
    let _ = std::fs::remove_file(&archive_path);
    let _ = std::fs::remove_dir_all(&staging);
    on_progress(&format!("installed {}", instance.id()));
    Ok(install_dir)
}

/// Download a published tarball, or run the catalog's packaging script into `dest`.
async fn materialize_archive(
    ctx: &SharedContext,
    instance_id: &str,
    artifact: &super::remote::SharedArtifact,
    dest: &Path,
    on_progress: &impl Fn(&str),
) -> Result<(), SharedError> {
    if let Some(url) = artifact.url.as_deref().filter(|url| !url.is_empty()) {
        return download(ctx, url, dest, on_progress).await;
    }
    let script = artifact
        .script
        .as_deref()
        .filter(|script| !script.is_empty())
        .ok_or_else(|| SharedError(format!("{instance_id}: artifact needs a url or a script")))?;
    run_pack_script(
        ctx,
        instance_id,
        script,
        &artifact.script_args,
        dest,
        on_progress,
    )
    .await
}

async fn run_pack_script(
    ctx: &SharedContext,
    instance_id: &str,
    script: &str,
    args: &[String],
    dest: &Path,
    on_progress: &impl Fn(&str),
) -> Result<(), SharedError> {
    let script_path = stage_script(ctx, instance_id, script).await?;
    let origin = catalog_origin(ctx.remote.url());
    on_progress(&format!("packaging {instance_id} with {script}"));
    let mut command = tokio::process::Command::new("bash");
    command.arg(&script_path).args(args).arg(dest);
    command.env("HEARTH_CATALOG_ORIGIN", &origin);
    if let Some(dir) = script_path.parent() {
        command.current_dir(dir);
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(45 * 60), command.output())
        .await
        .map_err(|_| SharedError(format!("{instance_id}: packaging script timed out")))?
        .map_err(|e| {
            SharedError(format!(
                "{instance_id}: failed to run packaging script: {e}"
            ))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(SharedError(format!(
            "{instance_id}: packaging script failed: {stderr}{stdout}"
        )));
    }
    if !dest.is_file() {
        return Err(SharedError(format!(
            "{instance_id}: packaging script did not write {}",
            dest.display()
        )));
    }
    Ok(())
}

/// A `file://` catalog runs the script that sits next to it. An `https://` catalog downloads the
/// script from the same directory as `catalog.json`.
async fn stage_script(
    ctx: &SharedContext,
    instance_id: &str,
    script: &str,
) -> Result<PathBuf, SharedError> {
    if let Some(path) = local_catalog_script(ctx.remote.url(), script) {
        return Ok(path);
    }
    let url = if script.contains("://") {
        script.to_string()
    } else {
        format!("{}/{}", catalog_origin(ctx.remote.url()), script)
    };
    let dest = ctx.downloads_dir().join(format!("{instance_id}.pack.sh"));
    download(ctx, &url, &dest, &|_| {}).await?;
    Ok(dest)
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

async fn download(
    ctx: &SharedContext,
    url: &str,
    dest: &Path,
    on_progress: &impl Fn(&str),
) -> Result<(), SharedError> {
    on_progress(&format!("downloading {url}"));
    if let Some(rest) = url.strip_prefix("file://") {
        // file:// artifacts exist for tests and local fixtures — the sha256 check below still runs.
        std::fs::copy(rest, dest).map_err(|e| SharedError(format!("failed to copy {url}: {e}")))?;
        return Ok(());
    }
    let response = ctx
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
    let bytes = response
        .bytes()
        .await
        .map_err(|e| SharedError(format!("download failed for {url}: {e}")))?;
    let mut file = std::fs::File::create(dest)
        .map_err(|e| SharedError(format!("cannot create {}: {e}", dest.display())))?;
    file.write_all(&bytes)
        .map_err(|e| SharedError(format!("cannot write {}: {e}", dest.display())))?;
    Ok(())
}

fn verify_sha256(path: &Path, expected: &str) -> Result<(), SharedError> {
    let bytes = std::fs::read(path).map_err(|e| SharedError(e.to_string()))?;
    let digest = hex_digest(&sha2::Sha256::digest(&bytes));
    if !digest.eq_ignore_ascii_case(expected) {
        let _ = std::fs::remove_file(path);
        return Err(SharedError(format!(
            "sha256 mismatch: expected {expected}, got {digest}"
        )));
    }
    Ok(())
}

/// `tar -xzf` via the system tar — present on every supported macOS, no crate needed.
async fn extract(archive: &Path, dest: &Path) -> Result<(), SharedError> {
    let output = tokio::time::timeout(
        EXTRACT_TIMEOUT,
        tokio::process::Command::new("tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(dest)
            .output(),
    )
    .await
    .map_err(|_| SharedError("tar extract timed out".to_string()))?
    .map_err(|e| SharedError(format!("failed to spawn tar: {e}")))?;
    if !output.status.success() {
        return Err(SharedError(format!(
            "tar extract failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

/// What inside the staging dir becomes `installs/<name>/<version>`: the single top-level directory
/// if there is exactly one (the usual tarball shape), otherwise the staging dir itself.
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
        let sha = hex_digest(&sha2::Sha256::digest(std::fs::read(&archive).unwrap()));
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
}
