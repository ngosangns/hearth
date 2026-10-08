// Deterministic port allocation for shared instances. hash(name@version)
// picks a base slot in the shared range; collisions (another registry entry,
// or a live foreign listener) probe forward from there. The winner is
// persisted in registry.json, so the resolved port is stable across daemon
// restarts even when it had to move.
package shared

import (
	"crypto/sha256"
	"encoding/binary"
	"net"
)

// DeriveBasePort is the deterministic base slot — same input, same port, no
// state needed.
func DeriveBasePort(instanceID string) uint16 {
	digest := sha256.Sum256([]byte(instanceID))
	n := binary.BigEndian.Uint64(digest[0:8])
	return SharedPortRangeStart + uint16(n%uint64(SharedPortRangeSize))
}

// AllocatePorts finds `count` contiguous free ports, starting at-or-after the
// deterministic base and probing forward. A candidate that would run past the
// end of the shared range is skipped — the block never wraps back to
// SharedPortRangeStart in the middle. Every port in the block is bound before
// accepting it, so a live listener on the second port rejects the whole block.
func AllocatePorts(instanceID string, count uint16, taken map[uint16]bool) []uint16 {
	if count < 1 || count > MaxSharedPorts {
		return nil
	}
	base := DeriveBasePort(instanceID)
	rangeEnd := SharedPortRangeStart + SharedPortRangeSize
	for offset := uint16(0); offset < SharedPortRangeSize; offset++ {
		start := SharedPortRangeStart + (base-SharedPortRangeStart+offset)%SharedPortRangeSize
		last := uint32(start) + uint32(count) - 1
		if last >= uint32(rangeEnd) {
			continue
		}
		ports := make([]uint16, count)
		for i := range ports {
			ports[i] = start + uint16(i)
		}
		conflict := false
		for _, p := range ports {
			if taken[p] {
				conflict = true
				break
			}
		}
		if conflict {
			continue
		}
		if BindAll(ports) {
			return ports
		}
	}
	return nil
}

// BindAll holds listeners for the whole block until every bind has succeeded,
// then drops them together. Also the whole check for recipes with pinned
// ports (outside the shared range) — there is nothing to probe, just the
// question of whether the requested set is free.
func BindAll(ports []uint16) bool {
	listeners := make([]net.Listener, 0, len(ports))
	for _, port := range ports {
		l, err := net.Listen("tcp", (&net.TCPAddr{IP: net.ParseIP("127.0.0.1"), Port: int(port)}).String())
		if err != nil {
			for _, l := range listeners {
				l.Close()
			}
			return false
		}
		listeners = append(listeners, l)
	}
	for _, l := range listeners {
		l.Close()
	}
	return true
}
