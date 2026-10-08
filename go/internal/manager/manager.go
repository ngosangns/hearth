// HearthManager — ported from rust/crates/hearth-core/src/manager/http/mod.rs.
//
// Ties together the event store (ManagerEventStore), OperationScheduler,
// CursorLogStore, AtomicStateStore, the lock-claim protocol and a
// ProcessSupervisor behind a net/http server on a loopback, OS-assigned port.
// Route handlers live in routes.go; the smp-only /v1/shared/* surface in
// shared_http.go.
//
// Public API for callers (cmd/hearth, tests):
//
//	manager.Bootstrap(opts) (*HearthManager, error)
//	(*HearthManager).Shutdown(mode)            // LeaveServices | StopServices
//	(*HearthManager).ShutdownCompletion() <-chan struct{}
//	(*HearthManager).Router() http.Handler
//	(*HearthManager).Info() state.ManagerInfo
//	(*HearthManager).InstanceID() string
//	(*HearthManager).BearerToken() string
//	(*HearthManager).BaseURL() string
//	(*HearthManager).Catalog() *catalog.ServiceCatalog
//	(*HearthManager).ReloadCatalog(next) (*ReloadOutcome, error)
//	(*HearthManager).Close()                   // leave-services shutdown
//	(*HearthManager).CloseServer()             // stop the HTTP listener (tests/embedders)
//	StopServicesOnDrop{Manager: m}.Stop()      // test/embedder guard (Rust's Drop)
package manager

import (
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/google/uuid"
	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/iso8601"
	"github.com/ngosangns/hearth/go/internal/paths"
	"github.com/ngosangns/hearth/go/internal/platform"
	"github.com/ngosangns/hearth/go/internal/state"
	"github.com/ngosangns/hearth/go/internal/supervisor"
)

const managerMetadataVersion uint32 = 1

func now() string { return iso8601.FormatMillis(time.Now().UnixMilli()) }

// lifecycleEvent is the one `service.lifecycle` payload shape, published once
// per transition by SetServiceState and by the queued-start bookkeeping.
func lifecycleEvent(s *state.ServiceLifecycleState) map[string]any {
	return map[string]any{
		"serviceId":   s.ServiceID,
		"actualState": string(s.ActualState),
		"readiness":   string(s.Readiness),
		"generation":  s.Generation,
		"operationId": s.CurrentOperationID,
	}
}

// defaultDaemonOwned reports whether a catalog service is daemon-owned (i.e.
// not `ownership: external`). A service absent from the catalog counts as
// daemon-owned.
func defaultDaemonOwned(cat *catalog.ServiceCatalog, serviceID string) bool {
	for i := range cat.Services {
		if cat.Services[i].ID == serviceID {
			return cat.Services[i].Ownership == nil || *cat.Services[i].Ownership != catalog.OwnershipExternal
		}
	}
	return true
}

func defaultStateOr(existing *state.ServiceLifecycleState, serviceID, timestamp string) state.ServiceLifecycleState {
	if existing != nil {
		return *existing
	}
	return state.ServiceLifecycleState{
		ServiceID:    serviceID,
		DesiredState: state.DesiredStopped,
		ActualState:  state.ActualStopped,
		Readiness:    state.ReadinessUnknown,
		Generation:   0,
		CreatedAt:    timestamp,
		UpdatedAt:    timestamp,
	}
}

func isActiveState(s state.ActualServiceState) bool {
	for _, v := range supervisor.ActiveStates {
		if v == s {
			return true
		}
	}
	return false
}

func derefOrEmpty(s *string) string {
	if s == nil {
		return ""
	}
	return *s
}

// ---------------------------------------------------------------------------------------------
// HearthManager
// ---------------------------------------------------------------------------------------------

// HearthManagerOptions configures Bootstrap. Every Rust `Option<T>` is a
// pointer (nil = None).
type HearthManagerOptions struct {
	RuntimeDirectory *string
	Root             *string
	Catalog          catalog.ServiceCatalog
	EventCapacity    *int
	LogTailBytes     *uint64
	LogMaxBytes      *uint64
	LogRotationCount *int
	Supervisor       *supervisor.SupervisorOptions
	// Shared is set only for the smp daemon (`hearth smp`) — it enables the
	// /v1/shared/* route surface.
	Shared *SharedContext
}

// HearthManager is the daemon behind the HTTP+SSE surface.
type HearthManager struct {
	instanceID       string
	events           *ManagerEventStore
	operations       *OperationScheduler
	logs             *CursorLogStore
	stateStore       *AtomicStateStore
	runtimeDirectory string
	io               fileio.FileIO
	lock             *LockHandle
	token            string

	catalogMu sync.RWMutex
	catalog   *catalog.ServiceCatalog

	stateMu sync.Mutex
	state   *state.PersistedManagerState

	metadataMu sync.Mutex
	metadata   *state.ManagerMetadata

	closed  atomic.Bool
	closing atomic.Bool

	lifecycle           sync.Mutex
	catalogReloadSerial sync.Mutex

	// persistLock serializes the snapshot→write→fsync sequence so saves stay
	// ordered while the file I/O runs AFTER stateMu is released — an
	// F_FULLFSYNC can cost tens of ms and must not block ServiceStates or the
	// supervisor.
	persistLock sync.Mutex

	// reloadRequests maps requestId → (catalog, response) for recent
	// /v1/manager/reload calls, so a retried reload replays its original answer
	// instead of re-applying (and reporting nothing changed).
	reloadRequestsMu     sync.Mutex
	reloadRequests       []reloadRequestEntry
	reloadRequestsSerial sync.Mutex

	supervisor *supervisor.ProcessSupervisor

	externalSyncMu   sync.Mutex
	externalSyncStop chan struct{}

	shutdownOnce sync.Once
	shutdownDone chan struct{}

	shared *SharedContext

	routes []routeEntry

	serverMu sync.Mutex
	server   *http.Server
}

type reloadRequestEntry struct {
	requestID string
	catalog   any
	response  *reloadResponse
}

// ReloadOutcome reports what a catalog reload did.
type ReloadOutcome struct {
	Stopped []string
	Changed []string
}

// ReloadErrorKind discriminates ReloadError variants.
type ReloadErrorKind int

const (
	// ReloadClosing — the manager is shutting down.
	ReloadClosing ReloadErrorKind = iota
	// ReloadInvalid — the new catalog failed validation.
	ReloadInvalid
	// ReloadStopFailed — removed-but-active services failed to stop.
	ReloadStopFailed
)

// ReloadFailure is one removed service that failed to stop.
type ReloadFailure struct {
	ServiceID string
	Error     string
}

// ReloadError is why a catalog reload was refused. On every variant the
// previous catalog stays in place.
type ReloadError struct {
	Kind     ReloadErrorKind
	Errors   []string
	Failures []ReloadFailure
	Stopped  []string
}

func (e *ReloadError) Error() string {
	switch e.Kind {
	case ReloadClosing:
		return "manager is shutting down"
	case ReloadInvalid:
		return strings.Join(e.Errors, "; ")
	default:
		detail := make([]string, len(e.Failures))
		for i, f := range e.Failures {
			detail[i] = f.ServiceID + ": " + f.Error
		}
		stoppedNote := ""
		if len(e.Stopped) > 0 {
			stoppedNote = "; stopped before the failure: " + strings.Join(e.Stopped, ", ")
		}
		return "catalog not reloaded; removed services failed to stop (" + strings.Join(detail, "; ") + ")" + stoppedNote
	}
}

// reloadErrorToHTTP maps a ReloadError to the HTTP envelope.
func reloadErrorToHTTP(e *ReloadError) *ManagerHttpError {
	switch e.Kind {
	case ReloadClosing:
		return newHTTPError(http.StatusConflict, "manager_closing", "Manager is shutting down")
	case ReloadInvalid:
		return newHTTPError(http.StatusUnprocessableEntity, "invalid_catalog", e.Error())
	default:
		return newHTTPError(http.StatusConflict, "stop_failed", e.Error())
	}
}

// InstanceID is this manager's instance id (the Host interface method).
func (m *HearthManager) InstanceID() string { return m.instanceID }

// BearerToken is the token every /v1 route and /healthz's instanceId require.
func (m *HearthManager) BearerToken() string { return m.token }

// RuntimeDirectory is the manager's runtime directory.
func (m *HearthManager) RuntimeDirectory() string { return m.runtimeDirectory }

// Events exposes the event store (for embedders/tests).
func (m *HearthManager) Events() *ManagerEventStore { return m.events }

// Operations exposes the operation scheduler (for embedders/tests).
func (m *HearthManager) Operations() *OperationScheduler { return m.operations }

// Logs exposes the cursor log store (for embedders/tests).
func (m *HearthManager) Logs() *CursorLogStore { return m.logs }

// Info returns the manager's metadata.
func (m *HearthManager) Info() state.ManagerInfo {
	m.metadataMu.Lock()
	md := m.metadata
	m.metadataMu.Unlock()
	if md == nil {
		panic("manager has not started")
	}
	return state.ManagerInfo{
		ProtocolVersion:  md.ProtocolVersion,
		InstanceID:       md.InstanceID,
		Pid:              md.Pid,
		Port:             md.Port,
		StartedAt:        md.StartedAt,
		MetadataVersion:  md.Version,
		RuntimeDirectory: m.runtimeDirectory,
	}
}

// BaseURL is the loopback base URL the manager is serving on.
func (m *HearthManager) BaseURL() string {
	m.metadataMu.Lock()
	md := m.metadata
	m.metadataMu.Unlock()
	if md == nil {
		panic("manager has not started")
	}
	return fmt.Sprintf("http://127.0.0.1:%d", md.Port)
}

// Catalog returns the current catalog. The pointer is replaced (never mutated
// in place) on reload, so a returned pointer stays valid.
func (m *HearthManager) Catalog() *catalog.ServiceCatalog {
	m.catalogMu.RLock()
	defer m.catalogMu.RUnlock()
	return m.catalog
}

func (m *HearthManager) lifecycleGeneration(serviceID string) uint64 {
	m.stateMu.Lock()
	defer m.stateMu.Unlock()
	if st, ok := m.state.Services[serviceID]; ok {
		return st.Generation
	}
	return 0
}

// ReloadCatalog swaps in a new catalog after validating it. A service removed
// from the new catalog that is currently active gets stopped first, using the
// *old* catalog (the supervisor needs the old definition to know how to stop
// it) — only then does the swap happen, so a removed-but-still-stopping
// service is never briefly invisible from ServiceStates()/`/v1/services` while
// its process is still alive. `external`-owned removed services are left alone.
// A service that stays present but whose definition changed is left running
// as-is and reported in `changed`. A removed service that fails to stop aborts
// the whole reload (ReloadStopFailed) and the old catalog stays — a stop must
// never be reported without stopping something.
//
// Serialized against itself (not m.lifecycle, which supervisor.Stop's own
// state writes run through — nesting into that from here would deadlock).
func (m *HearthManager) ReloadCatalog(next catalog.ServiceCatalog) (*ReloadOutcome, error) {
	m.catalogReloadSerial.Lock()
	defer m.catalogReloadSerial.Unlock()
	if m.closing.Load() {
		return nil, &ReloadError{Kind: ReloadClosing}
	}
	validation := catalog.ValidateCatalog(&next)
	if len(validation.Errors) > 0 {
		return nil, &ReloadError{Kind: ReloadInvalid, Errors: validation.Errors}
	}
	previous := m.Catalog()
	nextIDs := make(map[string]bool, len(next.Services))
	for i := range next.Services {
		nextIDs[next.Services[i].ID] = true
	}
	removedIDs := []string{}
	for i := range previous.Services {
		if !nextIDs[previous.Services[i].ID] {
			removedIDs = append(removedIDs, previous.Services[i].ID)
		}
	}
	changed := []string{}
	for i := range previous.Services {
		s := previous.Services[i]
		for j := range next.Services {
			if next.Services[j].ID == s.ID {
				if !reflect.DeepEqual(next.Services[j], s) {
					changed = append(changed, s.ID)
				}
				break
			}
		}
	}

	stopped := []string{}
	var failures []ReloadFailure
	for _, serviceID := range removedIDs {
		if !defaultDaemonOwned(previous, serviceID) {
			continue
		}
		m.stateMu.Lock()
		st := m.state.Services[serviceID]
		m.stateMu.Unlock()
		if st == nil || !isActiveState(st.ActualState) {
			continue
		}
		if err := m.supervisor.Stop(serviceID, nil); err != nil {
			failures = append(failures, ReloadFailure{ServiceID: serviceID, Error: err.Error()})
		} else {
			stopped = append(stopped, serviceID)
		}
	}
	if len(failures) > 0 {
		return nil, &ReloadError{Kind: ReloadStopFailed, Failures: failures, Stopped: stopped}
	}
	hasExternal := false
	for i := range next.Services {
		if next.Services[i].Ownership != nil && *next.Services[i].Ownership == catalog.OwnershipExternal {
			hasExternal = true
			break
		}
	}
	m.catalogMu.Lock()
	m.catalog = &next
	m.catalogMu.Unlock()
	m.events.Publish("manager.catalog-reloaded", map[string]any{
		"removed": removedIDs,
		"changed": changed,
		"stopped": stopped,
	})
	if hasExternal {
		// The first `ownership: external` service can arrive by reload (a new
		// `shared:` entry) — adopt it now rather than on the next 2s tick.
		// Spawned, not awaited: the pass takes each per-service lock in turn,
		// and a slow probe or an in-flight attach would otherwise stall the
		// reload response past the client's manager timeout.
		go m.supervisor.SyncExternalServices()
	}
	return &ReloadOutcome{Stopped: stopped, Changed: changed}, nil
}

// persist snapshots the current state and writes it. The snapshot is taken and
// written under persistLock so consecutive saves stay ordered, and the
// write+fsync never runs inside stateMu. A failed save surfaces through
// `manager.error` instead of stopping persistence silently.
func (m *HearthManager) persist() {
	m.persistLock.Lock()
	defer m.persistLock.Unlock()
	m.stateMu.Lock()
	snapshot := state.PersistedManagerState{
		Version:  m.state.Version,
		Services: make(map[string]*state.ServiceLifecycleState, len(m.state.Services)),
	}
	for k, v := range m.state.Services {
		snapshot.Services[k] = v
	}
	m.stateMu.Unlock()
	if err := m.stateStore.Save(&snapshot); err != nil {
		m.RecordBackgroundError("state-store", err.Error())
	}
}

// ShutdownCompletion returns a channel closed when the manager has finished
// shutting down. Unlike Rust's `watch` value, closing a channel broadcasts to
// every current AND future receiver — a subscriber that arrives after the
// shutdown already finished still observes completion.
func (m *HearthManager) ShutdownCompletion() <-chan struct{} { return m.shutdownDone }

// Close is a leave-services shutdown.
func (m *HearthManager) Close() { m.Shutdown(LeaveServices) }

// Shutdown shuts the manager down. StopServices stops managed processes first;
// LeaveServices detaches them for the next daemon to re-adopt (also what
// SIGTERM / `hearth manager restart` use once past any refuse-if-active guard).
func (m *HearthManager) Shutdown(mode ShutdownMode) {
	m.closing.Store(true)
	m.operations.CloseMutations()
	m.supervisor.BeginShutdown()
	m.operations.DrainServices()
	if mode.StopsServices() {
		m.supervisor.Shutdown()
	} else {
		// Services stay up; the `docker logs` followers this daemon spawned for
		// them do not.
		m.supervisor.DetachAllOutput()
	}
	m.lifecycle.Lock()
	defer m.lifecycle.Unlock()
	m.closeLocked()
}

func (m *HearthManager) closeLocked() {
	if m.closed.Swap(true) {
		return
	}
	m.externalSyncMu.Lock()
	if m.externalSyncStop != nil {
		close(m.externalSyncStop)
		m.externalSyncStop = nil
	}
	m.externalSyncMu.Unlock()
	releasePrepared := PrepareOwnedLockRelease(m.io, m.lock, m.token)
	m.events.Publish("manager.stopped", map[string]any{"instanceId": m.instanceID})
	if releasePrepared {
		ReleaseOwnedLock(m.io, m.lock, m.token)
	}
	m.shutdownOnce.Do(func() { close(m.shutdownDone) })
}

// CloseServer stops the HTTP listener. Rust's axum server dies with the
// process; a Go embedder/test needs an explicit hook so the port is released.
// It is deliberately NOT called from closeLocked: doing so would race the
// `202 Accepted` response to POST /v1/manager/shutdown.
func (m *HearthManager) CloseServer() {
	m.serverMu.Lock()
	server := m.server
	m.server = nil
	m.serverMu.Unlock()
	if server != nil {
		_ = server.Close()
	}
}

// ---------------------------------------------------------------------------------------------
// Host implementation (supervisor.Host)
// ---------------------------------------------------------------------------------------------

// HearthManager is the supervisor's Host.
var _ supervisor.Host = (*HearthManager)(nil)

// ServiceStates returns every catalog service's lifecycle row, defaulting a
// missing row to `stopped`.
func (m *HearthManager) ServiceStates() []state.ServiceLifecycleState {
	timestamp := now()
	cat := m.Catalog()
	m.stateMu.Lock()
	defer m.stateMu.Unlock()
	out := make([]state.ServiceLifecycleState, 0, len(cat.Services))
	for i := range cat.Services {
		out = append(out, defaultStateOr(m.state.Services[cat.Services[i].ID], cat.Services[i].ID, timestamp))
	}
	return out
}

// ServiceState fetches one service's state without cloning every other
// service's — the supervisor asks on every log chunk and probe tick.
func (m *HearthManager) ServiceState(serviceID string) *state.ServiceLifecycleState {
	cat := m.Catalog()
	var def *catalog.ServiceDefinition
	for i := range cat.Services {
		if cat.Services[i].ID == serviceID {
			def = &cat.Services[i]
			break
		}
	}
	if def == nil {
		return nil
	}
	timestamp := now()
	m.stateMu.Lock()
	defer m.stateMu.Unlock()
	st := defaultStateOr(m.state.Services[def.ID], def.ID, timestamp)
	return &st
}

// SetServiceState records a supervisor transition and publishes the one
// `service.lifecycle` event for it.
func (m *HearthManager) SetServiceState(next *state.ServiceLifecycleState) {
	m.lifecycle.Lock()
	defer m.lifecycle.Unlock()
	if m.closed.Load() {
		return
	}
	event := lifecycleEvent(next)
	m.stateMu.Lock()
	m.state.Services[next.ServiceID] = next
	m.stateMu.Unlock()
	m.persist()
	m.events.Publish("service.lifecycle", event)
}

// AppendLog appends a service log chunk and publishes `service.log`.
func (m *HearthManager) AppendLog(serviceID, data string) {
	if m.closed.Load() {
		return
	}
	if err := m.logs.Append(serviceID, data); err != nil {
		m.RecordBackgroundError("service-log:"+serviceID, err.Error())
		return
	}
	m.events.Publish("service.log", map[string]any{"serviceId": serviceID})
}

// Publish emits an event with an object payload.
func (m *HearthManager) Publish(eventType string, data map[string]any) {
	m.events.Publish(eventType, data)
}

// RecordBackgroundError emits a `manager.error` event — fire-and-forget work
// must never panic into the daemon.
func (m *HearthManager) RecordBackgroundError(scope, err string) {
	m.events.Publish("manager.error", map[string]any{"scope": scope, "message": err})
}

// ---------------------------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------------------------

// BootstrapError is Bootstrap's error. ClaimLock is set when the lock-claim
// protocol refused (e.g. another live manager already serves this root).
type BootstrapError struct {
	Message   string
	ClaimLock *ClaimLockError
}

func (e *BootstrapError) Error() string {
	if e.ClaimLock != nil {
		return e.ClaimLock.Error()
	}
	return e.Message
}

// Bootstrap boots a HearthManager: validates the catalog, claims the runtime
// lock, wires every store and the supervisor, binds a loopback port and starts
// serving. The caller owns the returned manager and must Shutdown it.
func Bootstrap(options HearthManagerOptions) (*HearthManager, error) {
	plat := platform.CurrentPlatform()
	if !platform.IsSupportedHearthPlatform(plat) {
		return nil, &BootstrapError{Message: platform.UnsupportedPlatformMessage(plat)}
	}
	validation := catalog.ValidateCatalog(&options.Catalog)
	if len(validation.Errors) > 0 {
		return nil, &BootstrapError{Message: "Invalid service catalog: " + strings.Join(validation.Errors, "; ")}
	}
	root := ""
	if options.Root != nil {
		root = *options.Root
	} else {
		cwd, err := os.Getwd()
		if err != nil {
			return nil, &BootstrapError{Message: "cannot resolve the current directory: " + err.Error()}
		}
		root = cwd
	}
	runtimeDirectory := ""
	if options.RuntimeDirectory != nil {
		runtimeDirectory = *options.RuntimeDirectory
	} else {
		runtimeDirectory = paths.ResolveRuntimeDirectory(root, derefOrEmpty(options.Catalog.RuntimeDirectory))
	}
	io := fileio.New(options.Catalog.PrivateFileGuard == nil || *options.Catalog.PrivateFileGuard)
	token := RandomToken()
	httpClient := &http.Client{}
	bootstrapMetadata := state.ManagerMetadata{
		Version:         managerMetadataVersion,
		ProtocolVersion: state.ProtocolVersion,
		InstanceID:      uuid.NewString(),
		Pid:             int64(os.Getpid()),
		Port:            0,
		StartedAt:       now(),
	}
	lock, err := ClaimLock(io, runtimeDirectory, &bootstrapMetadata, token, NewHTTPHealthcheck(httpClient))
	if err != nil {
		var cle *ClaimLockError
		if errors.As(err, &cle) {
			return nil, &BootstrapError{ClaimLock: cle}
		}
		return nil, &BootstrapError{Message: err.Error()}
	}

	events := NewEventStore(derefInt(options.EventCapacity), bootstrapMetadata.InstanceID)
	operations := NewOperationScheduler(events)
	logs := NewCursorLogStore(io, filepath.Join(runtimeDirectory, "logs"), derefU64(options.LogTailBytes), derefU64(options.LogMaxBytes), derefInt(options.LogRotationCount))
	stateStore := NewAtomicStateStore(io, runtimeDirectory)
	stateStore.ReportError = func(message string) {
		events.Publish("manager.error", map[string]any{"scope": "state-store", "message": message})
	}

	manager := &HearthManager{
		instanceID:       bootstrapMetadata.InstanceID,
		events:           events,
		operations:       operations,
		logs:             logs,
		stateStore:       stateStore,
		runtimeDirectory: runtimeDirectory,
		io:               io,
		lock:             lock,
		token:            token,
		catalog:          &options.Catalog,
		state:            &state.PersistedManagerState{Version: state.StateVersion, Services: map[string]*state.ServiceLifecycleState{}},
		shutdownDone:     make(chan struct{}),
		shared:           options.Shared,
	}

	var supervisorOptions supervisor.SupervisorOptions
	if options.Supervisor != nil {
		supervisorOptions = *options.Supervisor
	} else {
		supervisorOptions = supervisor.DefaultSupervisorOptions(root, runtimeDirectory, nil)
	}
	supervisorOptions.IsClosing = func() bool { return manager.closing.Load() }
	manager.supervisor = supervisor.NewProcessSupervisor(manager, supervisorOptions)
	manager.routes = manager.buildRoutes()

	bootstrapErr := func() error {
		loaded := stateStore.Load()
		manager.stateMu.Lock()
		manager.state = loaded
		manager.stateMu.Unlock()
		manager.supervisor.Reconcile()

		listener, err := net.Listen("tcp", "127.0.0.1:0")
		if err != nil {
			return err
		}
		port := listener.Addr().(*net.TCPAddr).Port
		server := &http.Server{Handler: manager.Router()}
		manager.serverMu.Lock()
		manager.server = server
		manager.serverMu.Unlock()
		go func() { _ = server.Serve(listener) }()

		finalMetadata := bootstrapMetadata
		finalMetadata.Port = uint16(port)
		manager.metadataMu.Lock()
		manager.metadata = &finalMetadata
		manager.metadataMu.Unlock()
		key := ReadLockOwnershipKey(io, runtimeDirectory)
		if key == nil {
			return errors.New("Missing ownership key")
		}
		metadataJSON, err := marshalNoEscape(finalMetadata)
		if err != nil {
			return err
		}
		if err := io.WriteFile(lock.MetadataPath, string(metadataJSON)); err != nil {
			return err
		}
		proof := CreateLockOwnershipProof(*key, &finalMetadata, token)
		proofJSON, err := marshalNoEscape(&proof)
		if err != nil {
			return err
		}
		if err := io.WriteFile(lock.ProofPath, string(proofJSON)); err != nil {
			return err
		}
		manager.events.Publish("manager.started", map[string]any{"instanceId": manager.instanceID})

		// The adoption loop runs unconditionally: a catalog with no
		// `ownership: external` service makes each tick a no-op, and the first
		// such service may only arrive later by reload.
		manager.supervisor.SyncExternalServices()
		stop := make(chan struct{})
		manager.externalSyncMu.Lock()
		manager.externalSyncStop = stop
		manager.externalSyncMu.Unlock()
		go func() {
			ticker := time.NewTicker(2 * time.Second)
			defer ticker.Stop()
			for {
				select {
				case <-stop:
					return
				case <-ticker.C:
					manager.supervisor.SyncExternalServices()
				}
			}
		}()
		return nil
	}()
	if bootstrapErr != nil {
		PrepareOwnedLockRelease(io, lock, token)
		ReleaseOwnedLock(io, lock, token)
		return nil, &BootstrapError{Message: bootstrapErr.Error()}
	}
	return manager, nil
}

func derefInt(v *int) int {
	if v == nil {
		return 0
	}
	return *v
}

func derefU64(v *uint64) uint64 {
	if v == nil {
		return 0
	}
	return *v
}

// ---------------------------------------------------------------------------------------------
// StopServicesOnDrop
// ---------------------------------------------------------------------------------------------

// StopServicesOnDrop is the Go equivalent of Rust's `Drop` guard. Go has no
// destructors, so callers (tests, embedders) must call Stop — typically via
// `defer`. It runs a StopServices shutdown bounded to 15s, then SIGKILLs the
// group of any recorded POSIX identity whose leader still shows its recorded
// start time, so a reused pid is never signalled.
type StopServicesOnDrop struct {
	Manager *HearthManager
}

// Stop runs the bounded stop-services shutdown and the group kill.
func (s *StopServicesOnDrop) Stop() {
	if s.Manager == nil {
		return
	}
	done := make(chan struct{})
	go func() {
		s.Manager.Shutdown(StopServices)
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(15 * time.Second):
	}
	for _, st := range s.Manager.ServiceStates() {
		if st.Identity != nil && !st.Identity.IsDocker() {
			killGroupIfSameProcess(st.Identity.PidValue(), st.Identity.PgidValue(), st.Identity.StartIdentityValue())
		}
	}
}

func killGroupIfSameProcess(pid, pgid int64, startIdentity string) {
	if pid <= 1 || pgid <= 1 || startIdentity == "" {
		return
	}
	out, err := exec.Command("ps", "-o", "lstart=", "-p", strconv.FormatInt(pid, 10)).Output()
	if err != nil {
		return
	}
	if strings.TrimSpace(string(out)) == startIdentity {
		// Plain syscall; the group was just verified to still hold the recorded
		// process.
		_ = syscall.Kill(-int(pgid), syscall.SIGKILL)
	}
}
