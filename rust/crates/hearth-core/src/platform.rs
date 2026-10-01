//! Platform checks and the few raw process primitives the manager needs outside the supervisor.

pub fn is_supported_hearth_platform(platform: &str) -> bool {
    platform == "darwin" || platform == "linux"
}

pub fn unsupported_platform_message(platform: &str) -> String {
    format!("Hearth manager supports macOS and Linux only; {platform} is unsupported")
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

/// Sends SIGTERM to `pid` — the graceful shutdown path a daemon already has (`DaemonLifecycle` runs
/// with `stop_services: false`, so it leaves its services running for the next daemon to re-adopt).
/// Used to replace a daemon that predates a shutdown mode this binary knows about; the caller must
/// have authenticated the pid first (the lock-ownership proof `discover` verifies), never a pid read
/// from anywhere else.
#[cfg(unix)]
pub fn terminate_pid(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .is_ok()
}

#[cfg(not(unix))]
pub fn terminate_pid(_pid: i64) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_platforms() {
        assert!(is_supported_hearth_platform("darwin"));
        assert!(is_supported_hearth_platform("linux"));
        assert!(!is_supported_hearth_platform("win32"));
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

    #[cfg(unix)]
    #[test]
    fn terminate_pid_actually_ends_a_live_process() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        assert!(!terminate_pid(0), "a non-positive pid is never signalled");
        // Its own process group, so nothing here can signal the test harness itself.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i64;
        assert!(is_pid_alive(pid));
        assert!(terminate_pid(pid));
        // `wait` reaps it and reports the signal it died from.
        let status = child.wait().unwrap();
        assert_eq!(
            status.signal(),
            Some(nix::sys::signal::Signal::SIGTERM as i32)
        );
        assert!(!is_pid_alive(pid));
    }
}
