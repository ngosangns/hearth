//! Whole-process-tree signalling — port of `ProcessSupervisor.processTree`/`processTreeAlive`/
//! `signalProcessTree` from `src/core/supervisor.ts`.
//!
//! **Sharp edge (AGENTS.md)**: stopping a service must signal its *whole process tree*, snapshotted
//! from `ps` *before* the first signal: a wrapper (e.g. `air`) can run the real long-lived server in
//! its **own** process group, so signalling only the tracked pgid leaves that server alive holding
//! its port. The snapshot is only walked when the OS table still shows the recorded `start_identity`
//! for the leader pid, so a stale/reused pid can never pull an unrelated live tree into a signal or
//! into the wait-for-death loop. This module is pure parsing/graph logic — no process spawning or
//! signalling happens here; see `default_adapters.rs` for the `ps`/`kill` shell-outs that feed it.
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::OnceLock;

use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsTreeRow {
    pub pid: i64,
    pub ppid: i64,
    pub pgid: i64,
    /// `ps`'s `lstart` field, a fixed 24-character field — the pid-reuse guard compares this exact
    /// string, never a parsed timestamp.
    pub start_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTreeEntry {
    pub pid: i64,
    pub pgid: i64,
    pub start_identity: String,
}

fn tree_row_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d+)\s+(\d+)\s+(\d+)\s+(.{24})").unwrap())
}

/// Parses the output of `ps -Ao pid=,ppid=,pgid=,lstart=`.
pub fn parse_ps_tree_rows(stdout: &str) -> Vec<PsTreeRow> {
    stdout
        .split('\n')
        .filter_map(|line| {
            let trimmed = line.trim_start();
            tree_row_re().captures(trimmed).map(|c| PsTreeRow {
                pid: c[1].parse().unwrap(),
                ppid: c[2].parse().unwrap(),
                pgid: c[3].parse().unwrap(),
                start_identity: c[4].trim().to_string(),
            })
        })
        .collect()
}

/// Snapshot of the managed process's whole tree, taken while the caller still owns the leader.
/// Returns an empty tree (refusing to walk anything) unless the OS table's row for `leader_pid`
/// still shows `leader_start_identity` — a caller-supplied pid that no longer matches must never
/// pull an unrelated live tree (pid reuse) into a signal or into the wait-for-death loop.
pub fn build_process_tree(rows: &[PsTreeRow], leader_pid: i64, leader_start_identity: &str) -> Vec<ProcessTreeEntry> {
    let leader_matches = rows
        .iter()
        .find(|row| row.pid == leader_pid)
        .map(|row| row.start_identity == leader_start_identity)
        .unwrap_or(false);
    if !leader_matches {
        return Vec::new();
    }

    let by_pid: HashMap<i64, &PsTreeRow> = rows.iter().map(|r| (r.pid, r)).collect();
    let mut by_parent: HashMap<i64, Vec<&PsTreeRow>> = HashMap::new();
    for row in rows {
        by_parent.entry(row.ppid).or_default().push(row);
    }

    let mut tree = Vec::new();
    let mut seen: HashSet<i64> = HashSet::from([leader_pid]);
    let mut queue: VecDeque<i64> = VecDeque::from([leader_pid]);
    while let Some(pid) = queue.pop_front() {
        if let Some(row) = by_pid.get(&pid) {
            tree.push(ProcessTreeEntry { pid: row.pid, pgid: row.pgid, start_identity: row.start_identity.clone() });
        }
        for child in by_parent.get(&pid).into_iter().flatten() {
            if seen.insert(child.pid) {
                queue.push_back(child.pid);
            }
        }
    }
    tree
}

fn alive_row_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d+)\s+(.{24})").unwrap())
}

/// Parses the output of `ps -Ao pid=,lstart=` into `pid -> lstart`.
pub fn parse_ps_alive_rows(stdout: &str) -> HashMap<i64, String> {
    stdout
        .split('\n')
        .filter_map(|line| {
            let trimmed = line.trim_start();
            alive_row_re().captures(trimmed).map(|c| (c[1].parse().unwrap(), c[2].trim().to_string()))
        })
        .collect()
}

/// A pid is only counted as still-ours when its start time matches the snapshot, so a recycled pid
/// can never keep a stop waiting (or, worse, earn a SIGKILL).
pub fn process_tree_alive(tree: &[ProcessTreeEntry], alive: &HashMap<i64, String>) -> bool {
    if tree.is_empty() {
        return false;
    }
    tree.iter().any(|entry| alive.get(&entry.pid) == Some(&entry.start_identity))
}

/// Groups tree members by pgid, excluding the leader's own pgid and any pgid `<= 1` — the leader
/// itself is signalled separately by the caller; this is only the "other pgids in the tree" set that
/// `signal_process_tree` needs to separately re-verify before signalling (the `air`-forks-into-its-
/// own-pgid case).
pub fn secondary_process_groups(tree: &[ProcessTreeEntry], leader_pgid: i64) -> HashMap<i64, Vec<ProcessTreeEntry>> {
    let mut groups: HashMap<i64, Vec<ProcessTreeEntry>> = HashMap::new();
    for entry in tree {
        if entry.pgid == leader_pgid || entry.pgid <= 1 {
            continue;
        }
        groups.entry(entry.pgid).or_default().push(entry.clone());
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    const LSTART_A: &str = "Sun Sep 20 10:00:00 2026";
    const LSTART_B: &str = "Sun Sep 20 10:00:01 2026";

    fn row(pid: i64, ppid: i64, pgid: i64, start: &str) -> PsTreeRow {
        PsTreeRow { pid, ppid, pgid, start_identity: start.to_string() }
    }

    #[test]
    fn parses_ps_tree_rows() {
        let stdout = format!("  100     1   100 {LSTART_A}\n  101   100   101 {LSTART_B}\n");
        let rows = parse_ps_tree_rows(&stdout);
        assert_eq!(rows, vec![row(100, 1, 100, LSTART_A), row(101, 100, 101, LSTART_B)]);
    }

    #[test]
    fn builds_a_tree_that_crosses_a_process_group_boundary() {
        // The `air`-forks-its-server-into-its-own-pgid scenario: leader pid=100/pgid=100, child
        // pid=101 reparented into its own pgid=101.
        let rows = vec![row(100, 1, 100, LSTART_A), row(101, 100, 101, LSTART_B)];
        let tree = build_process_tree(&rows, 100, LSTART_A);
        assert_eq!(
            tree,
            vec![
                ProcessTreeEntry { pid: 100, pgid: 100, start_identity: LSTART_A.to_string() },
                ProcessTreeEntry { pid: 101, pgid: 101, start_identity: LSTART_B.to_string() },
            ]
        );
    }

    #[test]
    fn refuses_to_walk_a_tree_when_the_leader_start_identity_no_longer_matches() {
        // Simulates pid reuse: the OS table's row for pid 100 now belongs to a different process.
        let rows = vec![row(100, 1, 100, "Sun Sep 20 11:00:00 2026")];
        let tree = build_process_tree(&rows, 100, LSTART_A);
        assert!(tree.is_empty());
    }

    #[test]
    fn refuses_to_walk_a_tree_when_the_leader_pid_is_gone() {
        let rows = vec![row(200, 1, 200, LSTART_A)];
        let tree = build_process_tree(&rows, 100, LSTART_A);
        assert!(tree.is_empty());
    }

    #[test]
    fn deduplicates_a_diamond_shaped_process_tree() {
        // Not a realistic process tree (pids don't fork-join like this) but exercises the `seen` set
        // guard against infinite loops / duplicate entries regardless.
        let rows = vec![row(100, 1, 100, LSTART_A), row(101, 100, 100, LSTART_A), row(102, 100, 100, LSTART_A)];
        let tree = build_process_tree(&rows, 100, LSTART_A);
        assert_eq!(tree.len(), 3);
    }

    #[test]
    fn process_tree_alive_true_when_any_member_still_matches() {
        let tree = vec![ProcessTreeEntry { pid: 100, pgid: 100, start_identity: LSTART_A.to_string() }];
        let mut alive = HashMap::new();
        alive.insert(100, LSTART_A.to_string());
        assert!(process_tree_alive(&tree, &alive));
    }

    #[test]
    fn process_tree_alive_false_when_pid_was_reused() {
        let tree = vec![ProcessTreeEntry { pid: 100, pgid: 100, start_identity: LSTART_A.to_string() }];
        let mut alive = HashMap::new();
        alive.insert(100, "a different lstart value...".to_string());
        assert!(!process_tree_alive(&tree, &alive));
    }

    #[test]
    fn process_tree_alive_false_for_an_empty_tree() {
        assert!(!process_tree_alive(&[], &HashMap::new()));
    }

    #[test]
    fn secondary_process_groups_excludes_the_leader_pgid_and_pgid_zero_or_one() {
        let tree = vec![
            ProcessTreeEntry { pid: 100, pgid: 100, start_identity: LSTART_A.to_string() },
            ProcessTreeEntry { pid: 101, pgid: 101, start_identity: LSTART_B.to_string() },
            ProcessTreeEntry { pid: 102, pgid: 1, start_identity: LSTART_B.to_string() },
        ];
        let groups = secondary_process_groups(&tree, 100);
        assert_eq!(groups.len(), 1);
        assert!(groups.contains_key(&101));
    }
}

// Real OS-level regression test — port of `test/core/terminate-tree-regression.test.ts`. Spawns a
// shell that backgrounds a job into its own process group (mirroring `air` handing its built server
// its own pgid), then proves the tree-walk-and-signal logic actually kills it. This is the live
// oracle for the whole-process-tree sharp edge; it exercises real `ps`/real signals, not a stub.
#[cfg(all(test, unix))]
mod os_level_regression {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    fn run_ps_tree_snapshot() -> String {
        String::from_utf8(Command::new("ps").args(["-Ao", "pid=,ppid=,pgid=,lstart="]).output().unwrap().stdout).unwrap()
    }

    fn run_ps_alive_snapshot() -> String {
        String::from_utf8(Command::new("ps").args(["-Ao", "pid=,lstart="]).output().unwrap().stdout).unwrap()
    }

    fn is_process_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn stop_kills_a_child_that_forked_into_its_own_process_group() {
        // `process_group(0)` (setpgid(0,0)) puts the spawned shell into its OWN new process group,
        // exactly like the real production adapter's `detached: true` spawn does for a managed
        // service — critical for this test's safety, not just fidelity: without it, the shell
        // inherits *this test process's own* pgid, and the killpg calls below would signal the whole
        // cargo-test process group instead of just the subtree under test.
        // `set -m` turns on job control so `sleep 60 &` gets its own process group distinct from the
        // shell's — exactly the shape `air` produces for its managed server child.
        let mut child = Command::new("sh").args(["-c", "set -m; sleep 60 & wait"]).process_group(0).spawn().unwrap();
        let leader_pid = child.id() as i64;

        // Wait for the backgrounded `sleep` to actually land in its own process group before
        // snapshotting — a race against the shell's own job-control setup.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut child_pid: Option<i64> = None;
        while Instant::now() < deadline {
            let rows = parse_ps_tree_rows(&run_ps_tree_snapshot());
            if let Some(sleep_row) = rows.iter().find(|r| r.ppid == leader_pid && r.pgid != leader_pid) {
                child_pid = Some(sleep_row.pid);
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let child_pid = child_pid.expect("backgrounded sleep never landed in its own process group");

        let leader_rows = parse_ps_tree_rows(&run_ps_tree_snapshot());
        let leader_start_identity = leader_rows.iter().find(|r| r.pid == leader_pid).unwrap().start_identity.clone();
        let tree = build_process_tree(&leader_rows, leader_pid, &leader_start_identity);
        assert!(tree.iter().any(|e| e.pid == child_pid), "tree walk should have found the backgrounded child");

        let leader_pgid = leader_rows.iter().find(|r| r.pid == leader_pid).unwrap().pgid;

        // Signal the leader's own pgid (as `signal_process_tree` does)...
        unsafe {
            libc::killpg(leader_pgid as libc::pid_t, libc::SIGTERM);
        }
        // ...then separately re-verify and signal any other pgid in the tree — the actual behavior
        // under test: without this second step, the backgrounded `sleep` (a different pgid) survives.
        for (pgid, members) in secondary_process_groups(&tree, leader_pgid) {
            let alive = run_ps_alive_snapshot();
            let alive_rows = parse_ps_alive_rows(&alive);
            let still_ours = members.iter().any(|m| alive_rows.get(&m.pid) == Some(&m.start_identity));
            if still_ours {
                unsafe {
                    libc::killpg(pgid as libc::pid_t, libc::SIGTERM);
                }
            }
        }

        let _ = child.wait();
        thread::sleep(Duration::from_millis(200));
        assert!(!is_process_alive(child_pid as i32), "the backgrounded child (own process group) must not survive");
    }
}
