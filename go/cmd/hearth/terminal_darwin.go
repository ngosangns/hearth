//go:build darwin

package main

import (
	"os"
	"syscall"
	"unsafe"
)

// isTerminal reports whether f is a terminal. ModeCharDevice is not enough:
// /dev/null is a character device, and a GUI or `</dev/null` must stay
// non-interactive.
func isTerminal(f *os.File) bool {
	if f == nil {
		return false
	}
	var termios syscall.Termios
	_, _, errno := syscall.Syscall(
		syscall.SYS_IOCTL,
		f.Fd(),
		uintptr(syscall.TIOCGETA),
		uintptr(unsafe.Pointer(&termios)),
	)
	return errno == 0
}
