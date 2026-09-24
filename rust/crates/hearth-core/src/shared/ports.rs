//! Deterministic port allocation for shared instances. `hash(name@version)` picks a base slot in
//! the shared range; collisions (another registry entry, or a live foreign listener) probe forward
//! from there. The winner is persisted in `registry.json`, so the resolved port is stable across
//! daemon restarts even when it had to move.
use std::collections::HashSet;

use sha2::Digest;

use super::{SHARED_PORT_RANGE_SIZE, SHARED_PORT_RANGE_START};

/// The deterministic base slot — same input, same port, no state needed.
pub fn derive_base_port(instance_id: &str) -> u16 {
    let digest = sha2::Sha256::digest(instance_id.as_bytes());
    let n = u64::from_be_bytes(digest[0..8].try_into().unwrap());
    SHARED_PORT_RANGE_START + (n % u64::from(SHARED_PORT_RANGE_SIZE)) as u16
}

/// First candidate at-or-after the base slot (wrapping inside the range) that is neither claimed
/// by another registry entry nor bound by a live process. Binds a probe listener rather than
/// connecting — a successful bind proves the port is actually free, which a refused connect
/// cannot distinguish from a filtered one.
pub async fn allocate_port(instance_id: &str, taken: &HashSet<u16>) -> Option<u16> {
    let base = derive_base_port(instance_id);
    for offset in 0..SHARED_PORT_RANGE_SIZE {
        let port = SHARED_PORT_RANGE_START + (base - SHARED_PORT_RANGE_START + offset) % SHARED_PORT_RANGE_SIZE;
        if taken.contains(&port) {
            continue;
        }
        if tokio::net::TcpListener::bind(("127.0.0.1", port)).await.is_ok() {
            return Some(port);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_port_is_deterministic_and_in_range() {
        let a = derive_base_port("postgres@16.4");
        let b = derive_base_port("postgres@16.4");
        assert_eq!(a, b);
        assert!((SHARED_PORT_RANGE_START..SHARED_PORT_RANGE_START + SHARED_PORT_RANGE_SIZE).contains(&a));
        assert_ne!(derive_base_port("postgres@16.4"), derive_base_port("postgres@16.5"));
    }

    #[tokio::test]
    async fn skips_registry_taken_and_live_ports() {
        let base = derive_base_port("redis@7.2");
        // Occupy the base slot with a real listener; the allocator must move past it.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", base)).await.unwrap();
        let port = allocate_port("redis@7.2", &HashSet::new()).await.unwrap();
        assert_ne!(port, base);
        drop(listener);
        // A registry-taken base slot is skipped without any listener involved.
        let port = allocate_port("redis@7.2", &HashSet::from([base])).await.unwrap();
        assert_ne!(port, base);
    }
}
