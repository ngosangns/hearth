//go:build windows

package platform

import (
	"os"
	"os/exec"
	"syscall"
)

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

func Executable() string {
	exe, err := os.Executable()
	if err != nil {
		return "hearth"
	}
	return exe
}
