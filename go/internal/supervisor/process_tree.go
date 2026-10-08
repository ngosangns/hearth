// Whole-process-tree signalling logic — pure parsing/graph code. The `ps`
// shell-outs live in adapters.go; the engine acts on the snapshots built here.
package supervisor

import (
	"regexp"
	"strconv"
	"strings"
)

// PsTreeRow is one parsed row of `ps -Ao pid=,ppid=,pgid=,lstart=`.
type PsTreeRow struct {
	Pid  int64
	Ppid int64
	Pgid int64
	// `ps`'s `lstart` field, a fixed 24-character field — the pid-reuse guard
	// compares this exact string, never a parsed timestamp.
	StartIdentity string
}

// ProcessTreeEntry is one member of a snapshotted tree.
type ProcessTreeEntry struct {
	Pid           int64
	Pgid          int64
	StartIdentity string
}

type SnapshotKind int

const (
	// SnapshotUnknown — the table could not be read. NOT an empty tree:
	// callers must not signal and must not treat the process as dead.
	SnapshotUnknown SnapshotKind = iota
	// SnapshotAbsent — the table was read and the leader pid is gone or its
	// start identity no longer matches. Signalling the stored pgid would hit
	// a recycled group.
	SnapshotAbsent
	// SnapshotPresent — safe to signal after a fresh identity check.
	SnapshotPresent
)

type ProcessTreeSnapshot struct {
	Kind    SnapshotKind
	Entries []ProcessTreeEntry
}

func UnknownSnapshot() ProcessTreeSnapshot { return ProcessTreeSnapshot{Kind: SnapshotUnknown} }
func AbsentSnapshot() ProcessTreeSnapshot  { return ProcessTreeSnapshot{Kind: SnapshotAbsent} }
func PresentSnapshot(entries []ProcessTreeEntry) ProcessTreeSnapshot {
	return ProcessTreeSnapshot{Kind: SnapshotPresent, Entries: entries}
}

var treeRowRe = regexp.MustCompile(`^(\d+)\s+(\d+)\s+(\d+)\s+(.{24})`)

// ParsePsTreeRows parses `ps -Ao pid=,ppid=,pgid=,lstart=` output.
func ParsePsTreeRows(stdout string) []PsTreeRow {
	var rows []PsTreeRow
	for _, line := range strings.Split(stdout, "\n") {
		caps := treeRowRe.FindStringSubmatch(strings.TrimLeft(line, " \t"))
		if caps == nil {
			continue
		}
		pid, err1 := strconv.ParseInt(caps[1], 10, 64)
		ppid, err2 := strconv.ParseInt(caps[2], 10, 64)
		pgid, err3 := strconv.ParseInt(caps[3], 10, 64)
		if err1 != nil || err2 != nil || err3 != nil {
			continue
		}
		rows = append(rows, PsTreeRow{
			Pid: pid, Ppid: ppid, Pgid: pgid,
			StartIdentity: strings.TrimSpace(caps[4]),
		})
	}
	return rows
}

// BuildProcessTree snapshots the managed process's whole tree. Returns an
// empty tree (refusing to walk anything) unless the OS table's row for
// leaderPID still shows leaderStartIdentity — a caller-supplied pid that no
// longer matches must never pull an unrelated live tree into a signal.
//
// When the leader leads its own process group (every spawned service is in
// its own group), every other member of that group is part of the tree too,
// even with no ppid path to the leader. A double fork (`(cmd &)`) reparents
// to launchd but leaves the process in the group; without this it is missing
// from the wait-for-death set.
func BuildProcessTree(rows []PsTreeRow, leaderPID int64, leaderStartIdentity string) []ProcessTreeEntry {
	var leader *PsTreeRow
	for i := range rows {
		if rows[i].Pid == leaderPID && rows[i].StartIdentity == leaderStartIdentity {
			leader = &rows[i]
			break
		}
	}
	if leader == nil {
		return nil
	}
	var ownGroup int64
	hasOwnGroup := leader.Pgid == leaderPID && leaderPID > 1
	if hasOwnGroup {
		ownGroup = leaderPID
	}

	byPid := map[int64]*PsTreeRow{}
	byParent := map[int64][]*PsTreeRow{}
	for i := range rows {
		byPid[rows[i].Pid] = &rows[i]
		byParent[rows[i].Ppid] = append(byParent[rows[i].Ppid], &rows[i])
	}

	var tree []ProcessTreeEntry
	seen := map[int64]bool{leaderPID: true}
	queue := []int64{leaderPID}
	groupScanned := false
	for len(queue) > 0 {
		pid := queue[0]
		queue = queue[1:]
		if row, ok := byPid[pid]; ok {
			tree = append(tree, ProcessTreeEntry{
				Pid: row.Pid, Pgid: row.Pgid, StartIdentity: row.StartIdentity,
			})
		}
		for _, child := range byParent[pid] {
			if !seen[child.Pid] {
				seen[child.Pid] = true
				queue = append(queue, child.Pid)
			}
		}
		if len(queue) == 0 && !groupScanned {
			groupScanned = true
			// Group members the ppid walk did not reach, and their descendants.
			if hasOwnGroup {
				for i := range rows {
					if rows[i].Pgid == ownGroup && !seen[rows[i].Pid] {
						seen[rows[i].Pid] = true
						queue = append(queue, rows[i].Pid)
					}
				}
			}
		}
	}
	return tree
}

// MergeKnownDescendants is the sampler's running view of a service's tree. A
// member of `previous` that is no longer in `fresh` stays only while `alive`
// still shows it with the same start identity. `alive` of nil (the table could
// not be read) keeps those members for the next sample.
func MergeKnownDescendants(previous []ProcessTreeEntry, fresh []ProcessTreeEntry, alive map[int64]string) []ProcessTreeEntry {
	merged := append([]ProcessTreeEntry{}, fresh...)
	present := map[[2]any]bool{}
	for _, e := range merged {
		present[[2]any{e.Pid, e.StartIdentity}] = true
	}
	for _, entry := range previous {
		if present[[2]any{entry.Pid, entry.StartIdentity}] {
			continue
		}
		stillAlive := true
		if alive != nil {
			stillAlive = alive[entry.Pid] == entry.StartIdentity
		}
		if stillAlive {
			merged = append(merged, entry)
		}
	}
	return merged
}

// HasDepartedMembers reports whether previous has members fresh does not — the
// only case the sampler needs a second `ps` (liveness) for.
func HasDepartedMembers(previous, fresh []ProcessTreeEntry) bool {
	for _, entry := range previous {
		found := false
		for _, f := range fresh {
			if f.Pid == entry.Pid && f.StartIdentity == entry.StartIdentity {
				found = true
				break
			}
		}
		if !found {
			return true
		}
	}
	return false
}

var aliveRowRe = regexp.MustCompile(`^(\d+)\s+(.{24})`)

// ParsePsAliveRows parses `ps -Ao pid=,lstart=` into pid -> lstart.
func ParsePsAliveRows(stdout string) map[int64]string {
	out := map[int64]string{}
	for _, line := range strings.Split(stdout, "\n") {
		caps := aliveRowRe.FindStringSubmatch(strings.TrimLeft(line, " \t"))
		if caps == nil {
			continue
		}
		pid, err := strconv.ParseInt(caps[1], 10, 64)
		if err != nil {
			continue
		}
		out[pid] = strings.TrimSpace(caps[2])
	}
	return out
}

// ProcessTreeAlive reports a pid as still-ours only when its start time
// matches the snapshot, so a recycled pid can never keep a stop waiting (or,
// worse, earn a SIGKILL).
func ProcessTreeAlive(tree []ProcessTreeEntry, alive map[int64]string) bool {
	if len(tree) == 0 {
		return false
	}
	for _, entry := range tree {
		if alive[entry.Pid] == entry.StartIdentity {
			return true
		}
	}
	return false
}

// SecondaryProcessGroups groups tree members by pgid, excluding the leader's
// own pgid and any pgid <= 1.
func SecondaryProcessGroups(tree []ProcessTreeEntry, leaderPgid int64) map[int64][]ProcessTreeEntry {
	groups := map[int64][]ProcessTreeEntry{}
	for _, entry := range tree {
		if entry.Pgid == leaderPgid || entry.Pgid <= 1 {
			continue
		}
		groups[entry.Pgid] = append(groups[entry.Pgid], entry)
	}
	return groups
}
