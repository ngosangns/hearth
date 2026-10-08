package daemon

import (
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/manager"
	"github.com/ngosangns/hearth/go/internal/paths"
)

func writeMetadata(t *testing.T, dir, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(paths.MetadataPath(dir)), 0o755); err != nil {
		t.Fatalf("mkdir: %v", err)
	}
	if err := os.WriteFile(paths.MetadataPath(dir), []byte(content), 0o644); err != nil {
		t.Fatalf("write: %v", err)
	}
}

func TestReadLockInstanceIDIsEmptyStringForAMissingLock(t *testing.T) {
	dir := t.TempDir()
	got := ReadLockInstanceID(dir)
	if got == nil || *got != "" {
		t.Fatalf(`expected Some(""), got %v`, got)
	}
}

func TestReadLockInstanceIDIsNilForCorruptJSON(t *testing.T) {
	dir := t.TempDir()
	writeMetadata(t, dir, "not json {{{")
	if got := ReadLockInstanceID(dir); got != nil {
		t.Fatalf("expected nil, got %q", *got)
	}
}

func TestReadLockInstanceIDReadsTheRealInstanceID(t *testing.T) {
	dir := t.TempDir()
	writeMetadata(t, dir, `{"instanceId":"abc-123"}`)
	got := ReadLockInstanceID(dir)
	if got == nil || *got != "abc-123" {
		t.Fatalf("expected abc-123, got %v", got)
	}
}

func TestDaemonLogWritesAndRotates(t *testing.T) {
	dir := t.TempDir()
	log := CreateDaemonLog(dir)
	log("hello")
	content, err := os.ReadFile(filepath.Join(dir, "daemon.log"))
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	if !strings.Contains(string(content), "hello") {
		t.Fatal("expected hello in the log")
	}

	// Force rotation by writing well past the max size directly, then logging
	// once more.
	if err := os.WriteFile(filepath.Join(dir, "daemon.log"), []byte(strings.Repeat("x", 600*1024)), 0o644); err != nil {
		t.Fatalf("write: %v", err)
	}
	log("after rotation")
	if _, err := os.Stat(filepath.Join(dir, "daemon.log.1")); err != nil {
		t.Fatal("expected a rotated copy")
	}
	content, err = os.ReadFile(filepath.Join(dir, "daemon.log"))
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	if !strings.Contains(string(content), "after rotation") {
		t.Fatal("expected the new line in the active log")
	}
	if strings.Contains(string(content), "x") {
		t.Fatal("the rotated content should not still be in the active log")
	}
}

func TestLockOwnershipWatchReportsOnceThenStopsOnTakeover(t *testing.T) {
	dir := t.TempDir()
	writeMetadata(t, dir, `{"instanceId":"me"}`)
	var calls atomic.Int32
	watch := NewLockOwnershipWatch(dir, "me", func() { calls.Add(1) }, nil)

	watch.Check()
	if calls.Load() != 0 {
		t.Fatal("still owns the lock — should not report")
	}

	writeMetadata(t, dir, `{"instanceId":"someone-else"}`)
	watch.Check()
	if calls.Load() != 1 {
		t.Fatalf("expected 1 call, got %d", calls.Load())
	}

	// A second check after the takeover was already reported must not fire
	// again (the watch stops itself on the first loss).
	watch.Check()
	if calls.Load() != 1 {
		t.Fatalf("expected 1 call, got %d", calls.Load())
	}
}

func TestLockOwnershipWatchReportsOnLockFileDisappearing(t *testing.T) {
	dir := t.TempDir()
	writeMetadata(t, dir, `{"instanceId":"me"}`)
	var calls atomic.Int32
	watch := NewLockOwnershipWatch(dir, "me", func() { calls.Add(1) }, nil)

	if err := os.Remove(paths.MetadataPath(dir)); err != nil {
		t.Fatalf("remove: %v", err)
	}
	watch.Check()
	if calls.Load() != 1 {
		t.Fatalf("expected 1 call, got %d", calls.Load())
	}
}

func TestLockOwnershipWatchDoesNotMistakeAnUnreadableLockForALostOne(t *testing.T) {
	dir := t.TempDir()
	writeMetadata(t, dir, `{"instanceId":"me"}`)
	var calls atomic.Int32
	watch := NewLockOwnershipWatch(dir, "me", func() { calls.Add(1) }, nil)

	writeMetadata(t, dir, "not json at all")
	watch.Check()
	if calls.Load() != 0 {
		t.Fatal("an unreadable (not missing) lock file must not be treated as lost")
	}
}

// countingManager is a fake ShutdownManager for the lifecycle memoization test.
type countingManager struct {
	calls atomic.Int32
	done  chan struct{}
	once  sync.Once
}

func (m *countingManager) Shutdown(mode manager.ShutdownMode) {
	m.calls.Add(1)
	m.once.Do(func() { close(m.done) })
}

func (m *countingManager) ShutdownCompletion() <-chan struct{} { return m.done }

func TestDaemonLifecycleShutdownIsMemoized(t *testing.T) {
	mgr := &countingManager{done: make(chan struct{})}
	lifecycle := NewDaemonLifecycle(mgr, manager.LeaveServices)
	lifecycle.Shutdown()
	lifecycle.Shutdown()
	if mgr.calls.Load() != 1 {
		t.Fatalf("concurrent/repeated shutdown calls must only actually shut down once, got %d", mgr.calls.Load())
	}
	lifecycle.WaitForManagerShutdown()
}

// A second daemon for a root that already has a live one must report the
// loser, not a start.
func TestADaemonThatLosesTheLockRaceReportsAlreadyRunning(t *testing.T) {
	dir := t.TempDir()
	options := func() manager.HearthManagerOptions {
		runtimeDirectory := filepath.Join(dir, "runtime")
		root := dir
		guard := false
		return manager.HearthManagerOptions{
			RuntimeDirectory: &runtimeDirectory,
			Root:             &root,
			Catalog: catalog.ServiceCatalog{
				Groups:             map[string][]string{},
				StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
				PrivateFileGuard:   &guard,
			},
		}
	}
	winner, err := manager.Bootstrap(options())
	if err != nil {
		t.Fatalf("bootstrap: %v", err)
	}
	outcome := RunDaemon(options(), manager.LeaveServices)
	if outcome.Kind != DaemonOutcomeAlreadyRunning {
		t.Fatalf("expected AlreadyRunning, got %+v", outcome)
	}
	if outcome.Pid != int64(os.Getpid()) {
		t.Fatalf("expected pid %d, got %d", os.Getpid(), outcome.Pid)
	}
	if outcome.Port != winner.Info().Port {
		t.Fatalf("expected port %d, got %d", winner.Info().Port, outcome.Port)
	}
	winner.Close()
}
