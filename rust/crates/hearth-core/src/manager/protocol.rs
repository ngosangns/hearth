//! Small typed pieces of the daemon HTTP contract that are shared by the manager,
//! daemon lifecycle, and clients — kept out of the large `http` module.

/// How a manager/daemon shutdown should treat already-running managed services.
///
/// Wire values on `POST /v1/manager/shutdown`:
/// - `"leave-services"` → [`ShutdownMode::LeaveServices`] (also the default for SIGTERM /
///   `run_daemon(..., LeaveServices)` — processes outlive the daemon and are re-adopted)
/// - `"stop-services"` → [`ShutdownMode::StopServices`]
/// - `"refuse-if-active"` is a **pre-shutdown guard** on the HTTP handler, not a shutdown mode;
///   once past the guard it shuts down as [`ShutdownMode::LeaveServices`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
    LeaveServices,
    StopServices,
}

impl ShutdownMode {
    pub fn stops_services(self) -> bool {
        matches!(self, Self::StopServices)
    }
}
