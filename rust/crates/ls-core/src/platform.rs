//! Port of `src/core/platform.ts`.

pub fn is_supported_local_services_platform(platform: &str) -> bool {
    platform == "darwin" || platform == "linux"
}

pub fn unsupported_platform_message(platform: &str) -> String {
    format!("Local services manager supports macOS and Linux only; {platform} is unsupported")
}

pub fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UnsupportedPlatformError(String);

pub fn require_supported_local_services_platform(platform: &str) -> Result<(), UnsupportedPlatformError> {
    if is_supported_local_services_platform(platform) {
        Ok(())
    } else {
        Err(UnsupportedPlatformError(unsupported_platform_message(platform)))
    }
}

/// True when `pid` is alive (including "alive but owned by someone else", `EPERM`) — never confuse
/// a denied signal with a dead process. Used by the lock-claim protocol to avoid stealing a lock
/// from a live-but-slow-to-answer-healthchecks manager: a production incident (263 concurrent
/// daemons under load) was caused by treating a health-check timeout as proof of death instead of
/// checking the PID directly. **Do not weaken this to a timeout-based check** — see that incident.
#[cfg(unix)]
pub fn is_pid_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
pub fn is_pid_alive(_pid: i64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_platforms() {
        assert!(is_supported_local_services_platform("darwin"));
        assert!(is_supported_local_services_platform("linux"));
        assert!(!is_supported_local_services_platform("win32"));
    }

    #[test]
    fn unsupported_message_names_the_platform() {
        assert!(unsupported_platform_message("win32").contains("win32"));
    }

    #[test]
    fn is_pid_alive_rejects_non_positive() {
        assert!(!is_pid_alive(0));
        assert!(!is_pid_alive(-1));
    }

    #[test]
    fn is_pid_alive_true_for_self() {
        assert!(is_pid_alive(std::process::id() as i64));
    }

    #[test]
    fn is_pid_alive_false_for_an_implausible_pid() {
        // A pid this large will not exist on any real system.
        assert!(!is_pid_alive(i32::MAX as i64));
    }
}
