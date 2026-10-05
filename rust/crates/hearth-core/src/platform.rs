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
///
/// A zombie is not alive. `kill(pid, 0)` still succeeds for one, and the process that spawned a
/// daemon (`hearth tui`, via `spawn` without a wait) is the parent, so the exited daemon stays a
/// zombie until that parent reaps it. Treating the zombie as alive makes `manager restart` sit
/// for the whole stop timeout and makes `claim_lock` spin. An exited child is reaped here. A
/// zombie parented elsewhere is confirmed with the process table before it is called dead; an
/// unreadable table stays "alive", so a probe failure cannot steal a lock.
#[cfg(unix)]
pub fn is_pid_alive(pid: i64) -> bool {
    if pid <= 0 || pid > i64::from(i32::MAX) {
        return false;
    }
    let pid = pid as i32;
    match reap_exited_child(pid) {
        ChildPoll::Reaped => return false,
        ChildPoll::Running => return true,
        ChildPoll::NotChild => {}
    }
    if !signal_zero_succeeds(pid) {
        return false;
    }
    match zombie_status(pid) {
        ZombieStatus::Running => true,
        ZombieStatus::Exited => false,
        // `EPERM` from kill(0), or a table we could not read. A process we cannot prove dead
        // is still alive: the lock protocol must not take a false negative.
        ZombieStatus::Unknown => true,
    }
}

#[cfg(not(unix))]
pub fn is_pid_alive(_pid: i64) -> bool {
    false
}

/// Sends SIGTERM to `pid` — the graceful shutdown path a daemon already has (`DaemonLifecycle` runs
/// with `ShutdownMode::LeaveServices`, so it leaves its services running for the next daemon to re-adopt).
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

#[cfg(unix)]
enum ChildPoll {
    /// `waitpid` collected an exited child. The pid is gone.
    Reaped,
    /// Our child, and it has not exited.
    Running,
    /// Not our child (`ECHILD`), or `waitpid` failed. Fall through to the table check.
    NotChild,
}

#[cfg(unix)]
enum ZombieStatus {
    Running,
    Exited,
    Unknown,
}

/// Collects `pid` if it is an exited child of this process. `WNOHANG` does not stop a child
/// that is still running, and it does not touch a pid we did not spawn.
#[cfg(unix)]
fn reap_exited_child(pid: i32) -> ChildPoll {
    if pid <= 0 {
        return ChildPoll::NotChild;
    }
    loop {
        let mut status = 0;
        // Safety: `pid` is a specific child, `WNOHANG` only reports an exit that already
        // happened, and `status` is a valid out-pointer. A non-child returns `ECHILD`.
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if rc == pid {
            return ChildPoll::Reaped;
        }
        if rc == 0 {
            return ChildPoll::Running;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return ChildPoll::NotChild;
    }
}

#[cfg(unix)]
fn signal_zero_succeeds(pid: i32) -> bool {
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// `kill(pid, 0)` has already succeeded. Distinguish a running process from a zombie.
#[cfg(all(unix, target_os = "linux"))]
fn zombie_status(pid: i32) -> ZombieStatus {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => match proc_stat_state(&stat) {
            Some('Z') => ZombieStatus::Exited,
            Some(_) => ZombieStatus::Running,
            None => ps_zombie_status(pid),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ZombieStatus::Exited,
        Err(_) => ps_zombie_status(pid),
    }
}

/// State character after the comm field. `comm` is wrapped in parentheses and may itself
/// contain spaces and parentheses, so the state is the token after the last `)`.
#[cfg(all(unix, target_os = "linux"))]
fn proc_stat_state(stat: &str) -> Option<char> {
    let end = stat.rfind(')')?;
    stat[end + 1..].split_whitespace().next()?.chars().next()
}

/// `proc_pidinfo` fills a buffer for a live process and returns 0 for a zombie (and for a pid
/// that disappeared between `kill` and this call). A 0 is not proof on its own — confirm with
/// `ps` so a transient probe failure cannot mark a live manager dead.
#[cfg(all(unix, target_os = "macos"))]
fn zombie_status(pid: i32) -> ZombieStatus {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    // Safety: `info` is a valid out-buffer of the size we pass. `proc_pidinfo` only writes
    // that many bytes. A return of 0 writes nothing.
    let wrote = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast::<libc::c_void>(),
            std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
        )
    };
    if wrote > 0 {
        return ZombieStatus::Running;
    }
    ps_zombie_status(pid)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn zombie_status(pid: i32) -> ZombieStatus {
    ps_zombie_status(pid)
}

/// `ps -o stat=` for one pid. `Z` (or `Z+`, `Zs`) is a zombie. Exit status 1 means the pid
/// is already gone. A spawn failure or a hung `ps` is `Unknown` — the caller keeps the pid
/// alive rather than stealing a lock. The child is in its own process group so the timeout
/// cannot signal this process.
#[cfg(unix)]
fn ps_zombie_status(pid: i32) -> ZombieStatus {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut command = Command::new("ps");
    command
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return ZombieStatus::Unknown,
    };
    let deadline = Instant::now() + Duration::from_secs(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return ZombieStatus::Unknown;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return ZombieStatus::Unknown,
        }
    };
    if !status.success() {
        return ZombieStatus::Exited;
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut out);
    }
    let stat = out.trim();
    if stat.is_empty() {
        ZombieStatus::Unknown
    } else if stat.starts_with('Z') {
        ZombieStatus::Exited
    } else {
        ZombieStatus::Running
    }
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

    #[cfg(unix)]
    #[test]
    fn is_pid_alive_does_not_reap_a_running_child() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i64;
        assert!(is_pid_alive(pid), "a running child is alive");
        // The liveness check must not have collected the child, or this wait fails.
        assert!(terminate_pid(pid));
        assert!(child.wait().is_ok());
        assert!(!is_pid_alive(pid));
    }

    #[cfg(unix)]
    #[test]
    fn is_pid_alive_is_false_for_an_unreaped_exited_child() {
        use std::os::unix::process::CommandExt;
        use std::time::{Duration, Instant};
        let mut child = std::process::Command::new("true")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i64;
        let deadline = Instant::now() + Duration::from_secs(2);
        while is_pid_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "an exited child stayed alive to kill(pid, 0)"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // Reaped by the liveness check. The handle must not still see a running child,
        // and it must not be able to leave a zombie behind.
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => {}
            Ok(None) => panic!("exited child was still running"),
        }
    }
}
