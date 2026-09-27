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

/// A recipe may ask for a primary port plus a few extras (Kafka controller, MinIO console).
/// The cap keeps a typo from scanning the whole range for a huge contiguous block.
pub const MAX_SHARED_PORTS: u16 = 4;

/// Single-port allocation. Equivalent to `allocate_ports(id, 1, taken)` and returning the only port.
pub async fn allocate_port(instance_id: &str, taken: &HashSet<u16>) -> Option<u16> {
    allocate_ports(instance_id, 1, taken)
        .await
        .map(|ports| ports[0])
}

/// `count` contiguous free ports, starting at-or-after the deterministic base and probing forward.
/// A candidate that would run past the end of the shared range is skipped — the block never wraps
/// back to `SHARED_PORT_RANGE_START` in the middle. Binds every port in the block before accepting
/// it, so a live listener on the second port rejects the whole block.
pub async fn allocate_ports(
    instance_id: &str,
    count: u16,
    taken: &HashSet<u16>,
) -> Option<Vec<u16>> {
    if !(1..=MAX_SHARED_PORTS).contains(&count) {
        return None;
    }
    let base = derive_base_port(instance_id);
    let range_end = SHARED_PORT_RANGE_START + SHARED_PORT_RANGE_SIZE;
    for offset in 0..SHARED_PORT_RANGE_SIZE {
        let start = SHARED_PORT_RANGE_START
            + (base - SHARED_PORT_RANGE_START + offset) % SHARED_PORT_RANGE_SIZE;
        let last = u32::from(start) + u32::from(count) - 1;
        if last >= u32::from(range_end) {
            continue;
        }
        let ports: Vec<u16> = (0..count).map(|i| start + i).collect();
        if ports.iter().any(|port| taken.contains(port)) {
            continue;
        }
        if bind_all(&ports).await {
            return Some(ports);
        }
    }
    None
}

/// Bind-check only — for recipes with pinned ports (outside the shared range) there is nothing
/// to probe, just the question of whether the requested set is free.
pub async fn ports_free(ports: &[u16]) -> bool {
    bind_all(ports).await
}

/// Holds listeners for the whole block until every bind has succeeded, then drops them together.
async fn bind_all(ports: &[u16]) -> bool {
    let mut listeners = Vec::with_capacity(ports.len());
    for port in ports {
        match tokio::net::TcpListener::bind(("127.0.0.1", *port)).await {
            Ok(listener) => listeners.push(listener),
            Err(_) => return false,
        }
    }
    drop(listeners);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_port_is_deterministic_and_in_range() {
        let a = derive_base_port("postgres@16.4");
        let b = derive_base_port("postgres@16.4");
        assert_eq!(a, b);
        assert!(
            (SHARED_PORT_RANGE_START..SHARED_PORT_RANGE_START + SHARED_PORT_RANGE_SIZE)
                .contains(&a)
        );
        assert_ne!(
            derive_base_port("postgres@16.4"),
            derive_base_port("postgres@16.5")
        );
    }

    #[tokio::test]
    async fn skips_registry_taken_and_live_ports() {
        let base = derive_base_port("redis@7.2");
        // Occupy the base slot with a real listener; the allocator must move past it.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", base))
            .await
            .unwrap();
        let port = allocate_port("redis@7.2", &HashSet::new()).await.unwrap();
        assert_ne!(port, base);
        drop(listener);
        // A registry-taken base slot is skipped without any listener involved.
        let port = allocate_port("redis@7.2", &HashSet::from([base]))
            .await
            .unwrap();
        assert_ne!(port, base);
    }

    #[tokio::test]
    async fn allocates_a_contiguous_block_and_skips_a_broken_pair() {
        let id = "kafka@4.1.0";
        let base = derive_base_port(id);
        let range_end = SHARED_PORT_RANGE_START + SHARED_PORT_RANGE_SIZE;
        let ports = allocate_ports(id, 2, &HashSet::new()).await.unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[1], ports[0] + 1);
        assert!(ports[1] < range_end);

        if base + 1 < range_end {
            let _listener = tokio::net::TcpListener::bind(("127.0.0.1", base + 1))
                .await
                .unwrap();
            let shifted = allocate_ports(id, 2, &HashSet::new()).await.unwrap();
            assert_ne!(
                shifted[0], base,
                "a live listener on the second port must reject the whole block"
            );
            assert!(!shifted.contains(&(base + 1)));
        }

        let taken = HashSet::from([ports[0]]);
        let other = allocate_ports(id, 2, &taken).await.unwrap();
        assert!(!other.contains(&ports[0]));
        assert_eq!(other[1], other[0] + 1);
    }

    #[tokio::test]
    async fn rejects_a_block_wider_than_the_cap() {
        assert!(allocate_ports("redis@7.2", 0, &HashSet::new())
            .await
            .is_none());
        assert!(
            allocate_ports("redis@7.2", MAX_SHARED_PORTS + 1, &HashSet::new())
                .await
                .is_none()
        );
    }
}
