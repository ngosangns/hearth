// ShutdownMode — ported from rust/crates/hearth-core/src/manager/protocol.rs.
//
// Small typed pieces of the daemon HTTP contract shared by the manager, daemon
// lifecycle and clients.
package manager

// ShutdownMode selects how a manager/daemon shutdown treats already-running
// managed services.
//
// Wire values on POST /v1/manager/shutdown:
//   - "leave-services" → LeaveServices (also the default for SIGTERM /
//     run_daemon(..., LeaveServices) — processes outlive the daemon and are
//     re-adopted)
//   - "stop-services" → StopServices
//   - "refuse-if-active" is a pre-shutdown guard on the HTTP handler, not a
//     shutdown mode; once past the guard it shuts down as LeaveServices.
type ShutdownMode int

const (
	// LeaveServices detaches managed processes for the next daemon to re-adopt.
	LeaveServices ShutdownMode = iota
	// StopServices stops managed processes first.
	StopServices
)

// StopsServices reports whether this mode stops managed services.
func (m ShutdownMode) StopsServices() bool { return m == StopServices }
