// Production adapters: the real `ps`/`lsof`/`docker`/`tailscale` shell-outs
// and `os/exec`-based spawning that make a Supervisor actually run real
// processes and containers, as opposed to the fakes the engine's own unit
// tests use. Every `ps` call the supervisor makes lives here.
//
// Known simplification (same as the Rust original): forwardStream decodes each
// read chunk with strings.ToValidUTF8 independently rather than with a
// stateful streaming UTF-8 decoder — a multi-byte UTF-8 character split
// exactly across a pipe-read boundary can render as a replacement character in
// forwarded log output. Cosmetic only (log text, not a wire protocol).
package supervisor

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unicode/utf8"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/paths"
	"github.com/ngosangns/hearth/go/internal/shared"
	"github.com/ngosangns/hearth/go/internal/state"
)

const (
	rawLogPollMS         int64 = 200
	rawLogTruncateBytes        = 8 * 1024 * 1024
	rawLogReadChunk            = 64 * 1024
	execIdentitySettleMS       = 1000
	// probeCommandTimeout bounds every observational shell-out (`ps`, `lsof`,
	// `docker inspect`, `tailscale`). These run under the per-service lock, so
	// a wedged Docker Desktop or tailscaled must not block a service's stop —
	// or, via the sequential external-sync loop, every external service's
	// polling.
	probeCommandTimeout = 5 * time.Second
	// containerPollInterval is one batched `docker inspect` for every started
	// container per tick — see ContainerWatcher.
	containerPollInterval = 2 * time.Second
	// helperOutputDrain is how long a helper's output may stay open after its
	// leader exits before what holds it is killed.
	helperOutputDrain = time.Second
)

func toSignal(s ProcessSignal) syscall.Signal {
	if s == SignalKill {
		return syscall.SIGKILL
	}
	return syscall.SIGTERM
}

func sendSignalPID(pid int64, signal ProcessSignal) {
	if pid <= 1 {
		return
	}
	_ = syscall.Kill(int(pid), toSignal(signal))
}

// A negative pid is POSIX shorthand for "the whole process group". pgid 0 is
// the caller's own group and kill(-1, …) signals every process the user owns,
// so both are refused outright.
func sendSignalToGroup(pgid int64, signal ProcessSignal) {
	if pgid <= 1 {
		return
	}
	_ = syscall.Kill(-int(pgid), toSignal(signal))
}

// groupGuard SIGKILLs a helper's process group unless disarmed — the
// synchronous equivalent of KillGroupOnDrop. Callers defer/call Fire at every
// failure path and Disarm once the leader is reaped and its pipes are closed.
type groupGuard struct{ pgid int64 }

func (g *groupGuard) Fire() {
	if g.pgid > 1 {
		sendSignalToGroup(g.pgid, SignalKill)
	}
	g.pgid = 0
}

func (g *groupGuard) Disarm() { g.pgid = 0 }

// captureCommand runs an observational command and returns (exit code,
// stdout); -1 means it could not be spawned or did not finish within
// probeCommandTimeout (its process group is then killed).
func captureCommand(argv []string) (int32, string) {
	code, stdout, _ := captureCommandOutput(argv)
	return code, stdout
}

// captureCommandOutput is captureCommand plus stderr. Docker inspect writes
// "Cannot connect to the Docker daemon" and "No such container" there;
// dropping stderr makes both look like an empty answer.
func captureCommandOutput(argv []string) (int32, string, string) {
	if len(argv) == 0 {
		return -1, "", ""
	}
	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Stdin = nil
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	var outBuf, errBuf bytes.Buffer
	cmd.Stdout = &outBuf
	cmd.Stderr = &errBuf
	if err := cmd.Start(); err != nil {
		return -1, "", ""
	}
	// Dropped with the caller (a stop or sync racing its own deadline): the
	// probe's group goes too. Disarmed once both pipes reached EOF and the
	// leader is reaped.
	guard := &groupGuard{pgid: int64(cmd.Process.Pid)}
	waitCh := make(chan error, 1)
	go func() { waitCh <- cmd.Wait() }()
	timer := time.NewTimer(probeCommandTimeout)
	defer timer.Stop()
	select {
	case werr := <-waitCh:
		// Both pipes reached EOF and the leader is reaped: nothing of the
		// group holds them.
		guard.Disarm()
		code := int32(-1)
		if werr == nil {
			code = 0
		} else if ee, ok := werr.(*exec.ExitError); ok {
			code = int32(ee.ExitCode())
		} else {
			return -1, "", ""
		}
		return code, strings.ToValidUTF8(outBuf.String(), ""), strings.ToValidUTF8(errBuf.String(), "")
	case <-timer.C:
		// Either the leader is still running, or it was reaped and something
		// it started still holds a pipe — either way the group is still
		// populated. SIGKILL it; reap the leader if it is still ours.
		guard.Fire()
		_ = cmd.Process.Kill()
		<-waitCh
		return -1, "", ""
	}
}

// dockerInspectAnswered: `docker inspect` exit 1 is two different answers.
// "No such container" means the container is gone. Anything else (daemon
// socket down, empty stderr, a timeout) means the probe could not be asked —
// callers must treat that as unknown, never as gone.
func dockerInspectAnswered(code int32, stderr string) bool {
	if code == 0 {
		return true
	}
	if code == -1 {
		return false
	}
	lower := strings.ToLower(stderr)
	return strings.Contains(lower, "no such object") || strings.Contains(lower, "no such container")
}

var psObservedRe = regexp.MustCompile(`^(\d+)\s+(\d+)\s+(.{24})\s+(.+)$`)

type posixObservation struct {
	record      PosixProcessRecord
	commandLine string
}

// observedSystemProcessWithCommand returns pid's live record plus its raw
// `command=` text — the fingerprint is a hash, so anything shown to a person
// (the port-holder kill prompt) needs the text itself. A non-nil error means
// the probe itself failed (spawn/timeout — exit -1, or unparseable output):
// the caller must not read it as "process gone".
func observedSystemProcessWithCommand(pid int64) (*posixObservation, error) {
	pidStr := strconv.FormatInt(pid, 10)
	code, stdout := captureCommand([]string{
		"ps", "-o", "pid=", "-o", "pgid=", "-o", "lstart=", "-o", "command=", "-p", pidStr,
	})
	if code == -1 {
		return nil, errProbeFailed
	}
	if code != 0 {
		return nil, nil
	}
	caps := psObservedRe.FindStringSubmatch(strings.TrimSpace(stdout))
	if caps == nil {
		return nil, errProbeFailed
	}
	observedPid, err1 := strconv.ParseInt(caps[1], 10, 64)
	pgid, err2 := strconv.ParseInt(caps[2], 10, 64)
	if err1 != nil || err2 != nil {
		return nil, errProbeFailed
	}
	startIdentity := strings.TrimSpace(caps[3])
	commandLine := strings.TrimSpace(caps[4])
	return &posixObservation{
		record: PosixProcessRecord{
			Pid:                observedPid,
			Pgid:               pgid,
			StartIdentity:      startIdentity,
			CommandFingerprint: NormalizeObservedCommandFingerprint(caps[4]),
			CommandLine:        commandLine,
		},
		commandLine: commandLine,
	}, nil
}

var errProbeFailed = NewError("probe failed")

func observedSystemProcess(pid int64) (*ObservedProcess, error) {
	obs, err := observedSystemProcessWithCommand(pid)
	if err != nil || obs == nil {
		return nil, err
	}
	return &ObservedProcess{Record: ProcessRecord{Posix: &obs.record}, Alive: true}, nil
}

// processInspectionAvailable reports whether `ps` can inspect processes at
// all. Only a success is cached: `ps` does not disappear, but a one-off
// failure (a timeout under load) must not refuse every later spawn.
var psAvailable atomic.Bool

func processInspectionAvailable() bool {
	if psAvailable.Load() {
		return true
	}
	pid := strconv.Itoa(os.Getpid())
	code, _ := captureCommand([]string{"ps", "-o", "pid=", "-p", pid})
	available := code == 0
	if available {
		psAvailable.Store(true)
	}
	return available
}

// systemProcessTree snapshots leaderPID's whole tree — see
// ProcessAdapter.ProcessTree. A non-zero `ps` (spawn failure, timeout, or a
// real error) is Unknown, never an empty tree: an empty tree is what a reused
// leader pid looks like, and signalling from it would hit the wrong group.
func systemProcessTree(leaderPID int64, leaderStartIdentity string) ProcessTreeSnapshot {
	code, stdout := captureCommand([]string{"ps", "-Ao", "pid=,ppid=,pgid=,lstart="})
	if code != 0 {
		return UnknownSnapshot()
	}
	tree := BuildProcessTree(ParsePsTreeRows(stdout), leaderPID, leaderStartIdentity)
	if len(tree) == 0 {
		return AbsentSnapshot()
	}
	return PresentSnapshot(tree)
}

// systemLiveStartIdentities is pid -> lstart for every live process — see
// ProcessAdapter.LiveStartIdentities. nil when `ps` could not be read.
func systemLiveStartIdentities() map[int64]string {
	code, stdout := captureCommand([]string{"ps", "-Ao", "pid=,lstart="})
	if code != 0 {
		return nil
	}
	return ParsePsAliveRows(stdout)
}

func containerRunning(containerName string) (bool, error) {
	code, stdout, stderr := captureCommandOutput([]string{
		"docker", "inspect", "-f", "{{.State.Running}}", containerName,
	})
	if !dockerInspectAnswered(code, stderr) {
		return false, errProbeFailed
	}
	return strings.TrimSpace(stdout) == "true", nil
}

// containerRecord errors when the probe itself failed (spawn/timeout, or a
// daemon that cannot be reached): a wedged Docker Desktop must not read as
// "container gone". "No such container" is a nil record with nil error.
func containerRecord(containerName, commandFingerprint string) (*DockerContainerRecord, error) {
	code, stdout, stderr := captureCommandOutput([]string{
		"docker", "inspect", "-f", "{{.Id}}\t{{.State.Running}}\t{{.State.StartedAt}}", containerName,
	})
	if !dockerInspectAnswered(code, stderr) {
		return nil, errProbeFailed
	}
	parts := strings.Split(strings.TrimSpace(stdout), "\t")
	if len(parts) == 3 && parts[1] == "true" && parts[0] != "" && parts[2] != "" {
		return &DockerContainerRecord{
			ContainerName:      containerName,
			ContainerID:        parts[0],
			ContainerStartedAt: parts[2],
			CommandFingerprint: commandFingerprint,
		}, nil
	}
	return nil, nil
}

// runningContainers returns container id -> StartedAt for each of ids that is
// currently running, from one `docker inspect`. nil when docker could not be
// asked at all (a missing id is simply absent from the map, which is how a
// stopped container is reported).
func runningContainers(containerIDs []string) map[string]string {
	argv := []string{
		"docker", "inspect", "--type", "container", "-f",
		"{{.Id}}\t{{.State.Running}}\t{{.State.StartedAt}}",
	}
	argv = append(argv, containerIDs...)
	code, stdout, stderr := captureCommandOutput(argv)
	if !dockerInspectAnswered(code, stderr) {
		return nil
	}
	out := map[string]string{}
	for _, line := range strings.Split(stdout, "\n") {
		parts := strings.Split(strings.TrimSpace(line), "\t")
		if len(parts) == 3 && parts[1] == "true" {
			out[parts[0]] = parts[2]
		}
	}
	return out
}

// containerWatch is one tracked container.
type containerWatch struct {
	record DockerContainerRecord
	exited chan int32
}

// ContainerWatcher reports when a started container stops being the instance
// that was started (stopped, removed, or recreated under a new id). One poll
// loop checks every tracked container with a single `docker inspect` every
// containerPollInterval — a per-container 500 ms poll meant ~20 docker CLI
// spawns a second for ten services — and exits once nothing is tracked.
//
// Rust also retires a watch whose oneshot receiver was dropped; Go has no
// channel-abandonment signal, and the engine always drains Exited (or stops
// the container), so a watch is retired when its container is gone.
type ContainerWatcher struct {
	mu      sync.Mutex
	watches []*containerWatch
	polling bool
}

// Watch registers record for tracking; exited receives 0 when the container
// is gone.
func (w *ContainerWatcher) Watch(record DockerContainerRecord, exited chan int32) {
	startPolling := false
	w.mu.Lock()
	w.watches = append(w.watches, &containerWatch{record: record, exited: exited})
	if !w.polling {
		w.polling = true
		startPolling = true
	}
	w.mu.Unlock()
	if startPolling {
		go w.poll()
	}
}

func (w *ContainerWatcher) poll() {
	for {
		time.Sleep(containerPollInterval)
		w.mu.Lock()
		var ids []string
		for _, watch := range w.watches {
			ids = append(ids, watch.record.ContainerID)
		}
		if len(w.watches) == 0 {
			w.polling = false
			w.mu.Unlock()
			return
		}
		w.mu.Unlock()

		running := runningContainers(ids)
		if running == nil {
			continue
		}
		// Only containers this tick actually asked about can be declared gone
		// — a watch added while `docker inspect` ran is absent from `running`
		// without having exited.
		var gone []chan int32
		w.mu.Lock()
		asked := map[string]bool{}
		for _, id := range ids {
			asked[id] = true
		}
		var kept []*containerWatch
		for _, watch := range w.watches {
			id := watch.record.ContainerID
			if asked[id] && running[id] != watch.record.ContainerStartedAt {
				gone = append(gone, watch.exited)
			} else {
				kept = append(kept, watch)
			}
		}
		w.watches = kept
		w.mu.Unlock()
		for _, exited := range gone {
			select {
			case exited <- 0:
			default:
			}
		}
	}
}

func sameContainerInstance(expected *DockerContainerRecord, observed *DockerContainerRecord) bool {
	return observed != nil &&
		observed.ContainerName == expected.ContainerName &&
		observed.ContainerID == expected.ContainerID &&
		observed.ContainerStartedAt == expected.ContainerStartedAt
}

func tailnetServing() bool {
	code, stdout := captureCommand([]string{"tailscale", "serve", "status", "--json"})
	if code != 0 {
		return false
	}
	var v map[string]any
	if err := json.Unmarshal([]byte(stdout), &v); err != nil {
		return false
	}
	web, ok := v["Web"].(map[string]any)
	return ok && len(web) > 0
}

func tcpProbe(port uint16) bool {
	conn, err := net.DialTimeout("tcp", fmt.Sprintf("127.0.0.1:%d", port), 250*time.Millisecond)
	if err != nil {
		return false
	}
	_ = conn.Close()
	return true
}

// portHolders resolves the pids listening on `port` per `lsof`, each resolved
// through `ps` so a holder carries the same lstart identity the pid-reuse
// guard compares before signalling, and its raw command line for the "held by
// pid N (cmd)" refusal/prompt text.
//
// nil means "no capability" — `lsof` couldn't even spawn — so callers degrade
// to the generic "an unowned process" wording and a kill request can't resolve
// a target (fails closed, not blind). A non-nil empty slice means `lsof` ran
// and found nobody: between PortInUse and this call the holder exited, which
// the reclaim loop treats as "nothing to signal, just re-poll the port".
func portHolders(port uint16) *[]PortHolder {
	spec := fmt.Sprintf("-iTCP:%d", port)
	code, stdout := captureCommand([]string{"lsof", "-nP", "-t", spec, "-sTCP:LISTEN"})
	if code == -1 {
		return nil
	}
	var holders []PortHolder
	seen := map[int64]bool{}
	for _, field := range strings.Fields(stdout) {
		pid, err := strconv.ParseInt(field, 10, 64)
		if err != nil || seen[pid] {
			continue
		}
		seen[pid] = true
		if obs, err := observedSystemProcessWithCommand(pid); err == nil && obs != nil {
			holders = append(holders, PortHolder{
				Pid:           pid,
				Pgid:          obs.record.Pgid,
				StartIdentity: obs.record.StartIdentity,
				Command:       obs.commandLine,
			})
		}
	}
	return &holders
}

// forwardStream drains a pipe, handing each chunk to onOutput.
func forwardStream(stream io.Reader, onOutput OnOutput) {
	buf := make([]byte, 8192)
	for {
		n, err := stream.Read(buf)
		if n > 0 && onOutput != nil {
			chunk := strings.ToValidUTF8(string(buf[:n]), "")
			if chunk != "" {
				onOutput(chunk)
			}
		}
		if err != nil || n == 0 {
			return
		}
	}
}

// finishHelperOutput gives a helper's output forwarders helperOutputDrain to
// reach EOF after the leader exited. A pipe still open after that is held by
// something the helper started in its group and left running (`sleep 1000 &
// exit 0`): SIGKILL the group, then give the forwarders a moment to read the
// last bytes. A child that redirected its output elsewhere and left the group
// is not ours to find here.
func finishHelperOutput(pgid int64, doneChans []chan struct{}) {
	if !waitAll(doneChans, helperOutputDrain) {
		sendSignalToGroup(pgid, SignalKill)
		waitAll(doneChans, 500*time.Millisecond)
	}
}

func waitAll(dones []chan struct{}, d time.Duration) bool {
	deadline := time.After(d)
	for _, ch := range dones {
		select {
		case <-ch:
		case <-deadline:
			return false
		}
	}
	return true
}

// envList renders an env map as KEY=value pairs for exec.Cmd.Env (which
// fully replaces the environment — env_clear + envs semantics).
func envList(env map[string]string) []string {
	out := make([]string, 0, len(env))
	for k, v := range env {
		out = append(out, k+"="+v)
	}
	return out
}

// runCommand spawns argv (cwd/env applied, own process group), forwards both
// stdout and stderr to onOutput, and returns its exit code (-1 if it couldn't
// even be spawned). ctx (may be nil) cancels the wait and kills the group.
func runCommand(ctx CommandContext, argv []string, cwd string, env map[string]string, onOutput OnOutput) int32 {
	if len(argv) == 0 {
		return -1
	}
	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Dir = cwd
	cmd.Env = envList(env)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return -1
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		return -1
	}
	if err := cmd.Start(); err != nil {
		return -1
	}
	pgid := int64(cmd.Process.Pid)
	outDone := make(chan struct{})
	errDone := make(chan struct{})
	go func() { forwardStream(stdout, onOutput); close(outDone) }()
	go func() { forwardStream(stderr, onOutput); close(errDone) }()
	waitCh := make(chan error, 1)
	go func() { waitCh <- cmd.Wait() }()

	cancelled := false
	var werr error
	if ctx == nil {
		werr = <-waitCh
	} else {
		select {
		case werr = <-waitCh:
		case <-ctx.Done():
			cancelled = true
			sendSignalToGroup(pgid, SignalKill)
			<-waitCh
		}
	}
	finishHelperOutput(pgid, []chan struct{}{outDone, errDone})
	if cancelled {
		return -1
	}
	if werr == nil {
		return 0
	}
	if ee, ok := werr.(*exec.ExitError); ok {
		return int32(ee.ExitCode())
	}
	return -1
}

// childHandle wraps an exec.Cmd whose Wait runs in a goroutine, exposing the
// result channel and whether the process has been reaped.
type childHandle struct {
	cmd    *exec.Cmd
	waitCh chan error
	reaped atomic.Bool
}

func (c *childHandle) wait() {
	c.waitCh <- c.cmd.Wait()
	c.reaped.Store(true)
}

// stopUnverifiedChild stops a just-spawned child whose identity could not be
// verified. The child was spawned with Setpgid so its pgid is its pid.
// Signalling only the leader leaves grandchildren (the shell's own children)
// holding the port. SIGKILL after the grace matches runCommand's guard.
func stopUnverifiedChild(child *childHandle) {
	pid := int64(child.cmd.Process.Pid)
	if child.reaped.Load() {
		return
	}
	sendSignalToGroup(pid, SignalTerm)
	select {
	case <-child.waitCh:
		return
	case <-time.After(500 * time.Millisecond):
	}
	if !child.reaped.Load() {
		sendSignalToGroup(pid, SignalKill)
		select {
		case <-child.waitCh:
		case <-time.After(500 * time.Millisecond):
		}
	}
}

// acceptsSpawnObservation — whether an observation of a just-spawned non-exec
// process can be trusted as its identity's command fingerprint.
//
// Right after spawning, the child may still be mid-`execve`, and macOS `ps`
// then reports a parenthesized placeholder (`(sh)`) instead of the real
// command line. Storing that as the fingerprint poisons the identity
// permanently: once exec completes, `ps` reports the real command,
// ownsIdentity compares unequal, and the daemon disowns and orphans the
// service it just started — after which the next start fails with "Port N is
// held by an unowned process".
//
// The OBSERVED value (not the expected one) is what must ultimately be
// stored: for an argv command, `ps` reports the resolved binary path where
// argv[0] may have been a bare name, and later Inspect comparisons are made
// against `ps` output. So an observation is accepted when it already equals
// what we spawned, or when it repeats identically across two polls — which a
// transient mid-exec placeholder does not.
func acceptsSpawnObservation(current *PosixProcessRecord, expectedFingerprint string, previous *PosixProcessRecord) bool {
	if current.CommandFingerprint == expectedFingerprint {
		return true
	}
	return previous != nil &&
		previous.Pid == current.Pid &&
		previous.Pgid == current.Pgid &&
		previous.StartIdentity == current.StartIdentity &&
		previous.CommandFingerprint == current.CommandFingerprint
}

func sameRecord(a, b *PosixProcessRecord) bool {
	if a == nil || b == nil {
		return false
	}
	return a.Pid == b.Pid && a.Pgid == b.Pgid &&
		a.StartIdentity == b.StartIdentity &&
		a.CommandFingerprint == b.CommandFingerprint &&
		a.CommandLine == b.CommandLine
}

// observedStableExecProcess — a shell wrapper's own fingerprint (the `sh -c
// ...` line as `ps` shows it *before* exec) differs from the execed program's;
// this polls until it observes the SAME non-wrapper fingerprint twice in a
// row, with a settle-check delay between, before trusting it.
func observedStableExecProcess(child *childHandle, expectedFingerprint string) (*PosixProcessRecord, error) {
	pid := int64(child.cmd.Process.Pid)
	var candidate *PosixProcessRecord
	for i := 0; i < 20; i++ {
		time.Sleep(25 * time.Millisecond)
		var rec *PosixProcessRecord
		if obs, err := observedSystemProcessWithCommand(pid); err == nil && obs != nil && !IsShellWrapperCommand(obs.commandLine) {
			rec = &obs.record
		}
		if rec == nil {
			candidate = nil
			continue
		}
		if rec.CommandFingerprint == expectedFingerprint {
			candidate = nil
			continue
		}
		if !sameRecord(candidate, rec) {
			candidate = rec
			continue
		}
		time.Sleep(execIdentitySettleMS * time.Millisecond)
		if settled, err := observedSystemProcessWithCommand(pid); err == nil && settled != nil {
			if !IsShellWrapperCommand(settled.commandLine) &&
				settled.record.CommandFingerprint != expectedFingerprint &&
				sameRecord(&settled.record, candidate) {
				return &settled.record, nil
			}
		}
		candidate = nil
	}
	stopUnverifiedChild(child)
	return nil, NewError("Unable to establish stable POSIX exec process identity")
}

// observedStableShellProcess — non-exec shell identity. The first `ps` row of
// `sh -c` already hashes to the logical command, and macOS `sh` then
// implicit-execs (dropping quotes) so that stored identity no longer matches
// the live process. Wait until the same row is still there after
// execIdentitySettleMS. A wrapper that never execs stays `sh -c` for the whole
// window and is accepted. A line that changes restarts the window on the new
// row.
func observedStableShellProcess(child *childHandle) (*PosixProcessRecord, error) {
	pid := int64(child.cmd.Process.Pid)
	var candidate *PosixProcessRecord
	deadline := time.Now().Add(15 * time.Second)
	for time.Now().Before(deadline) {
		time.Sleep(25 * time.Millisecond)
		var rec *PosixProcessRecord
		if obs, err := observedSystemProcess(pid); err == nil && obs != nil {
			rec = obs.Record.Posix
		}
		if rec == nil {
			candidate = nil
			continue
		}
		if !sameRecord(candidate, rec) {
			candidate = rec
			continue
		}
		time.Sleep(execIdentitySettleMS * time.Millisecond)
		obs, err := observedSystemProcess(pid)
		if err == nil && obs != nil && obs.Record.Posix != nil && sameRecord(obs.Record.Posix, candidate) {
			return obs.Record.Posix, nil
		}
		if err == nil && obs != nil {
			candidate = obs.Record.Posix
		} else {
			candidate = nil
		}
	}
	stopUnverifiedChild(child)
	return nil, NewError("Unable to establish stable POSIX shell process identity")
}

// utf8CompletePrefix is the longest prefix of bytes that does not end mid-way
// through a UTF-8 sequence, so a chunked read never renders a split character
// as a replacement character.
func utf8CompletePrefix(b []byte) int {
	if utf8.Valid(b) {
		return len(b)
	}
	i := 0
	last := 0
	for i < len(b) {
		if !utf8.FullRune(b[i:]) {
			return last
		}
		_, size := utf8.DecodeRune(b[i:])
		i += size
		last = i
	}
	return len(b)
}

func drainRawLogOnce(path string, offset *atomic.Uint64, onOutput OnOutput) {
	info, err := os.Stat(path)
	if err != nil {
		return
	}
	size := info.Size()
	start := int64(offset.Load())
	if size <= start {
		return
	}
	file, err := os.Open(path)
	if err != nil {
		return
	}
	buf := make([]byte, rawLogReadChunk)
	for start < size {
		want := size - start
		if want > int64(rawLogReadChunk) {
			want = int64(rawLogReadChunk)
		}
		if _, err := file.ReadAt(buf[:want], start); err != nil {
			break
		}
		// Always hold back a trailing partial character — the writer may be
		// mid-character at the current end of file too, and the next drain
		// picks up from `start`.
		end := utf8CompletePrefix(buf[:want])
		if end < 1 {
			end = 1
		}
		text := strings.ToValidUTF8(string(buf[:end]), "")
		if text != "" && onOutput != nil {
			onOutput(text)
		}
		start += int64(end)
		offset.Store(uint64(start))
	}
	_ = file.Close()
	// copytruncate, only once the file is big: the writer's fd is append-mode,
	// so after Truncate(0) its next write lands at the new end. Anything it
	// appends between the Stat check and Truncate is still lost — the size
	// check narrows that window but cannot close it — so truncation is kept
	// rare rather than attempted on every quiet poll.
	if size >= rawLogTruncateBytes {
		if f, err := os.OpenFile(path, os.O_WRONLY, 0); err == nil {
			if fi, err := f.Stat(); err == nil && fi.Size() == size {
				if f.Truncate(0) == nil {
					offset.Store(0)
				}
			}
			_ = f.Close()
		}
	}
}

// tailContainerLogs: `docker compose up` forwards only the compose CLI's own
// status lines — a container's real stdout/stderr lives behind `docker logs`.
// This follows it, bounded by since/tail — a daemon adopting a long-running
// external container must not replay days of retained output into the log
// store on every re-attach. The returned tail's stop kills the `docker logs`
// child; the task also ends on its own when the container stops, since
// `docker logs --follow` exits with it.
//
// Rust also sets `kill_on_drop` so the follower dies with the daemon even if
// nothing stops it. Go has no process-drop hook, so the engine's
// DetachAllOutput (a leave-services shutdown) is what ends it there.
func tailContainerLogs(containerName string, since *string, tail *uint64, env map[string]string, onOutput OnOutput) *OutputTail {
	cancel := make(chan struct{})
	done := &atomic.Bool{}
	go func() {
		defer done.Store(true)
		argv := []string{"logs", "--follow"}
		if since != nil {
			argv = append(argv, "--since", *since)
		}
		if tail != nil {
			argv = append(argv, "--tail", strconv.FormatUint(*tail, 10))
		}
		argv = append(argv, containerName)
		cmd := exec.Command("docker", argv...)
		cmd.Env = envList(env)
		cmd.Stdin = nil
		cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
		stdout, err := cmd.StdoutPipe()
		if err != nil {
			return
		}
		stderr, err := cmd.StderrPipe()
		if err != nil {
			return
		}
		if err := cmd.Start(); err != nil {
			return
		}
		var wg sync.WaitGroup
		wg.Add(2)
		go func() { defer wg.Done(); forwardStream(stdout, onOutput) }()
		go func() { defer wg.Done(); forwardStream(stderr, onOutput) }()
		waitCh := make(chan error, 1)
		go func() { waitCh <- cmd.Wait() }()
		select {
		case <-cancel:
			_ = cmd.Process.Kill()
			<-waitCh
		case <-waitCh:
		}
		wg.Wait()
	}()
	var once sync.Once
	return NewOutputTail(func() {
		once.Do(func() { close(cancel) })
	}, done)
}

// tailFile polls a raw capture file every rawLogPollMS, forwarding new bytes
// and truncating what it's read. The returned tail's done is the same flag
// stop sets — this follower never finishes on its own (the raw file outlives
// whatever wrote to it).
func tailFile(path string, skipBacklog bool, onOutput OnOutput) *OutputTail {
	stopped := &atomic.Bool{}
	offset := &atomic.Uint64{}
	// Adopted processes keep the previous daemon's raw capture file:
	// everything before this offset was already forwarded, so re-reading it
	// would replay old output into the log store.
	if skipBacklog {
		if info, err := os.Stat(path); err == nil {
			offset.Store(uint64(info.Size()))
		}
	}
	quit := make(chan struct{})
	var once sync.Once
	go func() {
		for !stopped.Load() {
			drainRawLogOnce(path, offset, onOutput)
			select {
			case <-quit:
				return
			case <-time.After(time.Duration(rawLogPollMS) * time.Millisecond):
			}
		}
	}()
	return NewOutputTail(func() {
		if stopped.CompareAndSwap(false, true) {
			drainRawLogOnce(path, offset, onOutput)
			once.Do(func() { close(quit) })
		}
	}, stopped)
}

func mergedEnvironment(base, extra map[string]string) map[string]string {
	env := make(map[string]string, len(base)+len(extra))
	for k, v := range base {
		env[k] = v
	}
	for k, v := range extra {
		env[k] = v
	}
	return env
}

// DefaultProcessAdapter is the production ProcessAdapter.
type DefaultProcessAdapter struct {
	Root             string
	RuntimeDirectory string
	BaseEnvironment  map[string]string
	Containers       *ContainerWatcher
}

func NewDefaultProcessAdapter(root, runtimeDir string, baseEnv map[string]string) *DefaultProcessAdapter {
	return &DefaultProcessAdapter{
		Root:             root,
		RuntimeDirectory: runtimeDir,
		BaseEnvironment:  baseEnv,
		Containers:       &ContainerWatcher{},
	}
}

func (a *DefaultProcessAdapter) Spawn(input SpawnInput, onOutput OnOutput) (*ManagedProcess, error) {
	env := mergedEnvironment(a.BaseEnvironment, input.Command.Environment)

	if catalog.IsContainerCommand(&input.Command) {
		argv, _ := CommandArgv(&input.Command.Command)
		cwd := filepath.Join(a.Root, input.Command.Cwd)
		if code := runCommand(nil, argv, cwd, env, onOutput); code != 0 {
			return nil, Errorf("Docker service command exited with %d", code)
		}
		containerName := *input.Command.ContainerName
		record, err := containerRecord(containerName, input.CommandFingerprint)
		if err != nil || record == nil {
			return nil, Errorf("Docker container %s is not running after start", containerName)
		}
		exited := make(chan int32, 1)
		a.Containers.Watch(*record, exited)
		return &ManagedProcess{
			Record: ProcessRecord{Docker: record},
			Exited: exited,
		}, nil
	}

	if !processInspectionAvailable() {
		return nil, NewError("POSIX process inspection is unavailable; refusing to start an unverified service process")
	}

	// Managed dev processes must outlive the daemon that spawned them. Piping
	// their stdout/stderr straight into this daemon would mean the pipe's read
	// end closes whenever the daemon exits, earning the child a SIGPIPE on its
	// next log write. Redirect to a plain file instead; AttachOutput/tailFile
	// reads that file separately.
	argv, execMode := CommandArgv(&input.Command.Command)
	if err := os.MkdirAll(paths.LogsDir(a.RuntimeDirectory), 0o755); err != nil {
		return nil, Errorf("failed to create logs directory: %v", err)
	}
	raw := paths.RawLogPath(a.RuntimeDirectory, input.ServiceID)
	if f, err := os.OpenFile(raw, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o644); err != nil {
		return nil, Errorf("%v", err)
	} else {
		_ = f.Close()
	}
	stdoutFile, err := os.OpenFile(raw, os.O_APPEND|os.O_WRONLY, 0o644)
	if err != nil {
		return nil, Errorf("%v", err)
	}
	defer stdoutFile.Close()

	cwd := filepath.Join(a.Root, input.Command.Cwd)
	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Dir = cwd
	cmd.Env = envList(env)
	cmd.Stdout = stdoutFile
	cmd.Stderr = stdoutFile
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := cmd.Start(); err != nil {
		return nil, Errorf("Service process failed to start: %v", err)
	}
	child := &childHandle{cmd: cmd, waitCh: make(chan error, 1)}
	go child.wait()
	pid := int64(cmd.Process.Pid)

	var record *PosixProcessRecord
	switch {
	case execMode:
		record, err = observedStableExecProcess(child, input.CommandFingerprint)
		if err != nil {
			return nil, err
		}
	case input.Command.Command.IsShell():
		record, err = observedStableShellProcess(child)
		if err != nil {
			return nil, err
		}
	default:
		// A freshly spawned child can still be mid-`execve` when `ps` is asked
		// about it, and macOS then reports a placeholder command line —
		// literally `(sh)` — instead of the real one (reproducible: ~15% of
		// spawns under load). Taking that first readable row as the
		// authoritative fingerprint poisons the identity for the rest of the
		// service's life: once exec completes `ps` reports the real command,
		// ownsIdentity compares unequal, and the daemon disowns and orphans
		// the service it just started — after which the next start fails with
		// "Port N is held by an unowned process".
		//
		// The OBSERVED value still has to be what gets stored (for an argv
		// command `ps` reports the resolved binary path where argv[0] may have
		// been a bare name, and later Inspect comparisons are made against
		// `ps` output), so this waits for a trustworthy observation rather
		// than substituting the expected fingerprint: either it already equals
		// what we spawned, or it repeats identically across two polls — which
		// a mid-exec placeholder never does.
		var observed *PosixProcessRecord
		var previous *PosixProcessRecord
		for attempt := 0; attempt < 8; attempt++ {
			if obs, oerr := observedSystemProcess(pid); oerr == nil && obs != nil && obs.Record.Posix != nil {
				current := obs.Record.Posix
				if acceptsSpawnObservation(current, input.CommandFingerprint, previous) {
					observed = current
					break
				}
				previous = current
			} else {
				previous = nil
			}
			if attempt < 7 {
				time.Sleep(25 * time.Millisecond)
			}
		}
		if observed == nil {
			stopUnverifiedChild(child)
			return nil, NewError("Unable to establish POSIX process ownership identity after 8 inspections")
		}
		record = observed
	}

	exited := make(chan int32, 1)
	go func() {
		werr := <-child.waitCh
		code := int32(-1)
		if werr == nil {
			code = 0
		} else if ee, ok := werr.(*exec.ExitError); ok {
			code = int32(ee.ExitCode())
		}
		exited <- code
	}()
	return &ManagedProcess{
		Record: ProcessRecord{Posix: record},
		Exited: exited,
	}, nil
}

func (a *DefaultProcessAdapter) Inspect(identity *state.ProcessIdentity) Inspection {
	if identity.ContainerID != nil {
		// Docker identity. CommandFingerprint is a plain string on the Go
		// identity (both posix and docker rows carry it); only the container
		// fields are pointers.
		record, err := containerRecord(*identity.ContainerName, identity.CommandFingerprint)
		if err != nil {
			return Inspection{Kind: InspectionUnknown}
		}
		if record == nil {
			return Inspection{Kind: InspectionGone}
		}
		expected := DockerContainerRecord{
			ContainerName:      *identity.ContainerName,
			ContainerID:        *identity.ContainerID,
			ContainerStartedAt: *identity.ContainerStartedAt,
			CommandFingerprint: identity.CommandFingerprint,
		}
		if sameContainerInstance(&expected, record) {
			return Observed(ProcessRecord{Docker: record}, true)
		}
		return Inspection{Kind: InspectionGone}
	}
	if identity.Pid != nil {
		if *identity.Pid == 0 {
			return Observed(ProcessRecord{Posix: &PosixProcessRecord{
				Pid:                0,
				Pgid:               0,
				StartIdentity:      "",
				CommandFingerprint: identity.CommandFingerprint,
				CommandLine:        "",
			}}, false)
		}
		observed, err := observedSystemProcess(*identity.Pid)
		if err != nil {
			return Inspection{Kind: InspectionUnknown}
		}
		if observed == nil {
			return Inspection{Kind: InspectionGone}
		}
		return Observed(observed.Record, true)
	}
	return Inspection{Kind: InspectionGone}
}

func (a *DefaultProcessAdapter) SignalGroup(pgid int64, signal ProcessSignal) {
	sendSignalToGroup(pgid, signal)
}

func (a *DefaultProcessAdapter) SignalPID(pid int64, expectedStartIdentity string, signal ProcessSignal) {
	if pid <= 1 {
		return
	}
	// The lstart the caller resolved is compared again at signal time — a
	// squatter that died and had its pid recycled between resolve and kill
	// must never take the signal.
	observed, err := observedSystemProcess(pid)
	if err == nil && observed != nil && observed.Alive && observed.Record.Posix != nil &&
		observed.Record.Posix.StartIdentity == expectedStartIdentity {
		sendSignalPID(pid, signal)
	}
}

func (a *DefaultProcessAdapter) ProcessTree(leaderPID int64, leaderStartIdentity string) ProcessTreeSnapshot {
	return systemProcessTree(leaderPID, leaderStartIdentity)
}

func (a *DefaultProcessAdapter) LiveStartIdentities() map[int64]string {
	return systemLiveStartIdentities()
}

func (a *DefaultProcessAdapter) CommandMatches(fingerprints, executables []string, cwd string) *[]CommandMatch {
	return MatchingServiceProcesses(a.Root, fingerprints, executables, cwd)
}

func (a *DefaultProcessAdapter) StopContainer(command *catalog.ServiceCommand, onOutput OnOutput) (bool, error) {
	defaultStop := catalog.CommandSpec{Argv: []string{"docker", "compose", "stop"}}
	spec := &defaultStop
	if command.DockerStopCommand != nil {
		spec = command.DockerStopCommand
	}
	argv, _ := CommandArgv(spec)
	// Compose is cwd- and env-sensitive (`COMPOSE_FILE`,
	// `COMPOSE_PROJECT_NAME`). Stop has to use the same directory and merged
	// environment the start used, or it targets a different project.
	cwd := filepath.Join(a.Root, command.Cwd)
	env := mergedEnvironment(a.BaseEnvironment, command.Environment)
	if code := runCommand(nil, argv, cwd, env, onOutput); code != 0 {
		return true, Errorf("Docker service stop exited with %d", code)
	}
	return true, nil
}

func (a *DefaultProcessAdapter) AttachOutput(serviceID string, source OutputSource, onOutput OnOutput) *OutputTail {
	if source.Process {
		return tailFile(paths.RawLogPath(a.RuntimeDirectory, serviceID), source.SkipBacklog, onOutput)
	}
	return tailContainerLogs(source.ContainerName, source.Since, source.Tail, a.BaseEnvironment, onOutput)
}

// DefaultProbeAdapter is the production ProbeAdapter.
type DefaultProbeAdapter struct {
	Root            string
	BaseEnvironment map[string]string
	HTTPClient      *http.Client
}

func NewDefaultProbeAdapter(root string, baseEnv map[string]string) *DefaultProbeAdapter {
	return &DefaultProbeAdapter{Root: root, BaseEnvironment: baseEnv, HTTPClient: &http.Client{}}
}

func (a *DefaultProbeAdapter) TCP(port uint16) bool { return tcpProbe(port) }

func (a *DefaultProbeAdapter) HTTP(url string) bool {
	client := &http.Client{Timeout: 250 * time.Millisecond}
	resp, err := client.Get(url)
	if err != nil {
		return false
	}
	defer resp.Body.Close()
	return resp.StatusCode >= 200 && resp.StatusCode < 300
}

func (a *DefaultProbeAdapter) Container(containerName string) bool {
	running, err := containerRunning(containerName)
	return err == nil && running
}

func (a *DefaultProbeAdapter) Tailnet() bool { return tailnetServing() }

func (a *DefaultProbeAdapter) PortInUse(port uint16) *bool {
	v := tcpProbe(port)
	return &v
}

func (a *DefaultProbeAdapter) PortHolders(port uint16) *[]PortHolder { return portHolders(port) }

func (a *DefaultProbeAdapter) Command(ctx CommandContext, command *catalog.CommandSpec, cwd *string) *bool {
	argv, _ := CommandArgv(command)
	dir := a.Root
	if cwd != nil {
		dir = filepath.Join(a.Root, *cwd)
	}
	ok := runCommand(ctx, argv, dir, a.BaseEnvironment, nil) == 0
	return &ok
}

// DefaultRunBuild is the production RunBuild.
type DefaultRunBuild struct {
	Root            string
	BaseEnvironment map[string]string
}

func NewDefaultRunBuild(root string, baseEnv map[string]string) *DefaultRunBuild {
	return &DefaultRunBuild{Root: root, BaseEnvironment: baseEnv}
}

func (b *DefaultRunBuild) Run(command *catalog.ServiceCommand, onOutput OnOutput, cancel <-chan struct{}) error {
	select {
	case <-cancel:
		return NewError("Build cancelled")
	default:
	}
	argv, _ := CommandArgv(&command.Command)
	env := mergedEnvironment(b.BaseEnvironment, command.Environment)
	cwd := filepath.Join(b.Root, command.Cwd)
	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Dir = cwd
	cmd.Env = envList(env)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return Errorf("Build command failed to start: %v", err)
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		return Errorf("Build command failed to start: %v", err)
	}
	if err := cmd.Start(); err != nil {
		return Errorf("Build command failed to start: %v", err)
	}
	pgid := int64(cmd.Process.Pid)
	outDone := make(chan struct{})
	errDone := make(chan struct{})
	go func() { forwardStream(stdout, onOutput); close(outDone) }()
	go func() { forwardStream(stderr, onOutput); close(errDone) }()
	waitCh := make(chan error, 1)
	go func() { waitCh <- cmd.Wait() }()

	select {
	case werr := <-waitCh:
		finishHelperOutput(pgid, []chan struct{}{outDone, errDone})
		code := int32(-1)
		if werr == nil {
			code = 0
		} else if ee, ok := werr.(*exec.ExitError); ok {
			code = int32(ee.ExitCode())
		}
		if code != 0 {
			return Errorf("Build command exited with %d", code)
		}
		return nil
	case <-cancel:
		sendSignalToGroup(pgid, SignalTerm)
		select {
		case <-waitCh:
		case <-time.After(5 * time.Second):
			sendSignalToGroup(pgid, SignalKill)
			<-waitCh
		}
		finishHelperOutput(pgid, []chan struct{}{outDone, errDone})
		return NewError("Build cancelled")
	}
}

// DefaultArtifactInstaller runs `artifact:` installs for file-loaded catalogs
// — delegates to the same tarball machinery the shared-services installer
// uses, scoped to the project's runtime directory.
type DefaultArtifactInstaller struct {
	Root             string
	RuntimeDirectory string
}

func (a *DefaultArtifactInstaller) Install(service *catalog.ServiceDefinition, onOutput OnOutput) error {
	artifact := service.Artifact
	if artifact == nil {
		return Errorf("%s: no artifact declared", service.ID)
	}
	if err := shared.InstallServiceArtifact(a.Root, a.RuntimeDirectory, service, artifact, func(line string) {
		if onOutput != nil {
			onOutput(line)
		}
	}); err != nil {
		return NewError(err.Error())
	}
	return nil
}

// DefaultSupervisorOptions wires production adapters. baseEnvironment should
// come from env.ResolveBaseEnvironment for a daemon that might be launched
// from a GUI (bare PATH, no login-shell customization) — nil defaults to the
// current process's own environment, i.e. whatever spawned the daemon.
func DefaultSupervisorOptions(root, runtimeDirectory string, baseEnvironment map[string]string) SupervisorOptions {
	if runtimeDirectory == "" {
		runtimeDirectory = paths.ResolveRuntimeDirectory(root, "")
	}
	if baseEnvironment == nil {
		baseEnvironment = map[string]string{}
		for _, kv := range os.Environ() {
			if i := strings.IndexByte(kv, '='); i >= 0 {
				baseEnvironment[kv[:i]] = kv[i+1:]
			}
		}
	}
	return SupervisorOptions{
		Process:  NewDefaultProcessAdapter(root, runtimeDirectory, baseEnvironment),
		RunBuild: NewDefaultRunBuild(root, baseEnvironment),
		ArtifactInstaller: &DefaultArtifactInstaller{
			Root:             root,
			RuntimeDirectory: runtimeDirectory,
		},
		Probes:             NewDefaultProbeAdapter(root, baseEnvironment),
		Preparation:        nil,
		Clock:              SystemClock{},
		ReadinessTimeoutMs: 10_000,
		ReadinessBackoffMs: 1_500,
		TerminationGraceMs: 5_000,
		IsClosing:          func() bool { return false },
	}
}
