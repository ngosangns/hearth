//go:build linux

package platform

import (
	"errors"
	"io/fs"
	"os"
	"strconv"
	"strings"
)

// platformZombieStatus reads /proc/<pid>/stat; state 'Z' is a zombie. Falls
// back to ps when the stat can't be parsed, and a vanished /proc entry is a
// confirmed-dead pid.
func platformZombieStatus(pid int) zombieStatusT {
	stat, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/stat")
	if errors.Is(err, fs.ErrNotExist) {
		return zombieExited
	}
	if err != nil {
		return psZombieStatus(pid)
	}
	if state, ok := procStatState(string(stat)); ok {
		if state == 'Z' {
			return zombieExited
		}
		return zombieRunning
	}
	return psZombieStatus(pid)
}

// procStatState returns the state character after the comm field. comm is
// wrapped in parentheses and may itself contain spaces and parentheses, so the
// state is the token after the last ')'.
func procStatState(stat string) (byte, bool) {
	end := strings.LastIndexByte(stat, ')')
	if end < 0 || end+1 >= len(stat) {
		return 0, false
	}
	fields := strings.Fields(stat[end+1:])
	if len(fields) == 0 || len(fields[0]) == 0 {
		return 0, false
	}
	return fields[0][0], true
}
