//! `hearthd update` installs the GitHub release asset `hearthd-vX.Y.Z` as
//! `~/.local/share/hearth/bin/hearthd-X.Y.Z` and points `~/.local/bin/hearthd` at it.
//!
//! The swap renames a new inode into place. Overwriting a mapped ad-hoc-signed binary, or running
//! `codesign --force` on that inode, SIGKILLs the process on macOS, so a download is never
//! re-signed and the previous file is kept for one generation.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::Io;

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/ngosangns/hearth/releases/latest";
const USAGE: &str = "usage: hearthd update [--check] [--json] [--force]";
const RESTART_NOTE: &str = "running daemons keep the previous binary until `hearthd --root <project> manager restart` (services stay up) and until `hearthd shared` is restarted for smp";

#[derive(Clone, Debug)]
struct Layout {
    link: PathBuf,
    dir: PathBuf,
}

impl Layout {
    fn from_home(home: &Path) -> Self {
        Self {
            link: home.join(".local/bin/hearthd"),
            dir: home.join(".local/share/hearth/bin"),
        }
    }

    fn versioned(&self, version: &str) -> PathBuf {
        self.dir.join(format!("hearthd-{version}"))
    }
}

struct UpdateEnv<'a> {
    args: &'a [String],
    current_version: &'a str,
    current_exe: &'a Path,
    layout: Layout,
    platform_supported: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
}

impl std::fmt::Display for SemVer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Debug)]
struct Release {
    version: String,
    asset: String,
    size: u64,
    sha256: String,
    url: String,
}

#[derive(Debug, Clone)]
enum FetchError {
    Status(u16),
    Message(String),
}

trait ReleaseTransport {
    fn fetch_latest(&self) -> impl std::future::Future<Output = Result<String, FetchError>> + Send;
    fn download(
        &self,
        url: &str,
        dest: &Path,
    ) -> impl std::future::Future<Output = Result<(), FetchError>> + Send;
}

#[derive(Debug)]
struct UpdateFlags {
    check: bool,
    json: bool,
    force: bool,
}

struct Report {
    current: String,
    latest: Option<String>,
    update_available: bool,
    asset: Option<String>,
    install_path: String,
    error: Option<String>,
}

enum LinkBackup {
    Missing,
    Symlink(PathBuf),
    RenamedFile(PathBuf),
}

struct UpdateLock {
    file: std::fs::File,
}

/// Runs `hearthd update`. `current_version` is the hearthd package version, not hearth-cli's.
pub async fn run(args: &[String], current_version: &str, io: &mut Io<'_>) -> i32 {
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            (io.err)(&format!(
                "hearthd update: cannot resolve the current executable: {error}"
            ));
            return 1;
        }
    };
    let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    else {
        (io.err)("hearthd update: HOME is not set");
        return 1;
    };
    let client = match GitHubReleaseClient::production() {
        Ok(client) => client,
        Err(error) => {
            (io.err)(&format!(
                "hearthd update: {}",
                fetch_message(&error, "GitHub request failed")
            ));
            return 1;
        }
    };
    let env = UpdateEnv {
        args,
        current_version,
        current_exe: &current_exe,
        layout: Layout::from_home(&home),
        platform_supported: cfg!(all(target_os = "macos", target_arch = "aarch64")),
    };
    run_with(&env, &client, &smoke_binary, io).await
}

async fn run_with(
    env: &UpdateEnv<'_>,
    transport: &impl ReleaseTransport,
    smoke: &(impl Fn(&Path) -> Result<String, String> + Sync),
    io: &mut Io<'_>,
) -> i32 {
    if env.args.len() == 1 && matches!(env.args[0].as_str(), "--help" | "-h") {
        (io.out)(USAGE);
        return 0;
    }
    let flags = match parse_update_args(env.args) {
        Ok(flags) => flags,
        Err(message) => {
            (io.err)(&message);
            return 2;
        }
    };
    let current = match parse_semver(env.current_version) {
        Some(version) => version,
        None => {
            return fail(
                io,
                flags.json,
                &report_base(env, None),
                &format!(
                    "this binary's version {:?} is not X.Y.Z",
                    env.current_version
                ),
            );
        }
    };
    if !env.platform_supported {
        return fail(
            io,
            flags.json,
            &report_base(env, None),
            "the release asset is darwin-arm64 only",
        );
    }
    if !install_owned(env.current_exe, &env.layout) {
        return fail(
            io,
            flags.json,
            &report_base(env, None),
            &format!(
                "hearthd update only replaces {} (this process is {})",
                env.layout.link.display(),
                env.current_exe.display()
            ),
        );
    }

    let document = match transport.fetch_latest().await {
        Ok(body) => body,
        Err(error) => {
            return fail(
                io,
                flags.json,
                &report_base(env, None),
                &fetch_message(&error, "GitHub request failed"),
            );
        }
    };
    let release = match parse_release(&document) {
        Ok(release) => release,
        Err(error) => return fail(io, flags.json, &report_base(env, None), &error),
    };
    let latest = parse_semver(&release.version).expect("parse_release only returns X.Y.Z");
    let mut report = report_base(env, Some(&release));
    report.update_available = latest > current;

    if flags.check || latest < current || (latest == current && !flags.force) {
        let human = if latest > current {
            format!("hearthd {latest} is available (current {current})")
        } else if latest == current {
            format!("hearthd {current} is already up to date")
        } else {
            format!("hearthd {current} is newer than the latest release {latest}")
        };
        return succeed(io, flags.json, &report, &human);
    }

    let _lock = match UpdateLock::acquire(&env.layout.dir) {
        Ok(lock) => lock,
        Err(error) => return fail(io, flags.json, &report, &error),
    };
    let inode = match file_id(&env.layout.link) {
        Ok(id) => id,
        Err(error) => {
            return fail(
                io,
                flags.json,
                &report,
                &format!("cannot stat {}: {error}", env.layout.link.display()),
            )
        }
    };

    let partial = env.layout.dir.join(format!(
        ".hearthd-{}.{}.partial",
        release.version,
        std::process::id()
    ));
    // Removed on drop, including when a later step moves the bytes back to this path.
    let _partial_guard = PartialFile(partial.clone());
    if let Err(error) = transport.download(&release.url, &partial).await {
        return fail(
            io,
            flags.json,
            &report,
            &fetch_message(&error, "download failed"),
        );
    }
    if let Err(error) = verify_file(&partial, release.size, &release.sha256) {
        return fail(io, flags.json, &report, &error);
    }
    if let Err(error) = ensure_executable(&partial) {
        return fail(io, flags.json, &report, &error);
    }
    match smoke(&partial) {
        Ok(printed) if printed == release.version => {}
        Ok(printed) => {
            return fail(
                io,
                flags.json,
                &report,
                &format!(
                    "smoke test printed \"hearthd {printed}\", expected hearthd {}",
                    release.version
                ),
            );
        }
        Err(error) => return fail(io, flags.json, &report, &error),
    }

    match file_id(&env.layout.link) {
        Ok(now) if now == inode => {}
        Ok(_) => {
            return fail(
                io,
                flags.json,
                &report,
                "install path changed while updating",
            )
        }
        Err(error) => {
            return fail(
                io,
                flags.json,
                &report,
                &format!("cannot stat {}: {error}", env.layout.link.display()),
            )
        }
    }

    let final_path = env.layout.versioned(&release.version);
    let previous_file = match rotate_into_place(&partial, &final_path) {
        Ok(previous) => previous,
        Err(error) => return fail(io, flags.json, &report, &error),
    };
    let previous_version = std::fs::read_link(&env.layout.link)
        .ok()
        .and_then(|target| version_from_name(&target));
    if let Err(error) = publish_link(&env.layout, &final_path) {
        if let Some(backup) = previous_file {
            let _ = std::fs::rename(&final_path, &partial);
            let _ = std::fs::rename(&backup, &final_path);
        } else if previous_version.as_deref() != Some(release.version.as_str()) {
            let _ = std::fs::remove_file(&final_path);
        }
        return fail(io, flags.json, &report, &error);
    }

    let mut keep = vec![release.version.as_str()];
    if let Some(previous) = previous_version.as_deref() {
        keep.push(previous);
    }
    prune_old_versions(&env.layout.dir, &keep);

    report.update_available = false;
    report.install_path = final_path.display().to_string();
    let human = if latest > current {
        format!("updated hearthd to {latest}\n{RESTART_NOTE}")
    } else {
        format!("reinstalled hearthd {latest}\n{RESTART_NOTE}")
    };
    succeed(io, flags.json, &report, &human)
}

fn parse_update_args(args: &[String]) -> Result<UpdateFlags, String> {
    let mut flags = UpdateFlags {
        check: false,
        json: false,
        force: false,
    };
    for arg in args {
        match arg.as_str() {
            "--check" if !flags.check => flags.check = true,
            "--json" if !flags.json => flags.json = true,
            "--force" if !flags.force => flags.force = true,
            "--check" | "--json" | "--force" => return Err(format!("duplicate flag: {arg}")),
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
            _ => return Err(USAGE.to_string()),
        }
    }
    Ok(flags)
}

fn parse_semver(text: &str) -> Option<SemVer> {
    if text.is_empty()
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(SemVer {
        major,
        minor,
        patch,
    })
}

fn parse_release(document: &str) -> Result<Release, String> {
    let parsed: Value = serde_json::from_str(document)
        .map_err(|_| "GitHub release payload is not valid JSON".to_string())?;
    let tag = parsed.get("tag_name").and_then(Value::as_str).unwrap_or("");
    let version = tag
        .strip_prefix('v')
        .and_then(parse_semver)
        .ok_or_else(|| format!("release tag {tag:?} is not vX.Y.Z"))?;
    if parsed.get("draft").and_then(Value::as_bool) != Some(false) {
        return Err("latest release is a draft".to_string());
    }
    if parsed.get("prerelease").and_then(Value::as_bool) != Some(false) {
        return Err("latest release is a prerelease".to_string());
    }
    let name = format!("hearthd-{tag}");
    let assets = parsed
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| "GitHub release payload is missing assets".to_string())?;
    let mut matching = assets
        .iter()
        .filter(|asset| asset.get("name").and_then(Value::as_str) == Some(name.as_str()));
    let Some(asset) = matching.next() else {
        return Err(format!("release {tag} has no asset named {name}"));
    };
    if matching.next().is_some() {
        return Err(format!(
            "release {tag} has more than one asset named {name}"
        ));
    }
    if asset.get("state").and_then(Value::as_str) != Some("uploaded") {
        return Err(format!("asset {name} is not uploaded"));
    }
    let size = asset
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("asset {name} has an empty size"))?;
    if size == 0 {
        return Err(format!("asset {name} has an empty size"));
    }
    let digest = asset
        .get("digest")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("asset {name} digest is missing"))?;
    let sha256 = parse_digest(digest)
        .ok_or_else(|| "asset digest must be sha256 and 64 hex characters".to_string())?;
    let expected_url =
        format!("https://github.com/ngosangns/hearth/releases/download/{tag}/{name}");
    let url = asset
        .get("browser_download_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    if url != expected_url {
        return Err(format!("download url must be {expected_url}"));
    }
    Ok(Release {
        version: version.to_string(),
        asset: name,
        size,
        sha256,
        url: url.to_string(),
    })
}

fn parse_digest(digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(hex.to_ascii_lowercase())
    } else {
        None
    }
}

fn fetch_message(error: &FetchError, prefix: &str) -> String {
    match error {
        FetchError::Status(403 | 429) => {
            "GitHub rate limit reached; set GITHUB_TOKEN or GH_TOKEN".to_string()
        }
        FetchError::Status(code) => format!("{prefix}: HTTP {code}"),
        FetchError::Message(message) => message.clone(),
    }
}

fn verify_file(path: &Path, expected_size: u64, expected_sha: &str) -> Result<(), String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        size += read as u64;
        hasher.update(&buffer[..read]);
    }
    if size != expected_size {
        return Err(format!(
            "download size mismatch: expected {expected_size} bytes, got {size}"
        ));
    }
    let got = hearth_core::shared::hex(&hasher.finalize());
    if got != expected_sha {
        return Err(format!(
            "sha256 mismatch: expected {expected_sha}, got {got}"
        ));
    }
    Ok(())
}

fn ensure_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("cannot chmod {}: {error}", path.display()))?;
    let mode = std::fs::metadata(path)
        .map_err(|error| format!("cannot stat {}: {error}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o111 == 0 {
        return Err("downloaded binary is not executable".to_string());
    }
    Ok(())
}

/// `--version` on the staged binary. A non-zero exit or any other text keeps the previous install.
fn smoke_binary(path: &Path) -> Result<String, String> {
    let mut child = std::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run {}: {error}", path.display()))?;
    let status = wait_timeout(&mut child, Duration::from_secs(30))?;
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if !status.success() {
        let code = status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string());
        return Err(format!("smoke test failed (exit {code})"));
    }
    let line = stdout.lines().next().unwrap_or("").trim();
    let Some(version) = line.strip_prefix("hearthd ") else {
        return Err(format!("smoke test printed {line:?}"));
    };
    let version = version.trim();
    if parse_semver(version).is_none() {
        return Err(format!("smoke test printed {line:?}"));
    }
    Ok(version.to_string())
}

fn wait_timeout(
    child: &mut std::process::Child,
    limit: Duration,
) -> Result<std::process::ExitStatus, String> {
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("smoke test timed out".to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(format!("smoke test failed: {error}")),
        }
    }
}

fn install_owned(current_exe: &Path, layout: &Layout) -> bool {
    if path_is_inside(current_exe, &layout.dir) {
        return true;
    }
    match std::fs::symlink_metadata(&layout.link) {
        Ok(meta) if meta.file_type().is_file() => {
            let exe = current_exe
                .canonicalize()
                .unwrap_or_else(|_| current_exe.to_path_buf());
            let link = layout
                .link
                .canonicalize()
                .unwrap_or_else(|_| layout.link.clone());
            exe == link
        }
        _ => false,
    }
}

fn path_is_inside(path: &Path, dir: &Path) -> bool {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    path.starts_with(&dir) && path != dir
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileId {
    dev: u64,
    ino: u64,
}

fn file_id(path: &Path) -> std::io::Result<Option<FileId>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(FileId {
            dev: meta.dev(),
            ino: meta.ino(),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

struct PartialFile(PathBuf);

impl Drop for PartialFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Moves `source` onto `dest` by renaming. An existing `dest` is renamed aside first so a process
/// that mapped it keeps that inode.
fn rotate_into_place(source: &Path, dest: &Path) -> Result<Option<PathBuf>, String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let mut previous = None;
    if std::fs::symlink_metadata(dest).is_ok() {
        let backup = dest.with_file_name(format!(
            "{}.previous",
            dest.file_name().unwrap_or_default().to_string_lossy()
        ));
        if std::fs::symlink_metadata(&backup).is_ok() {
            std::fs::remove_file(&backup)
                .map_err(|error| format!("cannot replace {}: {error}", backup.display()))?;
        }
        std::fs::rename(dest, &backup)
            .map_err(|error| format!("cannot move {} aside: {error}", dest.display()))?;
        previous = Some(backup);
    }
    if let Err(error) = std::fs::rename(source, dest) {
        if let Some(backup) = &previous {
            let _ = std::fs::rename(backup, dest);
        }
        return Err(format!("cannot install {}: {error}", dest.display()));
    }
    Ok(previous)
}

fn publish_link(layout: &Layout, versioned: &Path) -> Result<LinkBackup, String> {
    let parent = layout
        .link
        .parent()
        .ok_or_else(|| format!("cannot install {}", layout.link.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let relative = relative_from(parent, versioned);
    let backup = match std::fs::symlink_metadata(&layout.link) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LinkBackup::Missing,
        Err(error) => return Err(format!("cannot stat {}: {error}", layout.link.display())),
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = std::fs::read_link(&layout.link)
                .map_err(|error| format!("cannot read {}: {error}", layout.link.display()))?;
            LinkBackup::Symlink(target)
        }
        Ok(meta) if meta.file_type().is_file() => {
            let aside = unique_aside(&layout.dir);
            std::fs::rename(&layout.link, &aside)
                .map_err(|error| format!("cannot move {} aside: {error}", layout.link.display()))?;
            LinkBackup::RenamedFile(aside)
        }
        Ok(_) => {
            return Err(format!(
                "{} is not a file or a symlink",
                layout.link.display()
            ))
        }
    };
    if let Err(error) = place_symlink(&layout.link, &relative) {
        let _ = restore_link(&layout.link, &backup);
        return Err(error);
    }
    Ok(backup)
}

fn restore_link(link: &Path, backup: &LinkBackup) -> Result<(), String> {
    match backup {
        LinkBackup::Missing => {
            if std::fs::symlink_metadata(link).is_ok() {
                std::fs::remove_file(link)
                    .map_err(|error| format!("cannot remove {}: {error}", link.display()))?;
            }
            Ok(())
        }
        LinkBackup::Symlink(target) => place_symlink(link, target),
        LinkBackup::RenamedFile(aside) => {
            if std::fs::symlink_metadata(link).is_ok() {
                std::fs::remove_file(link)
                    .map_err(|error| format!("cannot remove {}: {error}", link.display()))?;
            }
            std::fs::rename(aside, link)
                .map_err(|error| format!("cannot restore {}: {error}", link.display()))?;
            Ok(())
        }
    }
}

fn place_symlink(link: &Path, target: &Path) -> Result<(), String> {
    let parent = link
        .parent()
        .ok_or_else(|| format!("cannot symlink {}", link.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let tmp = parent.join(format!(".hearthd-link-{}", uuid::Uuid::new_v4()));
    std::os::unix::fs::symlink(target, &tmp)
        .map_err(|error| format!("cannot symlink {}: {error}", link.display()))?;
    if let Err(error) = std::fs::rename(&tmp, link) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot replace {}: {error}", link.display()));
    }
    Ok(())
}

fn unique_aside(dir: &Path) -> PathBuf {
    let path = dir.join(format!("hearthd-previous-{}", std::process::id()));
    if std::fs::symlink_metadata(&path).is_ok() {
        dir.join(format!(
            "hearthd-previous-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    } else {
        path
    }
}

fn relative_from(base: &Path, target: &Path) -> PathBuf {
    let base: Vec<_> = base.components().collect();
    let target: Vec<_> = target.components().collect();
    let mut shared = 0;
    while shared < base.len() && shared < target.len() && base[shared] == target[shared] {
        shared += 1;
    }
    let mut out = PathBuf::new();
    for _ in shared..base.len() {
        out.push("..");
    }
    for component in &target[shared..] {
        out.push(component.as_os_str());
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

fn version_from_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let version = name.strip_prefix("hearthd-")?;
    parse_semver(version)?;
    Some(version.to_string())
}

fn prune_old_versions(dir: &Path, keep: &[&str]) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(version) = name.strip_prefix("hearthd-") else {
            continue;
        };
        if parse_semver(version).is_some() && !keep.contains(&version) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

impl UpdateLock {
    fn acquire(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let path = dir.join(".update.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        // SAFETY: `file` is an open descriptor. LOCK_EX | LOCK_NB does not touch memory, and the
        // descriptor stays open until this guard drops, which releases the lock.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == Some(libc::EAGAIN)
            {
                return Err("hearthd update is already running".to_string());
            }
            return Err(format!("cannot lock {}: {error}", path.display()));
        }
        Ok(Self { file })
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        // SAFETY: same descriptor as acquire. Unlocking a lock this process holds is defined.
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn report_base(env: &UpdateEnv<'_>, release: Option<&Release>) -> Report {
    Report {
        current: env.current_version.to_string(),
        latest: release.map(|release| release.version.clone()),
        update_available: false,
        asset: release.map(|release| release.asset.clone()),
        install_path: release
            .map(|release| env.layout.versioned(&release.version).display().to_string())
            .unwrap_or_else(|| env.layout.link.display().to_string()),
        error: None,
    }
}

fn succeed(io: &mut Io<'_>, json_mode: bool, report: &Report, human: &str) -> i32 {
    emit(io, json_mode, report, human, true);
    0
}

fn fail(io: &mut Io<'_>, json_mode: bool, report: &Report, error: &str) -> i32 {
    let report = Report {
        current: report.current.clone(),
        latest: report.latest.clone(),
        update_available: report.update_available,
        asset: report.asset.clone(),
        install_path: report.install_path.clone(),
        error: Some(error.to_string()),
    };
    emit(io, json_mode, &report, error, false);
    1
}

fn emit(io: &mut Io<'_>, json_mode: bool, report: &Report, human: &str, ok: bool) {
    if json_mode {
        (io.out)(&json_report(report));
        return;
    }
    if ok {
        for line in human.split('\n') {
            (io.out)(line);
        }
    } else {
        (io.err)(&format!("hearthd update: {human}"));
    }
}

fn json_report(report: &Report) -> String {
    let mut map = serde_json::Map::new();
    map.insert("current".to_string(), json!(report.current));
    map.insert("latest".to_string(), json!(report.latest));
    map.insert(
        "updateAvailable".to_string(),
        json!(report.update_available),
    );
    map.insert("asset".to_string(), json!(report.asset));
    map.insert("installPath".to_string(), json!(report.install_path));
    if let Some(error) = &report.error {
        map.insert("error".to_string(), json!(error));
    }
    Value::Object(map).to_string()
}

struct GitHubReleaseClient {
    http: reqwest::Client,
    latest_url: String,
    token: Option<String>,
}

impl GitHubReleaseClient {
    fn production() -> Result<Self, FetchError> {
        Self::new(LATEST_RELEASE_URL.to_string(), github_token())
    }

    fn new(latest_url: String, token: Option<String>) -> Result<Self, FetchError> {
        let http = reqwest::Client::builder()
            .user_agent("hearthd")
            .redirect(reqwest::redirect::Policy::limited(10))
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| FetchError::Message(format!("cannot build HTTP client: {error}")))?;
        Ok(Self {
            http,
            latest_url,
            token,
        })
    }
}

impl ReleaseTransport for GitHubReleaseClient {
    async fn fetch_latest(&self) -> Result<String, FetchError> {
        self.request_text(&self.latest_url, Duration::from_secs(30))
            .await
    }

    async fn download(&self, url: &str, dest: &Path) -> Result<(), FetchError> {
        let response = self.send(url, Duration::from_secs(600)).await?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                FetchError::Message(format!("cannot create {}: {error}", parent.display()))
            })?;
        }
        let mut file = tokio::fs::File::create(dest).await.map_err(|error| {
            FetchError::Message(format!("cannot create {}: {error}", dest.display()))
        })?;
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| FetchError::Message(format!("download failed: {error}")))?
        {
            use tokio::io::AsyncWriteExt;
            file.write_all(&chunk).await.map_err(|error| {
                FetchError::Message(format!("cannot write {}: {error}", dest.display()))
            })?;
        }
        use tokio::io::AsyncWriteExt;
        file.flush().await.map_err(|error| {
            FetchError::Message(format!("cannot write {}: {error}", dest.display()))
        })?;
        Ok(())
    }
}

impl GitHubReleaseClient {
    async fn request_text(&self, url: &str, timeout: Duration) -> Result<String, FetchError> {
        let response = self.send(url, timeout).await?;
        response
            .text()
            .await
            .map_err(|error| FetchError::Message(format!("GitHub request failed: {error}")))
    }

    async fn send(&self, url: &str, timeout: Duration) -> Result<reqwest::Response, FetchError> {
        let mut request = self
            .http
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "hearthd")
            .timeout(timeout);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| FetchError::Message(format!("GitHub request failed: {error}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(FetchError::Status(status.as_u16()));
        }
        Ok(response)
    }
}

fn github_token() -> Option<String> {
    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?;
    let token = token.trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    struct World {
        _tmp: tempfile::TempDir,
        layout: Layout,
    }

    fn world() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::from_home(tmp.path());
        std::fs::create_dir_all(&layout.dir).unwrap();
        std::fs::create_dir_all(layout.link.parent().unwrap()).unwrap();
        World { _tmp: tmp, layout }
    }

    fn seed_regular(world: &World, bytes: &[u8]) -> PathBuf {
        std::fs::write(&world.layout.link, bytes).unwrap();
        world.layout.link.clone()
    }

    fn seed_symlink(world: &World, version: &str, bytes: &[u8]) -> PathBuf {
        let path = world.layout.versioned(version);
        std::fs::write(&path, bytes).unwrap();
        let relative = relative_from(world.layout.link.parent().unwrap(), &path);
        std::os::unix::fs::symlink(&relative, &world.layout.link).unwrap();
        path
    }

    struct Scripted {
        latest: Mutex<Result<String, FetchError>>,
        body: Vec<u8>,
        downloads: AtomicUsize,
        on_download: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    }

    impl Scripted {
        fn new(latest: Result<String, FetchError>, body: Vec<u8>) -> Self {
            Self {
                latest: Mutex::new(latest),
                body,
                downloads: AtomicUsize::new(0),
                on_download: Mutex::new(None),
            }
        }

        fn downloads(&self) -> usize {
            self.downloads.load(Ordering::SeqCst)
        }
    }

    impl ReleaseTransport for Scripted {
        async fn fetch_latest(&self) -> Result<String, FetchError> {
            self.latest.lock().unwrap().clone()
        }

        async fn download(&self, _url: &str, dest: &Path) -> Result<(), FetchError> {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            if let Some(hook) = self.on_download.lock().unwrap().as_ref() {
                hook();
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(dest, &self.body).unwrap();
            Ok(())
        }
    }

    fn document(tag: &str, bytes: &[u8]) -> String {
        let sha = hearth_core::shared::hex(&Sha256::digest(bytes));
        json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "assets": [{
                "name": format!("hearthd-{tag}"),
                "state": "uploaded",
                "size": bytes.len(),
                "digest": format!("sha256:{sha}"),
                "browser_download_url": format!("https://github.com/ngosangns/hearth/releases/download/{tag}/hearthd-{tag}"),
            }]
        })
        .to_string()
    }

    fn mutate(document: &str, edit: impl FnOnce(&mut Value)) -> String {
        let mut value: Value = serde_json::from_str(document).unwrap();
        edit(&mut value);
        value.to_string()
    }

    struct Captured {
        code: i32,
        out: Vec<String>,
        err: Vec<String>,
    }

    async fn exec(
        env: &UpdateEnv<'_>,
        transport: &impl ReleaseTransport,
        smoke: &(impl Fn(&Path) -> Result<String, String> + Sync),
    ) -> Captured {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut io = Io {
                out: &mut |line: &str| out.push(line.to_string()),
                err: &mut |line: &str| err.push(line.to_string()),
                confirm: None,
            };
            run_with(env, transport, smoke, &mut io).await
        };
        Captured { code, out, err }
    }

    fn env<'a>(
        world: &'a World,
        exe: &'a Path,
        version: &'a str,
        argv: &'a [String],
    ) -> UpdateEnv<'a> {
        UpdateEnv {
            args: argv,
            current_version: version,
            current_exe: exe,
            layout: world.layout.clone(),
            platform_supported: true,
        }
    }

    fn ok_smoke(version: &'static str) -> impl Fn(&Path) -> Result<String, String> + Sync {
        move |_path| Ok(version.to_string())
    }

    fn link_target(world: &World) -> PathBuf {
        std::fs::read_link(&world.layout.link).unwrap()
    }

    #[tokio::test]
    async fn newer_release_installs_a_versioned_symlink() {
        let world = world();
        let exe = seed_regular(&world, b"old-regular");
        let before = file_id(&exe).unwrap().unwrap();
        let bytes = b"new-bytes";
        let transport = Scripted::new(Ok(document("v0.17.0", bytes)), bytes.to_vec());
        let argv = args(&[]);
        let captured = exec(
            &env(&world, &exe, "0.16.0", &argv),
            &transport,
            &ok_smoke("0.17.0"),
        )
        .await;
        assert_eq!(captured.code, 0, "{:?}", captured.err);
        assert_eq!(transport.downloads(), 1);
        assert_eq!(
            link_target(&world),
            PathBuf::from("../share/hearth/bin/hearthd-0.17.0")
        );
        let installed = world.layout.versioned("0.17.0");
        assert_eq!(std::fs::read(&installed).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&installed).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let aside = std::fs::read_dir(&world.layout.dir)
            .unwrap()
            .flatten()
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("hearthd-previous-")
            })
            .unwrap();
        assert_eq!(std::fs::read(aside.path()).unwrap(), b"old-regular");
        assert_eq!(file_id(&aside.path()).unwrap().unwrap(), before);
        assert!(captured
            .out
            .iter()
            .any(|line| line.contains("updated hearthd to 0.17.0")));
        assert!(captured
            .out
            .iter()
            .any(|line| line.contains("manager restart")));
        assert!(captured
            .out
            .iter()
            .any(|line| line.contains("hearthd shared")));
    }

    #[tokio::test]
    async fn equal_version_does_not_download() {
        let world = world();
        let exe = seed_symlink(&world, "0.16.0", b"current");
        let bytes = b"release";
        let transport = Scripted::new(Ok(document("v0.16.0", bytes)), bytes.to_vec());
        let argv = args(&[]);
        let captured = exec(
            &env(&world, &exe, "0.16.0", &argv),
            &transport,
            &ok_smoke("0.16.0"),
        )
        .await;
        assert_eq!(captured.code, 0, "{:?}", captured.err);
        assert_eq!(transport.downloads(), 0);
        assert_eq!(
            link_target(&world),
            PathBuf::from("../share/hearth/bin/hearthd-0.16.0")
        );
        assert!(captured
            .out
            .iter()
            .any(|line| line.contains("already up to date")));
    }

    #[tokio::test]
    async fn force_reinstalls_the_same_version() {
        let world = world();
        let exe = seed_symlink(&world, "0.16.0", b"local-build");
        let bytes = b"github-asset";
        let transport = Scripted::new(Ok(document("v0.16.0", bytes)), bytes.to_vec());
        let argv = args(&["--force"]);
        let captured = exec(
            &env(&world, &exe, "0.16.0", &argv),
            &transport,
            &ok_smoke("0.16.0"),
        )
        .await;
        assert_eq!(captured.code, 0, "{:?}", captured.err);
        assert_eq!(transport.downloads(), 1);
        assert_eq!(
            std::fs::read(world.layout.versioned("0.16.0")).unwrap(),
            bytes
        );
        assert_eq!(
            link_target(&world),
            PathBuf::from("../share/hearth/bin/hearthd-0.16.0")
        );
        assert!(world
            .layout
            .versioned("0.16.0")
            .with_file_name("hearthd-0.16.0.previous")
            .is_file());
        assert!(captured
            .out
            .iter()
            .any(|line| line.contains("reinstalled hearthd 0.16.0")));
    }

    #[tokio::test]
    async fn check_does_not_download() {
        let world = world();
        let exe = seed_symlink(&world, "0.16.0", b"current");
        let bytes = b"next";
        let transport = Scripted::new(Ok(document("v0.17.0", bytes)), bytes.to_vec());
        let argv = args(&["--check", "--json"]);
        let captured = exec(
            &env(&world, &exe, "0.16.0", &argv),
            &transport,
            &ok_smoke("0.17.0"),
        )
        .await;
        assert_eq!(captured.code, 0, "{:?}", captured.err);
        assert_eq!(transport.downloads(), 0);
        let payload: Value = serde_json::from_str(&captured.out[0]).unwrap();
        assert_eq!(payload["current"], "0.16.0");
        assert_eq!(payload["latest"], "0.17.0");
        assert_eq!(payload["updateAvailable"], true);
        assert_eq!(payload["asset"], "hearthd-v0.17.0");
        assert!(payload["installPath"]
            .as_str()
            .unwrap()
            .ends_with("hearthd-0.17.0"));
        assert_eq!(
            link_target(&world),
            PathBuf::from("../share/hearth/bin/hearthd-0.16.0")
        );
    }

    async fn assert_keeps_previous(document: String, body: &[u8], needle: &str) {
        let world = world();
        let exe = seed_symlink(&world, "0.16.0", b"old");
        let transport = Scripted::new(Ok(document), body.to_vec());
        let argv = args(&[]);
        let captured = exec(
            &env(&world, &exe, "0.16.0", &argv),
            &transport,
            &ok_smoke("0.17.0"),
        )
        .await;
        assert_eq!(captured.code, 1, "{:?}", captured.out);
        assert!(
            captured.err.iter().any(|line| line.contains(needle)),
            "{:?}",
            captured.err
        );
        assert_eq!(
            link_target(&world),
            PathBuf::from("../share/hearth/bin/hearthd-0.16.0")
        );
        assert_eq!(
            std::fs::read(world.layout.versioned("0.16.0")).unwrap(),
            b"old"
        );
        assert!(!world.layout.versioned("0.17.0").exists());
    }

    #[tokio::test]
    async fn bad_digest_leaves_the_previous_symlink() {
        let bytes = b"downloaded";
        let document = mutate(&document("v0.17.0", bytes), |value| {
            value["assets"][0]["digest"] = json!(format!("sha256:{}", "ab".repeat(32)));
        });
        assert_keeps_previous(document, bytes, "sha256 mismatch").await;
    }

    #[tokio::test]
    async fn short_download_leaves_the_previous_symlink() {
        let bytes = b"short";
        let document = mutate(&document("v0.17.0", bytes), |value| {
            value["assets"][0]["size"] = json!(bytes.len() as u64 + 10);
        });
        assert_keeps_previous(document, bytes, "download size mismatch").await;
    }

    #[tokio::test]
    async fn wrong_asset_name_leaves_the_previous_symlink() {
        let bytes = b"bytes";
        let document = mutate(&document("v0.17.0", bytes), |value| {
            value["assets"][0]["name"] = json!("hearthd-v0.17.0-linux");
        });
        assert_keeps_previous(document, bytes, "no asset named hearthd-v0.17.0").await;
    }

    #[tokio::test]
    async fn other_host_leaves_the_previous_symlink() {
        let bytes = b"bytes";
        let document = mutate(&document("v0.17.0", bytes), |value| {
            value["assets"][0]["browser_download_url"] =
                json!("https://example.com/hearthd-v0.17.0");
        });
        assert_keeps_previous(document, bytes, "download url must be https://github.com/ngosangns/hearth/releases/download/v0.17.0/hearthd-v0.17.0").await;
    }

    #[tokio::test]
    async fn draft_and_prerelease_are_rejected() {
        let bytes = b"bytes";
        let draft = mutate(&document("v0.17.0", bytes), |value| {
            value["draft"] = json!(true)
        });
        assert_keeps_previous(draft, bytes, "latest release is a draft").await;
        let pre = mutate(&document("v0.17.0", bytes), |value| {
            value["prerelease"] = json!(true)
        });
        assert_keeps_previous(pre, bytes, "latest release is a prerelease").await;
    }

    #[tokio::test]
    async fn malformed_digest_and_unuploaded_asset_are_rejected() {
        let bytes = b"bytes";
        let digest = mutate(&document("v0.17.0", bytes), |value| {
            value["assets"][0]["digest"] = json!("md5:abcd")
        });
        assert_keeps_previous(
            digest,
            bytes,
            "asset digest must be sha256 and 64 hex characters",
        )
        .await;
        let state = mutate(&document("v0.17.0", bytes), |value| {
            value["assets"][0]["state"] = json!("starter")
        });
        assert_keeps_previous(state, bytes, "is not uploaded").await;
    }

    #[tokio::test]
    async fn smoke_failure_rolls_back_to_the_previous_target() {
        let world = world();
        let exe = seed_symlink(&world, "0.16.0", b"old");
        let bytes = b"new";
        let transport = Scripted::new(Ok(document("v0.17.0", bytes)), bytes.to_vec());
        let argv = args(&[]);
        let smoke = |_path: &Path| Err("smoke test failed (exit 1)".to_string());
        let captured = exec(&env(&world, &exe, "0.16.0", &argv), &transport, &smoke).await;
        assert_eq!(captured.code, 1);
        assert!(captured
            .err
            .iter()
            .any(|line| line.contains("smoke test failed")));
        assert_eq!(
            link_target(&world),
            PathBuf::from("../share/hearth/bin/hearthd-0.16.0")
        );
        assert!(!world.layout.versioned("0.17.0").exists());
        let leftovers: Vec<_> = std::fs::read_dir(&world.layout.dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("partial"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[tokio::test]
    async fn smoke_wrong_version_keeps_the_previous_target() {
        let world = world();
        let exe = seed_symlink(&world, "0.16.0", b"old");
        let bytes = b"new";
        let transport = Scripted::new(Ok(document("v0.17.0", bytes)), bytes.to_vec());
        let argv = args(&[]);
        let captured = exec(
            &env(&world, &exe, "0.16.0", &argv),
            &transport,
            &ok_smoke("9.9.9"),
        )
        .await;
        assert_eq!(captured.code, 1);
        assert!(captured
            .err
            .iter()
            .any(|line| line.contains("expected hearthd 0.17.0")));
        assert_eq!(
            link_target(&world),
            PathBuf::from("../share/hearth/bin/hearthd-0.16.0")
        );
    }

    #[test]
    fn restore_rolls_back_a_symlink_and_a_regular_file() {
        let world = world();
        let old = world.layout.versioned("0.16.0");
        std::fs::write(&old, b"old").unwrap();
        let new = world.layout.versioned("0.17.0");
        std::fs::write(&new, b"new").unwrap();
        let relative_old = relative_from(world.layout.link.parent().unwrap(), &old);
        std::os::unix::fs::symlink(&relative_old, &world.layout.link).unwrap();
        let backup = publish_link(&world.layout, &new).unwrap();
        assert_eq!(
            link_target(&world),
            PathBuf::from("../share/hearth/bin/hearthd-0.17.0")
        );
        restore_link(&world.layout.link, &backup).unwrap();
        assert_eq!(link_target(&world), relative_old);

        std::fs::remove_file(&world.layout.link).unwrap();
        std::fs::write(&world.layout.link, b"regular").unwrap();
        let inode = file_id(&world.layout.link).unwrap();
        let backup = publish_link(&world.layout, &new).unwrap();
        assert!(std::fs::symlink_metadata(&world.layout.link)
            .unwrap()
            .file_type()
            .is_symlink());
        restore_link(&world.layout.link, &backup).unwrap();
        assert!(std::fs::symlink_metadata(&world.layout.link)
            .unwrap()
            .file_type()
            .is_file());
        assert_eq!(std::fs::read(&world.layout.link).unwrap(), b"regular");
        assert_eq!(file_id(&world.layout.link).unwrap(), inode);
    }

    #[tokio::test]
    async fn second_update_sees_the_lock() {
        let world = world();
        let exe = seed_symlink(&world, "0.16.0", b"old");
        let _held = UpdateLock::acquire(&world.layout.dir).unwrap();
        let bytes = b"new";
        let transport = Scripted::new(Ok(document("v0.17.0", bytes)), bytes.to_vec());
        let argv = args(&[]);
        let captured = exec(
            &env(&world, &exe, "0.16.0", &argv),
            &transport,
            &ok_smoke("0.17.0"),
        )
        .await;
        assert_eq!(captured.code, 1);
        assert!(
            captured
                .err
                .iter()
                .any(|line| line.contains("already running")),
            "{:?}",
            captured.err
        );
        assert_eq!(transport.downloads(), 0);
        assert_eq!(
            link_target(&world),
            PathBuf::from("../share/hearth/bin/hearthd-0.16.0")
        );
    }

    #[tokio::test]
    async fn refuses_a_binary_outside_the_install_path() {
        let world = world();
        let _exe = seed_symlink(&world, "0.16.0", b"old");
        let outside = world._tmp.path().join("target/release/hearthd");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, b"cargo").unwrap();
        let bytes = b"new";
        let transport = Scripted::new(Ok(document("v0.17.0", bytes)), bytes.to_vec());
        let argv = args(&[]);
        let captured = exec(
            &env(&world, &outside, "0.16.0", &argv),
            &transport,
            &ok_smoke("0.17.0"),
        )
        .await;
        assert_eq!(captured.code, 1);
        assert!(
            captured
                .err
                .iter()
                .any(|line| line.contains("only replaces")
                    && line.contains("target/release/hearthd")),
            "{:?}",
            captured.err
        );
        assert_eq!(transport.downloads(), 0);
        assert_eq!(
            link_target(&world),
            PathBuf::from("../share/hearth/bin/hearthd-0.16.0")
        );
    }

    #[tokio::test]
    async fn refuses_a_symlink_that_points_outside_the_versioned_dir() {
        let world = world();
        let outside = world._tmp.path().join("elsewhere/hearthd");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, b"other").unwrap();
        std::os::unix::fs::symlink(&outside, &world.layout.link).unwrap();
        let bytes = b"new";
        let transport = Scripted::new(Ok(document("v0.17.0", bytes)), bytes.to_vec());
        let argv = args(&[]);
        let captured = exec(
            &env(&world, &world.layout.link, "0.16.0", &argv),
            &transport,
            &ok_smoke("0.17.0"),
        )
        .await;
        assert_eq!(captured.code, 1);
        assert_eq!(transport.downloads(), 0);
        assert_eq!(std::fs::read_link(&world.layout.link).unwrap(), outside);
    }

    #[tokio::test]
    async fn refuses_a_non_darwin_arm64_platform() {
        let world = world();
        let exe = seed_regular(&world, b"old");
        let transport = Scripted::new(Err(FetchError::Message("network".to_string())), Vec::new());
        let argv = args(&[]);
        let mut update = env(&world, &exe, "0.16.0", &argv);
        update.platform_supported = false;
        let captured = exec(&update, &transport, &ok_smoke("0.17.0")).await;
        assert_eq!(captured.code, 1);
        assert!(captured
            .err
            .iter()
            .any(|line| line.contains("the release asset is darwin-arm64 only")));
        assert_eq!(transport.downloads(), 0);
        assert!(std::fs::symlink_metadata(&world.layout.link)
            .unwrap()
            .file_type()
            .is_file());
    }

    #[tokio::test]
    async fn inode_change_during_download_does_not_publish() {
        let world = world();
        let exe = seed_symlink(&world, "0.16.0", b"old");
        let bytes = b"new";
        let transport = Scripted::new(Ok(document("v0.17.0", bytes)), bytes.to_vec());
        let link = world.layout.link.clone();
        *transport.on_download.lock().unwrap() = Some(Box::new(move || {
            std::fs::remove_file(&link).unwrap();
            std::fs::write(&link, b"replaced").unwrap();
        }));
        let argv = args(&[]);
        let captured = exec(
            &env(&world, &exe, "0.16.0", &argv),
            &transport,
            &ok_smoke("0.17.0"),
        )
        .await;
        assert_eq!(captured.code, 1);
        assert!(
            captured
                .err
                .iter()
                .any(|line| line.contains("install path changed while updating")),
            "{:?}",
            captured.err
        );
        assert!(std::fs::symlink_metadata(&world.layout.link)
            .unwrap()
            .file_type()
            .is_file());
        assert_eq!(std::fs::read(&world.layout.link).unwrap(), b"replaced");
        assert!(!world.layout.versioned("0.17.0").exists());
    }

    #[tokio::test]
    async fn prunes_versions_older_than_the_previous_one() {
        let world = world();
        let exe = seed_symlink(&world, "0.15.0", b"previous");
        std::fs::write(world.layout.versioned("0.14.0"), b"ancient").unwrap();
        let bytes = b"new";
        let transport = Scripted::new(Ok(document("v0.16.0", bytes)), bytes.to_vec());
        let argv = args(&[]);
        let captured = exec(
            &env(&world, &exe, "0.15.0", &argv),
            &transport,
            &ok_smoke("0.16.0"),
        )
        .await;
        assert_eq!(captured.code, 0, "{:?}", captured.err);
        assert!(!world.layout.versioned("0.14.0").exists());
        assert_eq!(
            std::fs::read(world.layout.versioned("0.15.0")).unwrap(),
            b"previous"
        );
        assert_eq!(
            std::fs::read(world.layout.versioned("0.16.0")).unwrap(),
            bytes
        );
    }

    #[tokio::test]
    async fn does_not_downgrade() {
        let world = world();
        let exe = seed_symlink(&world, "0.18.0", b"dev");
        let bytes = b"release";
        let transport = Scripted::new(Ok(document("v0.17.0", bytes)), bytes.to_vec());
        let argv = args(&["--force"]);
        let captured = exec(
            &env(&world, &exe, "0.18.0", &argv),
            &transport,
            &ok_smoke("0.17.0"),
        )
        .await;
        assert_eq!(captured.code, 0, "{:?}", captured.err);
        assert_eq!(transport.downloads(), 0);
        assert!(captured
            .out
            .iter()
            .any(|line| line.contains("newer than the latest release")));
        assert_eq!(
            link_target(&world),
            PathBuf::from("../share/hearth/bin/hearthd-0.18.0")
        );
    }

    #[tokio::test]
    async fn rate_limit_mentions_a_token() {
        let world = world();
        let exe = seed_regular(&world, b"old");
        let transport = Scripted::new(Err(FetchError::Status(403)), Vec::new());
        let argv = args(&["--json"]);
        let captured = exec(
            &env(&world, &exe, "0.16.0", &argv),
            &transport,
            &ok_smoke("0.17.0"),
        )
        .await;
        assert_eq!(captured.code, 1);
        let payload: Value = serde_json::from_str(&captured.out[0]).unwrap();
        assert!(payload["error"].as_str().unwrap().contains("GITHUB_TOKEN"));
    }

    #[test]
    fn usage_rejects_unknown_flags_and_positionals() {
        assert!(parse_update_args(&args(&["--bogus"]))
            .unwrap_err()
            .contains("unknown flag"));
        assert!(parse_update_args(&args(&["now"]))
            .unwrap_err()
            .contains("usage:"));
        assert!(parse_update_args(&args(&["--json", "--json"]))
            .unwrap_err()
            .contains("duplicate flag"));
    }

    #[test]
    fn relative_link_matches_the_install_layout() {
        let world = world();
        let target = world.layout.versioned("0.16.0");
        let relative = relative_from(world.layout.link.parent().unwrap(), &target);
        assert_eq!(
            relative,
            PathBuf::from("../share/hearth/bin/hearthd-0.16.0")
        );
    }

    #[test]
    fn smoke_reads_a_version_line_and_rejects_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let ok = tmp.path().join("ok");
        std::fs::write(&ok, "#!/bin/sh\necho hearthd 0.2.0\n").unwrap();
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(smoke_binary(&ok).unwrap(), "0.2.0");

        let bad = tmp.path().join("bad");
        std::fs::write(&bad, "#!/bin/sh\necho nope\nexit 3\n").unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = smoke_binary(&bad).unwrap_err();
        assert!(
            error.contains("exit 3") || error.contains("nope"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn github_client_maps_403_and_writes_a_download() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 2048];
            let n = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer)
                .await
                .unwrap();
            let request = String::from_utf8_lossy(&buffer[..n]);
            let headers = request.to_ascii_lowercase();
            assert!(headers.contains("user-agent: hearthd"), "{request}");
            assert!(headers.contains("application/vnd.github+json"), "{request}");
            let body = b"rate limited";
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, body).await;
        });
        let client = GitHubReleaseClient::new(format!("http://{address}/latest"), None).unwrap();
        let error = client.fetch_latest().await.unwrap_err();
        assert!(fetch_message(&error, "GitHub request failed").contains("GITHUB_TOKEN"));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let payload = b"asset-bytes";
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 2048];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buffer).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, payload).await;
        });
        let client = GitHubReleaseClient::new(
            format!("http://{address}/latest"),
            Some("token".to_string()),
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("hearthd");
        client
            .download(&format!("http://{address}/hearthd-v0.17.0"), &dest)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        let sha = hearth_core::shared::hex(&Sha256::digest(payload));
        verify_file(&dest, payload.len() as u64, &sha).unwrap();
        let wrong = verify_file(&dest, payload.len() as u64, &"0".repeat(64)).unwrap_err();
        assert!(wrong.contains("sha256 mismatch"), "{wrong}");
    }
}
