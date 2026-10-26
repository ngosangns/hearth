//go:build !windows

package platform

import (
	"os"
	"os/exec"
	"syscall"
)

// SpawnDetached starts exe with argv in a process that outlives the caller.
// A short-lived intermediate (_spawn1 in cmd/hearth) sits between this process
// and the long-lived target so a GUI-spawned hearth is not torn down when the
// app quits (macOS process coalitions).
func SpawnDetached(exe string, argv []string, cwd string) error {
	command := exec.Command(exe, argv...)
	command.Stdin = nil
	command.Stdout = nil
	command.Stderr = nil
	if cwd != "" {
		command.Dir = cwd
	}
	command.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := command.Start(); err != nil {
		return err
	}
	go func() { _ = command.Wait() }()
	return nil
}

// Executable returns the hearth binary path for re-invocation.
func Executable() string {
	exe, err := os.Executable()
	if err != nil {
		return "hearth"
	}
	return exe
}
