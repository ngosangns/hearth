//go:build unix && !darwin && !linux

package platform

func platformZombieStatus(pid int) zombieStatusT {
	return psZombieStatus(pid)
}
