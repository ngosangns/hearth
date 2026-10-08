// Package supervisor ports the ProcessSupervisor state machine: start/stop/
// restart/readiness/adoption. Every OS interaction goes through the adapter
// interfaces in this file so the engine can run against fakes in tests;
// adapters.go is the production implementation.
package supervisor

import (
	"fmt"
	"sync/atomic"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/iso8601"
	"github.com/ngosangns/hearth/go/internal/state"
)

// ProcessSignal is the escalation pair a stop walks through.
type ProcessSignal int

const (
	SignalTerm ProcessSignal = iota
	SignalKill
)

// PosixProcessRecord is what `ps` reports about a live process. StartIdentity
// is the raw `lstart` field — the pid-reuse guard compares that exact string,
// never a parsed timestamp.
type PosixProcessRecord struct {
	Pid                int64
	Pgid               int64
	StartIdentity      string
	CommandFingerprint string
	// Raw `ps` command line. Empty when the adapter did not capture one.
	CommandLine string
}

// DockerContainerRecord is what `docker inspect` reports about a container.
type DockerContainerRecord struct {
	ContainerName      string
	ContainerID        string
	ContainerStartedAt string
	CommandFingerprint string
}

// ProcessRecord is the tagged union of the two record kinds.
type ProcessRecord struct {
	Posix  *PosixProcessRecord
	Docker *DockerContainerRecord
}

func (r ProcessRecord) CommandFingerprint() string {
	if r.Docker != nil {
		return r.Docker.CommandFingerprint
	}
	if r.Posix != nil {
		return r.Posix.CommandFingerprint
	}
	return ""
}

func (r ProcessRecord) IsDocker() bool { return r.Docker != nil }

// ObservedProcess is an inspect result: the record the probe saw, plus whether
// it is still alive.
type ObservedProcess struct {
	Record ProcessRecord
	Alive  bool
}

type InspectionKind int

const (
	// InspectionObserved — the probe ran; Observed holds what it saw.
	InspectionObserved InspectionKind = iota
	// InspectionGone — the probe ran and the process/container is gone, or a
	// different instance occupies the pid/container name.
	InspectionGone
	// InspectionUnknown — the probe itself could not answer. Callers decide
	// nothing: keep the state, fail the operation, or skip — never conclude
	// "dead".
	InspectionUnknown
)

type Inspection struct {
	Kind     InspectionKind
	Observed *ObservedProcess
}

func Observed(record ProcessRecord, alive bool) Inspection {
	return Inspection{Kind: InspectionObserved, Observed: &ObservedProcess{Record: record, Alive: alive}}
}

// ManagedProcess is a spawned, live process/container. Exited is a one-shot
// channel that receives the exit code exactly once.
type ManagedProcess struct {
	Record ProcessRecord
	Exited <-chan int32
}

// SpawnInput is the adapter's spawn request.
type SpawnInput struct {
	Command            catalog.ServiceCommand
	CommandFingerprint string
	ServiceID          string
}

type SupervisorError struct{ Message string }

func (e *SupervisorError) Error() string { return e.Message }

func Errorf(format string, args ...any) *SupervisorError {
	return &SupervisorError{Message: fmt.Sprintf(format, args...)}
}

func NewError(msg string) *SupervisorError { return &SupervisorError{Message: msg} }

// OnOutput is a log/output chunk callback.
type OnOutput func(data string)

// StartOptions carries per-call opt-ins for StartWithOptions. KillUnowned is
// the wire-level echo of the user's explicit "yes, kill the process holding
// this port" — the engine never sets it itself.
type StartOptions struct {
	KillUnowned bool
}

// OutputSource is where a service's stdout/stderr stream lives — what
// ProcessAdapter.AttachOutput should tail for it.
type OutputSource struct {
	// Process mode: tail the raw capture file. SkipBacklog starts the tail at
	// the file's current size.
	Process     bool
	SkipBacklog bool
	// Container mode: `docker logs --follow`.
	ContainerName string
	Since         *string
	Tail          *uint64
}

func ProcessSource(skipBacklog bool) OutputSource {
	return OutputSource{Process: true, SkipBacklog: skipBacklog}
}

func ContainerSource(name string, since *string, tail *uint64) OutputSource {
	return OutputSource{ContainerName: name, Since: since, Tail: tail}
}

// OutputTail is a live output tail from AttachOutput. IsDone reports whether
// the tail ended on its own (a `docker logs --follow` exits when its container
// does); a caller keying re-attach on "is there an entry" would otherwise never
// notice.
type OutputTail struct {
	stop func()
	done *atomic.Bool
}

func NewOutputTail(stop func(), done *atomic.Bool) *OutputTail {
	return &OutputTail{stop: stop, done: done}
}

func (t *OutputTail) IsDone() bool { return t.done.Load() }

func (t *OutputTail) Stop() {
	if t.stop != nil {
		t.stop()
		t.stop = nil
	}
	t.done.Store(true)
}

// ProcessAdapter is the seam every process interaction goes through.
type ProcessAdapter interface {
	Spawn(input SpawnInput, onOutput OnOutput) (*ManagedProcess, error)
	Inspect(identity *state.ProcessIdentity) Inspection
	SignalGroup(pgid int64, signal ProcessSignal)
	// SignalPID signals exactly one pid, and only if the pid still presents
	// expectedStartIdentity — the pid-reuse guard lives here so the signal can
	// never land on a recycled process.
	SignalPID(pid int64, expectedStartIdentity string, signal ProcessSignal)
	// ProcessTree snapshots the whole process tree under leaderPID. Absent
	// unless the OS table still shows leaderStartIdentity for the leader;
	// Unknown means the table could not be read (not an empty tree).
	ProcessTree(leaderPID int64, leaderStartIdentity string) ProcessTreeSnapshot
	// LiveStartIdentities is one pid -> start identity snapshot of every live
	// process, or nil when the table could not be read.
	LiveStartIdentities() map[int64]string
	// CommandMatches lists live processes whose cwd is cwd (relative to the
	// project root) and whose command is one of fingerprints or executables.
	// nil means the table could not be read.
	CommandMatches(fingerprints, executables []string, cwd string) *[]CommandMatch
	// StopContainer runs the command's docker stop. supported=false if this
	// adapter has no container-stop support.
	StopContainer(command *catalog.ServiceCommand, onOutput OnOutput) (supported bool, err error)
	// AttachOutput returns a live tail handle if output attachment is
	// supported; nil otherwise.
	AttachOutput(serviceID string, source OutputSource, onOutput OnOutput) *OutputTail
}

// PreparationAdapter runs a service's declarative `preparation` marker list.
type PreparationAdapter interface {
	Prepare(serviceID string, steps []string) error
}

// CommandMatch is a host process whose command and working directory are a
// service's. StartIdentity is the `ps lstart` the pid-reuse guard compares
// before signalling.
type CommandMatch struct {
	Pid           int64
	StartIdentity string
}

// PortHolder is one process holding a TCP listen socket on a catalog port.
// Command is the raw command line (not a fingerprint hash) for the "held by
// pid N (cmd)" refusal/prompt text; StartIdentity is the `ps lstart` the
// pid-reuse guard compares before signalling.
type PortHolder struct {
	Pid           int64
	Pgid          int64
	StartIdentity string
	Command       string
}

// ProbeAdapter is the seam readiness probes go through. nil returns mean "no
// adapter capability / probe could not answer" — callers degrade to
// unavailable, never to "gone".
type ProbeAdapter interface {
	TCP(port uint16) bool
	HTTP(url string) bool
	Container(containerName string) bool
	Tailnet() bool
	// PortInUse: nil means the probe could not answer.
	PortInUse(port uint16) *bool
	// PortHolders: nil means "no adapter capability" (fails closed); an empty
	// non-nil slice means it looked and found nobody.
	PortHolders(port uint16) *[]PortHolder
	// Command backs `{kind:"command"}` readiness. nil (not just false) means
	// "no adapter configured". The context bounds the command; cancelling it
	// kills the probe's process group.
	Command(ctx CommandContext, command *catalog.CommandSpec, cwd *string) *bool
}

// CommandContext bounds a probe/build command's lifetime. Done fires when the
// caller abandons the wait; implementations kill the process group then.
type CommandContext interface {
	Done() <-chan struct{}
}

// SupervisorClock is the time seam.
type SupervisorClock interface {
	NowMillis() int64
	Now() string
	Sleep(millis int64)
}

type SystemClock struct{}

func (SystemClock) NowMillis() int64   { return time.Now().UnixMilli() }
func (SystemClock) Now() string        { return iso8601.FormatMillis(time.Now().UnixMilli()) }
func (SystemClock) Sleep(millis int64) { time.Sleep(time.Duration(millis) * time.Millisecond) }

// SupervisorOptions wires the engine's adapters and budgets.
type SupervisorOptions struct {
	Process            ProcessAdapter
	RunBuild           RunBuild
	Probes             ProbeAdapter
	Preparation        PreparationAdapter // may be nil
	ArtifactInstaller  ArtifactInstaller  // may be nil
	Clock              SupervisorClock
	ReadinessTimeoutMs int64
	ReadinessBackoffMs int64
	TerminationGraceMs int64
	IsClosing          func() bool
}

// ArtifactInstaller runs the idempotent `artifact:` install (marker-checked,
// streams progress through onOutput) and keeps the service's data dir alive.
type ArtifactInstaller interface {
	Install(service *catalog.ServiceDefinition, onOutput OnOutput) error
}

// RunBuild runs a service's build command to completion, forwarding output.
// Cancelling ctx signals the build's process group.
type RunBuild interface {
	Run(command *catalog.ServiceCommand, onOutput OnOutput, cancel <-chan struct{}) error
}

// Host is the manager-side integration surface.
type Host interface {
	InstanceID() string
	Catalog() *catalog.ServiceCatalog
	ServiceStates() []state.ServiceLifecycleState
	// ServiceState fetches one service's state without cloning every other
	// service's — the supervisor asks on every log chunk and probe tick.
	ServiceState(serviceID string) *state.ServiceLifecycleState
	SetServiceState(next *state.ServiceLifecycleState)
	AppendLog(serviceID, data string)
	Publish(eventType string, data map[string]any)
	RecordBackgroundError(scope, err string)
}
