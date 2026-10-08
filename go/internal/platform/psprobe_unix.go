//go:build unix

package platform

import (
	"bytes"
	"os/exec"
	"strings"
	"syscall"
	"time"
)

// runPsZombieProbe runs `ps -o stat= -p <pid>` in its own process group so a
// timeout cannot signal this process, with a hard deadline.
func runPsZombieProbe(pid int, timeout time.Duration) zombieStatusT {
	cmd := exec.Command("ps", "-o", "stat=", "-p", itoa(pid))
	cmd.Stdin = nil
	cmd.Stderr = nil
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	out, err := cmd.StdoutPipe()
	if err != nil {
		return zombieUnknown
	}
	if err := cmd.Start(); err != nil {
		return zombieUnknown
	}
	done := make(chan struct{})
	var buf bytes.Buffer
	go func() {
		_, _ = buf.ReadFrom(out)
		close(done)
	}()
	waitDone := make(chan error, 1)
	go func() { waitDone <- cmd.Wait() }()
	select {
	case err := <-waitDone:
		<-done
		if err != nil {
			return zombieExited // ps exits 1 when the pid is gone
		}
	case <-time.After(timeout):
		_ = cmd.Process.Kill()
		<-waitDone
		return zombieUnknown
	}
	stat := strings.TrimSpace(buf.String())
	if stat == "" {
		return zombieUnknown
	}
	if strings.HasPrefix(stat, "Z") {
		return zombieExited
	}
	return zombieRunning
}

func itoa(i int) string {
	if i == 0 {
		return "0"
	}
	neg := i < 0
	if neg {
		i = -i
	}
	var b [20]byte
	pos := len(b)
	for i > 0 {
		pos--
		b[pos] = byte('0' + i%10)
		i /= 10
	}
	if neg {
		pos--
		b[pos] = '-'
	}
	return string(b[pos:])
}
