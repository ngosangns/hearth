//go:build darwin

package platform

import (
	"syscall"
	"unsafe"
)

// libc's proc_pidinfo() is a thin wrapper over the proc_info(2) syscall with
// callnum PROC_INFO_CALL_PIDINFO — stable Darwin ABI.
const (
	sysProcInfo         = 0x150 // SYS_PROC_INFO
	procInfoCallPidinfo = 0x2   // PROC_INFO_CALL_PIDINFO
	procPidTBsdInfo     = 3     // PROC_PIDTBSDINFO
	procBsdInfoSize     = 648   // sizeof(struct proc_bsdinfo)
)

// platformZombieStatus mirrors the Rust libc::proc_pidinfo probe: fills the
// buffer for a live process and returns 0 for a zombie (or a pid that
// disappeared between kill and this call). A 0 is not proof on its own —
// confirm with ps so a transient probe failure cannot mark a live manager dead.
func platformZombieStatus(pid int) zombieStatusT {
	buf := make([]byte, procBsdInfoSize)
	ret, _, errno := syscall.RawSyscall6(
		sysProcInfo,
		procInfoCallPidinfo,
		uintptr(pid),
		procPidTBsdInfo,
		0,
		uintptr(unsafe.Pointer(&buf[0])),
		procBsdInfoSize,
	)
	if errno != 0 || ret <= 0 {
		return psZombieStatus(pid)
	}
	return zombieRunning
}
