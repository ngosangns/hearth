// Package platform ports the raw process primitives the manager needs outside
// the supervisor: liveness checks that must never answer "dead" on a failed
// probe, and targeted termination.
package platform

import (
	"runtime"
	"syscall"
	"time"
)

func IsSupportedHearthPlatform(platform string) bool {
	return platform == "darwin" || platform == "linux"
}

func UnsupportedPlatformMessage(platform string) string {
	return "Hearth manager supports macOS and Linux only; " + platform + " is unsupported"
}

func CurrentPlatform() string { return runtime.GOOS }

// IsPIDAlive reports whether pid is alive, including "alive but owned by
// someone else" (EPERM). Never confuse a denied signal with a dead process:
// a failed probe is "unknown", and unknown must not steal a lock. A zombie is
// not alive: an exited child spawned by us is reaped, a foreign zombie is
// confirmed via the process table, and an unreadable table stays "alive".
func IsPIDAlive(pid int64) bool {
	if pid <= 0 || pid > 2147483647 {
		return false
	}
	p := int(pid)
	switch reapExitedChild(p) {
	case childReaped:
		return false
	case childRunning:
		return true
	}
	if !signalZeroSucceeds(p) {
		return false
	}
	switch zombieStatus(p) {
	case zombieRunning:
		return true
	case zombieExited:
		return false
	default: // zombieUnknown — cannot prove dead => alive
		return true
	}
}

// TerminatePID sends SIGTERM to pid — the graceful path used to replace a
// daemon that predates a known shutdown mode.
func TerminatePID(pid int64) bool {
	if pid <= 0 || pid > 2147483647 {
		return false
	}
	return syscall.Kill(int(pid), syscall.SIGTERM) == nil
}

type childPoll int

const (
	childNotChild childPoll = iota
	childReaped
	childRunning
)

// reapExitedChild collects pid if it is an exited child of this process.
// WNOHANG does not stop a running child and does not touch a pid we did not
// spawn.
func reapExitedChild(pid int) childPoll {
	var status syscall.WaitStatus
	var rusage syscall.Rusage
	for {
		rc, err := syscall.Wait4(pid, &status, syscall.WNOHANG, &rusage)
		if rc == pid {
			return childReaped
		}
		if rc == 0 {
			return childRunning
		}
		if err == syscall.EINTR {
			continue
		}
		return childNotChild
	}
}

func signalZeroSucceeds(pid int) bool {
	err := syscall.Kill(pid, 0)
	return err == nil || err == syscall.EPERM
}

type zombieStatusT int

const (
	zombieRunning zombieStatusT = iota
	zombieExited
	zombieUnknown
)

// kill(pid,0) already succeeded: distinguish running from zombie.
func zombieStatus(pid int) zombieStatusT {
	return platformZombieStatus(pid)
}

// psZombieStatus runs `ps -o stat= -p pid` with a 1s deadline. Exit status 1
// means the pid is already gone; a spawn failure or hung ps is Unknown so the
// caller keeps the pid alive rather than stealing a lock.
func psZombieStatus(pid int) zombieStatusT {
	return runPsZombieProbe(pid, time.Second)
}
