// Package daemon ports rust/crates/hearth-core/src/daemon.rs: the daemon-*process*
// glue around a HearthManager — diagnostics logging, the losing-side
// lock-takeover watch, and RunDaemon's signal and panic wiring.
package daemon

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"runtime/debug"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unicode/utf8"

	"github.com/ngosangns/hearth/go/internal/iso8601"
	"github.com/ngosangns/hearth/go/internal/manager"
	"github.com/ngosangns/hearth/go/internal/paths"
	"github.com/ngosangns/hearth/go/internal/state"
)

const (
	daemonLogMaxBytes = 512 * 1024
	// DaemonLogName is the diagnostics log file name inside the runtime
	// directory. Rust's `pub(crate)`; the manager HTTP surface reads the same
	// file but cannot import this package (it would cycle back through
	// manager.Bootstrap), so it keeps its own copy of the name.
	DaemonLogName     = "daemon.log"
	lockWatchInterval = 2 * time.Second
)

// DaemonLog is the daemon-level diagnostics sink.
type DaemonLog func(message string)

// CreateDaemonLog returns the daemon-level diagnostics sink. Whoever spawns a
// daemon usually detaches it with stdio ignored, so without this file a crash
// (or a refused duplicate) leaves no trace at all. Keeps one rotated copy.
func CreateDaemonLog(runtimeDirectory string) DaemonLog {
	path := filepath.Join(runtimeDirectory, DaemonLogName)
	rotated := filepath.Join(runtimeDirectory, DaemonLogName+".1")
	return func(message string) {
		if err := os.MkdirAll(runtimeDirectory, 0o755); err != nil {
			return
		}
		var size int64
		if info, err := os.Stat(path); err == nil {
			size = info.Size()
		}
		if size > daemonLogMaxBytes {
			_ = os.Remove(rotated)
			if err := os.Rename(path, rotated); err != nil {
				return
			}
		}
		file, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600)
		if err != nil {
			return
		}
		defer file.Close()
		_ = file.Chmod(0o600)
		_, _ = fmt.Fprintf(file, "%s %s\n", iso8601.Now(), message)
	}
}

// ReadLockInstanceID returns the lock's owner instance id: a pointer to "" when
// the lock file is gone (quarantined or released), nil when it exists but could
// not be read/parsed — a transient read failure must not be mistaken for losing
// the lock — and a pointer to the id otherwise.
func ReadLockInstanceID(runtimeDirectory string) *string {
	data, err := os.ReadFile(paths.MetadataPath(runtimeDirectory))
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			empty := ""
			return &empty
		}
		return nil
	}
	if !utf8.Valid(data) {
		return nil
	}
	var value any
	if err := json.Unmarshal(data, &value); err != nil {
		return nil
	}
	id := ""
	if obj, ok := value.(map[string]any); ok {
		if s, ok := obj["instanceId"].(string); ok {
			id = s
		}
	}
	return &id
}

// LockOwnershipWatch keeps the "one daemon per runtime directory" invariant
// enforced from the losing side. A daemon whose lock was taken over must stop
// managing services: two live daemons would otherwise fight over one state
// file. It never touches the winner's lock — it just leaves the field.
type LockOwnershipWatch struct {
	runtimeDirectory string
	instanceID       string
	onLockLost       func()
	interval         time.Duration
	stopped          atomic.Bool
	mu               sync.Mutex
	stopCh           chan struct{}
	started          bool
}

func NewLockOwnershipWatch(runtimeDirectory, instanceID string, onLockLost func(), interval *time.Duration) *LockOwnershipWatch {
	d := lockWatchInterval
	if interval != nil {
		d = *interval
	}
	return &LockOwnershipWatch{
		runtimeDirectory: runtimeDirectory,
		instanceID:       instanceID,
		onLockLost:       onLockLost,
		interval:         d,
	}
}

func (w *LockOwnershipWatch) Start() {
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.stopped.Load() || w.started {
		return
	}
	w.started = true
	stopCh := make(chan struct{})
	w.stopCh = stopCh
	go func() {
		defer RecoverPanic()
		ticker := time.NewTicker(w.interval)
		defer ticker.Stop()
		for {
			select {
			case <-stopCh:
				return
			case <-ticker.C:
				w.Check()
			}
		}
	}()
}

func (w *LockOwnershipWatch) Stop() {
	w.stopped.Store(true)
	w.mu.Lock()
	defer w.mu.Unlock()
	if w.stopCh != nil {
		close(w.stopCh)
		w.stopCh = nil
	}
}

func (w *LockOwnershipWatch) Check() {
	if w.stopped.Load() {
		return
	}
	owner := ReadLockInstanceID(w.runtimeDirectory)
	if owner == nil {
		return // unreadable — unknown, not lost
	}
	if *owner == w.instanceID {
		return
	}
	w.Stop()
	w.onLockLost()
}

// ShutdownManager is the slice of the manager the daemon lifecycle drives. The
// concrete *manager.HearthManager satisfies it.
type ShutdownManager interface {
	Shutdown(mode manager.ShutdownMode)
	ShutdownCompletion() <-chan struct{}
}

// managerHandle is the manager surface RunDaemon needs beyond the documented
// bootstrap API: the instance id and the bound port. Kept as a local interface
// so daemon.go names exactly what it depends on; the concrete
// *manager.HearthManager satisfies it.
type managerHandle interface {
	ShutdownManager
	InstanceID() string
	Info() state.ManagerInfo
}

type DaemonLifecycle struct {
	manager ShutdownManager
	mode    manager.ShutdownMode
	once    sync.Once
}

func NewDaemonLifecycle(mgr ShutdownManager, mode manager.ShutdownMode) *DaemonLifecycle {
	return &DaemonLifecycle{manager: mgr, mode: mode}
}

// Shutdown is triggered by SIGINT/SIGTERM. Default ShutdownMode LeaveServices
// leaves already-running services alone — they're detached processes that
// outlive this daemon and get reconciled/re-adopted by the next one — so
// interrupting the daemon never kills a developer's in-flight work. Memoized:
// concurrent/repeated calls only actually shut down once.
func (l *DaemonLifecycle) Shutdown() {
	l.once.Do(func() {
		l.manager.Shutdown(l.mode)
	})
}

func (l *DaemonLifecycle) WaitForManagerShutdown() {
	<-l.manager.ShutdownCompletion()
}

var (
	panicLogOnce sync.Once
	panicLogMu   sync.RWMutex
	panicLog     DaemonLog
)

// InstallPanicLog records the diagnostics sink RecoverPanic writes to.
//
// Rust installs a process-wide panic hook; Go has no such hook, so the daemon's
// spawned goroutines must `defer RecoverPanic()` instead. Installed once per
// process.
func InstallPanicLog(log DaemonLog) {
	panicLogOnce.Do(func() {
		panicLogMu.Lock()
		panicLog = log
		panicLogMu.Unlock()
	})
}

// RecoverPanic routes a recovered panic's message and location into daemon.log,
// then swallows it so the daemon keeps running — the Go analogue of a panic
// unwinding one tokio task while the runtime survives. Use as
// `defer RecoverPanic()` in a spawned goroutine.
func RecoverPanic() {
	recovered := recover()
	if recovered == nil {
		return
	}
	message := fmt.Sprintf("panic in goroutine: %v\n%s", recovered, debug.Stack())
	panicLogMu.RLock()
	log := panicLog
	panicLogMu.RUnlock()
	if log != nil {
		log(message)
	}
	_, _ = fmt.Fprintln(os.Stderr, message)
}

// DaemonOutcomeKind tags how a RunDaemon call ended.
type DaemonOutcomeKind int

const (
	// DaemonOutcomeStopped — served until shutdown, then closed cleanly.
	DaemonOutcomeStopped DaemonOutcomeKind = iota
	// DaemonOutcomeAlreadyRunning — another daemon already holds this root's
	// lock and answers its health check, so this one never started. The caller
	// exits non-zero and says so: it used to exit 0 with only a `daemon.log`
	// line, which read as a successful start.
	DaemonOutcomeAlreadyRunning
	// DaemonOutcomeFailed — bootstrap failed; the reason is in `daemon.log`.
	DaemonOutcomeFailed
)

// DaemonOutcome is how a RunDaemon call ended. Pid and Port are set only for
// DaemonOutcomeAlreadyRunning.
type DaemonOutcome struct {
	Kind DaemonOutcomeKind
	Pid  int64
	Port uint16
}

// alreadyRunning extracts the pid and port from a bootstrap error that raced
// another daemon (Rust: BootstrapError::ClaimLock(ClaimLockError::AlreadyRunning)).
func alreadyRunning(err error) (*state.ManagerMetadata, uint16, bool) {
	var bootstrapErr *manager.BootstrapError
	if !errors.As(err, &bootstrapErr) ||
		bootstrapErr.ClaimLock == nil ||
		!bootstrapErr.ClaimLock.AlreadyRunning ||
		bootstrapErr.ClaimLock.Metadata == nil {
		return nil, 0, false
	}
	return bootstrapErr.ClaimLock.Metadata, bootstrapErr.ClaimLock.Port, true
}

// RunDaemon boots a HearthManager, wires SIGINT/SIGTERM to a graceful
// DaemonLifecycle shutdown, and returns once the manager has fully closed. A
// raced ClaimLockError::AlreadyRunning (another daemon won the lock claim
// first) is DaemonOutcomeAlreadyRunning.
func RunDaemon(options manager.HearthManagerOptions, mode manager.ShutdownMode) DaemonOutcome {
	root := options.Root
	if root == nil {
		cwd, err := os.Getwd()
		if err != nil {
			panic(err) // Rust: std::env::current_dir().unwrap()
		}
		root = &cwd
	}
	runtimeDirectory := options.RuntimeDirectory
	if runtimeDirectory == nil {
		catalogRuntimeDir := ""
		if options.Catalog.RuntimeDirectory != nil {
			catalogRuntimeDir = *options.Catalog.RuntimeDirectory
		}
		rd := paths.ResolveRuntimeDirectory(*root, catalogRuntimeDir)
		runtimeDirectory = &rd
	}
	log := CreateDaemonLog(*runtimeDirectory)
	InstallPanicLog(log)

	bootstrapped, err := manager.Bootstrap(options)
	if err != nil {
		if metadata, port, ok := alreadyRunning(err); ok {
			log(fmt.Sprintf("bootstrap raced another daemon: pid %d already serves this root on port %d", metadata.Pid, port))
			return DaemonOutcome{Kind: DaemonOutcomeAlreadyRunning, Pid: metadata.Pid, Port: port}
		}
		log(fmt.Sprintf("bootstrap failed: %v", err))
		return DaemonOutcome{Kind: DaemonOutcomeFailed}
	}
	var mgr managerHandle = bootstrapped
	log(fmt.Sprintf("listening on 127.0.0.1:%d, root=%s", mgr.Info().Port, *root))

	lifecycle := NewDaemonLifecycle(mgr, mode)

	var lockWatch *LockOwnershipWatch
	if owner := ReadLockInstanceID(*runtimeDirectory); owner != nil && *owner == mgr.InstanceID() {
		lockWatch = NewLockOwnershipWatch(*runtimeDirectory, mgr.InstanceID(), func() {
			log("shutdown: manager lock is owned by another daemon")
			go func() {
				defer RecoverPanic()
				lifecycle.Shutdown()
			}()
		}, nil)
		lockWatch.Start()
	}

	go func() {
		defer RecoverPanic()
		signals := make(chan os.Signal, 1)
		signal.Notify(signals, syscall.SIGINT, syscall.SIGTERM)
		<-signals
		if lockWatch != nil {
			lockWatch.Stop()
		}
		log("shutdown: signal")
		lifecycle.Shutdown()
	}()

	// A detached daemon already runs in its own session with no controlling
	// terminal, so a terminal hangup should never reach it — but an explicit
	// ignore means a stray SIGHUP can never fall back to the default
	// terminate-the-process behavior either.
	go func() {
		sighup := make(chan os.Signal, 1)
		signal.Notify(sighup, syscall.SIGHUP)
		for range sighup {
		}
	}()

	lifecycle.WaitForManagerShutdown()
	return DaemonOutcome{Kind: DaemonOutcomeStopped}
}
