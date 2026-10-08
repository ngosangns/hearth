// Package client is the unified HTTP+SSE client a Go CLI, MCP server or app
// uses to talk to a hearth daemon.
//
// Ported from rust/crates/hearth-cli/src/client.rs (the typed per-endpoint
// `Client` and the caching `ManagerClient`), rust/crates/hearth-cli/src/lib.rs
// (discovery, the shared `request`, `encode_path_segment`, `operation_id` and
// the `ensure`/`require_client_for` connection lifecycle), and the daemon
// client halves of rust/crates/hearth-tui/src/client.rs (the SSE reconnect
// loop, the 64 KiB frame cap, `daemon_log`/`daemon_pid`) and
// rust/crates/hearth-mcp/src/client.rs (the operation/log/event primitives its
// tools build on, and the shared-services helpers).
//
// Surface:
//
//   - Connection lifecycle: Discover, RequireClientFor, Ensure, NewClient,
//     Options, Discovery, BootstrappingDaemon.
//   - One discovered connection: Client with ManagerInfo, Services, URLs,
//     Catalog, Log, DaemonLog, Operation, Submit, BulkStart, Wait,
//     EventStream, Events, Watch, Request, RequestWithTimeout, EnsurePayload.
//   - Cached connection: ManagerClient, which re-discovers only after a
//     transport or authentication failure (IsConnectionFailure).
//   - Shared services (smp): SMPCatalog, DiscoverSMP, EnsureSMP, SMPRequest,
//     SMPRequestSlow, SharedInstance, SharedAttach, SharedDetach, SharedProbe,
//     SharedRemove, SharedInstall, SharedInstances, SharedConnection.
//   - Helpers: EncodePathSegment, OperationID, NewRequestID, IsConnectionFailure.
//
// Every daemon-talking method takes a context.Context first. A request is
// bounded by DefaultRequestTimeout (10s) unless it goes through
// RequestWithTimeout with a nil timeout — the shared attach/install path can
// legitimately take minutes, and the SSE stream is never bounded. Errors are
// *Error values whose Kind callers match on (or errors.Is against the
// sentinels).
package client

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/manager"
	"github.com/ngosangns/hearth/go/internal/paths"
	"github.com/ngosangns/hearth/go/internal/platform"
	"github.com/ngosangns/hearth/go/internal/state"
)

const (
	// DefaultRequestTimeout bounds one manager request's transport, matching
	// the Rust MANAGER_REQUEST_TIMEOUT. The SSE stream is never bounded by it.
	DefaultRequestTimeout = 10 * time.Second
	// operationPollInterval is the Rust OPERATION_POLL_INTERVAL.
	operationPollInterval = 100 * time.Millisecond
	// ensureSpawnWait bounds a freshly spawned daemon that has not claimed the
	// lock yet; ensureBootstrapWait bounds one that holds the lock and is still
	// bootstrapping (it records port 0 until it has re-adopted every service).
	ensureSpawnWait     = 5 * time.Second
	ensureBootstrapWait = 120 * time.Second
)

// sharedHTTPClient is the one HTTP client every request shares, so polls reuse
// pooled connections. Timeouts are per request, never here: the SSE stream
// must stay open indefinitely.
var sharedHTTPClient HTTPDoer = &http.Client{}

func (c *Client) httpDoer() HTTPDoer {
	if c.Doer != nil {
		return c.Doer
	}
	return sharedHTTPClient
}

// newFileIO builds the file-access strategy discovery uses: guarded unless the
// catalog explicitly disables the private-file guard.
func newFileIO(cat *catalog.ServiceCatalog) fileio.FileIO {
	guarded := true
	if cat != nil && cat.PrivateFileGuard != nil {
		guarded = *cat.PrivateFileGuard
	}
	return fileio.New(guarded)
}

func runtimeDirOf(cat *catalog.ServiceCatalog) string {
	if cat != nil && cat.RuntimeDirectory != nil {
		return *cat.RuntimeDirectory
	}
	return ""
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

func unavailableError(message string) *Error {
	return &Error{Kind: KindUnavailable, Message: message, ExitCode: ExitUnavailable}
}

func protocolError() *Error {
	return &Error{Kind: KindProtocol, Message: "hearth manager protocol is incompatible", ExitCode: ExitProtocol}
}

func unauthorizedError() *Error {
	return &Error{Kind: KindUnauthorized, Message: "hearth manager authentication failed", ExitCode: ExitUnauthorized}
}

func malformedError(err error) *Error {
	return &Error{Kind: KindMalformed, Message: err.Error(), ExitCode: ExitFailed}
}

func usageError(message string) *Error {
	return &Error{Kind: KindUsage, Message: message, ExitCode: ExitUsage}
}

// IsConnectionFailure reports whether err says the connection itself is gone —
// the daemon went away, restarted on another port, or rotated its token — as
// opposed to an API-level rejection. ManagerClient invalidates its cache and
// retries once on exactly these.
func IsConnectionFailure(err error) bool {
	var e *Error
	if !errors.As(err, &e) {
		return false
	}
	switch e.Kind {
	case KindUnavailable, KindTimeout:
		return true
	case KindHTTP:
		// A port the dead daemon used to hold answering without the hearth
		// error envelope means something else took it over; the cache entry is
		// worthless either way.
		return e.Code == "unauthorized" || e.Code == "request_failed"
	}
	return false
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

// validMetadata mirrors the Rust client's `valid_metadata` (note: it does not
// check the protocol version — that is a separate Discovery outcome).
func validMetadata(m *state.ManagerMetadata) bool {
	return m.Version == 1 && m.InstanceID != "" && m.Port > 0 && m.Pid > 0 && m.StartedAt != ""
}

// Discover probes root for a daemon. It never spawns one.
func Discover(ctx context.Context, root string, cat *catalog.ServiceCatalog) Discovery {
	d, _ := discoverProbed(ctx, newFileIO(cat), nil, root, cat)
	return d
}

// discoverProbed is Discover plus whether the liveness probe was answered
// `unauthorized` — Live covers both, and RequireClientFor needs the difference
// without probing the daemon a second time.
func discoverProbed(ctx context.Context, io fileio.FileIO, doer HTTPDoer, root string, cat *catalog.ServiceCatalog) (Discovery, bool) {
	runtimeDirectory := paths.ResolveRuntimeDirectory(root, runtimeDirOf(cat))
	lockDirectory := paths.LockDir(runtimeDirectory)
	if !io.IsPrivateDirectory(runtimeDirectory) || !io.IsPrivateDirectory(lockDirectory) {
		return Discovery{Kind: DiscoveryAbsent}, false
	}
	rawMetadata, err := io.ReadFile(paths.MetadataPath(runtimeDirectory))
	if err != nil || rawMetadata == nil {
		return Discovery{Kind: DiscoveryAbsent}, false
	}
	rawToken, err := io.ReadFile(paths.TokenPath(runtimeDirectory))
	if err != nil || rawToken == nil {
		return Discovery{Kind: DiscoveryAbsent}, false
	}
	var metadata state.ManagerMetadata
	if json.Unmarshal([]byte(*rawMetadata), &metadata) != nil {
		return Discovery{Kind: DiscoveryMalformed}, false
	}
	token := strings.TrimSpace(*rawToken)
	if !validMetadata(&metadata) || token == "" {
		return Discovery{Kind: DiscoveryMalformed}, false
	}
	artifacts := manager.ReadOwnedLockArtifacts(io, lockDirectory)
	if artifacts == nil {
		return Discovery{Kind: DiscoveryMalformed}, false
	}
	ownershipKey := manager.ReadLockOwnershipKey(io, runtimeDirectory)
	if ownershipKey == nil {
		return Discovery{Kind: DiscoveryMalformed}, false
	}
	if !manager.VerifyLockOwnershipProof(ownershipKey, &artifacts.Metadata, artifacts.Token, &artifacts.Proof) {
		return Discovery{Kind: DiscoveryMalformed}, false
	}
	client := &Client{
		Root:             root,
		RuntimeDirectory: runtimeDirectory,
		Metadata:         metadata,
		Token:            token,
		Doer:             doer,
	}
	if metadata.ProtocolVersion != state.ProtocolVersion {
		return Discovery{Kind: DiscoveryIncompatible, Client: client}, false
	}
	if _, err := client.requestRaw(ctx, "/v1/manager", http.MethodGet, nil, nil, &defaultTimeout); err == nil {
		return Discovery{Kind: DiscoveryLive, Client: client}, false
	} else if strings.Contains(err.Error(), "unauthorized") {
		return Discovery{Kind: DiscoveryLive, Client: client}, true
	}
	return Discovery{Kind: DiscoveryStale, Client: client}, false
}

// BootstrappingDaemon is the pid of a live daemon that holds root's lock but
// has not published its port yet, or nil.
func BootstrappingDaemon(ctx context.Context, root string, cat *catalog.ServiceCatalog) *int64 {
	if ctx.Err() != nil {
		return nil
	}
	return bootstrappingDaemon(newFileIO(cat), root, cat)
}

func bootstrappingDaemon(io fileio.FileIO, root string, cat *catalog.ServiceCatalog) *int64 {
	runtimeDirectory := paths.ResolveRuntimeDirectory(root, runtimeDirOf(cat))
	raw, err := io.ReadFile(paths.MetadataPath(runtimeDirectory))
	if err != nil || raw == nil {
		return nil
	}
	var metadata state.ManagerMetadata
	if json.Unmarshal([]byte(*raw), &metadata) != nil {
		return nil
	}
	if metadata.Port == 0 && metadata.Pid > 0 && platform.IsPIDAlive(metadata.Pid) {
		pid := metadata.Pid
		return &pid
	}
	return nil
}

// RequireClientFor returns a live, authorized daemon for root — never spawns
// one. Discover already probed /v1/manager, so its answer decides liveness and
// auth without a second round trip.
func RequireClientFor(ctx context.Context, root string, cat *catalog.ServiceCatalog) (*Client, error) {
	return requireClientForWith(ctx, newFileIO(cat), nil, root, cat)
}

func requireClientForWith(ctx context.Context, io fileio.FileIO, doer HTTPDoer, root string, cat *catalog.ServiceCatalog) (*Client, error) {
	d, unauthorized := discoverProbed(ctx, io, doer, root, cat)
	switch d.Kind {
	case DiscoveryIncompatible:
		return nil, protocolError()
	case DiscoveryLive:
		if unauthorized {
			return nil, unauthorizedError()
		}
		return d.Client, nil
	default:
		return nil, unavailableError("hearth manager is unavailable")
	}
}

// Ensure discovers a live daemon, spawning one (via options.SpawnDaemon) and
// polling if none is found.
//
// A daemon that holds the lock but is still bootstrapping (port 0, pid alive)
// is waited for, up to ensureBootstrapWait, instead of being reported
// unavailable after a fixed 5s, and is never joined by a second spawn. The
// wait ends as soon as that pid exits.
func Ensure(ctx context.Context, root string, options *Options) (*Client, error) {
	if options == nil {
		options = &Options{}
	}
	io := newFileIO(options.Catalog)
	doer := options.Doer
	discovered := discoverWith(ctx, io, doer, root, options.Catalog)
	switch discovered.Kind {
	case DiscoveryLive:
		return discovered.Client, nil
	case DiscoveryIncompatible:
		return nil, protocolError()
	case DiscoveryStale:
		// /v1/manager failed but the recorded pid is still alive. Spawning
		// another daemon would fight it for the lock; report unavailable and
		// let the caller retry or restart.
		if platform.IsPIDAlive(discovered.Client.Metadata.Pid) {
			return nil, unavailableError("hearth manager is unavailable")
		}
	}
	if bootstrappingDaemon(io, root, options.Catalog) == nil && options.SpawnDaemon != nil {
		options.SpawnDaemon(root)
	}
	started := time.Now()
	var lastBootstrapping *int64
	for {
		if err := sleepCtx(ctx, 50*time.Millisecond); err != nil {
			return nil, err
		}
		discovered := discoverWith(ctx, io, doer, root, options.Catalog)
		switch discovered.Kind {
		case DiscoveryLive:
			return discovered.Client, nil
		case DiscoveryIncompatible:
			return nil, protocolError()
		}
		elapsed := time.Since(started)
		if pid := bootstrappingDaemon(io, root, options.Catalog); pid != nil {
			if elapsed < ensureBootstrapWait {
				lastBootstrapping = pid
				continue
			}
			return nil, unavailableError(fmt.Sprintf(
				"hearth manager (pid %d) is still starting after %ds",
				*pid, int(ensureBootstrapWait.Seconds()),
			))
		}
		if lastBootstrapping != nil {
			// One more look: it may have published its port between the two reads.
			if d := discoverWith(ctx, io, doer, root, options.Catalog); d.Kind == DiscoveryLive {
				return d.Client, nil
			}
			return nil, unavailableError(fmt.Sprintf(
				"hearth manager (pid %d) exited before it started listening; see its log in the runtime directory",
				*lastBootstrapping,
			))
		}
		if elapsed >= ensureSpawnWait {
			return nil, unavailableError("hearth manager is unavailable")
		}
	}
}

func discoverWith(ctx context.Context, io fileio.FileIO, doer HTTPDoer, root string, cat *catalog.ServiceCatalog) Discovery {
	d, _ := discoverProbed(ctx, io, doer, root, cat)
	return d
}

func sleepCtx(ctx context.Context, d time.Duration) error {
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-timer.C:
		return nil
	}
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

// EncodePathSegment percent-encodes one URL path segment (anything outside RFC
// 3986's unreserved set), so a service or operation id can never change the
// request path or add a query.
func EncodePathSegment(segment string) string {
	var b strings.Builder
	b.Grow(len(segment))
	for i := range segment {
		c := segment[i]
		if c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' ||
			c == '-' || c == '.' || c == '_' || c == '~' {
			b.WriteByte(c)
		} else {
			fmt.Fprintf(&b, "%%%02X", c)
		}
	}
	return b.String()
}

// OperationID validates an operation id: only RFC 3986 unreserved characters,
// 1–128 of them.
func OperationID(value string) (string, error) {
	if len(value) < 1 || len(value) > 128 {
		return "", usageError("operationId is invalid")
	}
	for i := range value {
		c := value[i]
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' ||
			c == '.' || c == '_' || c == '~' || c == '-') {
			return "", usageError("operationId is invalid")
		}
	}
	return value, nil
}

// NewRequestID mints a UUID v4 for an operation's requestId. The daemon
// dedupes on it, so a retry after a timeout must reuse the same id.
func NewRequestID() string {
	var b [16]byte
	_, _ = rand.Read(b[:])
	b[6] = (b[6] & 0x0f) | 0x40
	b[8] = (b[8] & 0x3f) | 0x80
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}

// Request sends one request to the daemon with the standard auth + protocol
// headers and the default 10s transport timeout, returning the decoded JSON
// object.
func (c *Client) Request(ctx context.Context, path, method string, body any) (map[string]any, error) {
	return c.request(ctx, path, method, body, nil, &defaultTimeout)
}

// RequestWithTimeout is Request with a caller-chosen transport timeout (nil =
// unbounded) and protocol version (nil = this binary's). The shared
// attach/install path uses it because a first-time install downloads and
// extracts a tarball.
func (c *Client) RequestWithTimeout(ctx context.Context, path, method string, body any, protocolVersion *uint32, timeout *time.Duration) (map[string]any, error) {
	return c.request(ctx, path, method, body, protocolVersion, timeout)
}

var defaultTimeout = DefaultRequestTimeout

func (c *Client) request(ctx context.Context, path, method string, body any, protocolVersion *uint32, timeout *time.Duration) (map[string]any, error) {
	raw, err := c.requestRaw(ctx, path, method, body, protocolVersion, timeout)
	if err != nil {
		return nil, err
	}
	var decoded map[string]any
	if json.Unmarshal(raw, &decoded) != nil {
		decoded = map[string]any{}
	}
	return decoded, nil
}

// requestRaw performs the request and returns the raw success body. A
// transport failure is KindUnavailable (or KindTimeout); an HTTP error is
// KindHTTP with the daemon's error code and message.
func (c *Client) requestRaw(ctx context.Context, path, method string, body any, protocolVersion *uint32, timeout *time.Duration) ([]byte, error) {
	reqCtx := ctx
	if timeout != nil {
		var cancel context.CancelFunc
		reqCtx, cancel = context.WithTimeout(ctx, *timeout)
		defer cancel()
	}
	url := fmt.Sprintf("http://127.0.0.1:%d%s", c.Metadata.Port, path)
	var reader io.Reader
	if body != nil {
		encoded, err := json.Marshal(body)
		if err != nil {
			return nil, malformedError(err)
		}
		reader = bytes.NewReader(encoded)
	}
	req, err := http.NewRequestWithContext(reqCtx, method, url, reader)
	if err != nil {
		return nil, unavailableError("manager unavailable")
	}
	req.Header.Set("Authorization", "Bearer "+c.Token)
	protocol := state.ProtocolVersion
	if protocolVersion != nil {
		protocol = *protocolVersion
	}
	req.Header.Set("x-hearth-protocol", strconv.FormatUint(uint64(protocol), 10))
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := c.httpDoer().Do(req)
	if err != nil {
		if isTimeout(err) {
			return nil, &Error{Kind: KindTimeout, Message: "manager request timed out", ExitCode: ExitUnavailable}
		}
		return nil, unavailableError("manager unavailable")
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		code := "request_failed"
		message := strconv.Itoa(resp.StatusCode)
		var decoded map[string]any
		if json.Unmarshal(raw, &decoded) == nil {
			if errObj, ok := decoded["error"].(map[string]any); ok {
				if s, ok := errObj["code"].(string); ok {
					code = s
				}
				if s, ok := errObj["message"].(string); ok {
					message = s
				}
			}
		}
		return nil, &Error{Kind: KindHTTP, Code: code, Message: message, ExitCode: ExitUnavailable}
	}
	return raw, nil
}

func isTimeout(err error) bool {
	if errors.Is(err, context.DeadlineExceeded) {
		return true
	}
	var netErr net.Error
	if errors.As(err, &netErr) {
		return netErr.Timeout()
	}
	return os.IsTimeout(err)
}

// ---------------------------------------------------------------------------
// Typed endpoints
// ---------------------------------------------------------------------------

// ManagerInfo is GET /v1/manager.
func (c *Client) ManagerInfo(ctx context.Context) (*state.ManagerInfo, error) {
	raw, err := c.requestRaw(ctx, "/v1/manager", http.MethodGet, nil, nil, &defaultTimeout)
	if err != nil {
		return nil, err
	}
	var info state.ManagerInfo
	if err := json.Unmarshal(raw, &info); err != nil {
		return nil, malformedError(err)
	}
	return &info, nil
}

// Services is GET /v1/services — the authoritative service rows.
func (c *Client) Services(ctx context.Context) ([]state.ServiceLifecycleState, error) {
	raw, err := c.requestRaw(ctx, "/v1/services", http.MethodGet, nil, nil, &defaultTimeout)
	if err != nil {
		return nil, err
	}
	var body struct {
		Services []state.ServiceLifecycleState `json:"services"`
	}
	if err := json.Unmarshal(raw, &body); err != nil {
		return nil, malformedError(err)
	}
	return body.Services, nil
}

// URLs is GET /v1/urls — the `{ urls, unresolved }` body.
func (c *Client) URLs(ctx context.Context) (*URLs, error) {
	raw, err := c.requestRaw(ctx, "/v1/urls", http.MethodGet, nil, nil, &defaultTimeout)
	if err != nil {
		return nil, err
	}
	var body URLs
	if err := json.Unmarshal(raw, &body); err != nil {
		return nil, malformedError(err)
	}
	return &body, nil
}

// Catalog is GET /v1/catalog — the catalog the daemon is serving right now,
// which may be newer than the one this process loaded at startup.
func (c *Client) Catalog(ctx context.Context) (*catalog.ServiceCatalog, error) {
	raw, err := c.requestRaw(ctx, "/v1/catalog", http.MethodGet, nil, nil, &defaultTimeout)
	if err != nil {
		return nil, err
	}
	var body struct {
		Catalog catalog.ServiceCatalog `json:"catalog"`
	}
	if err := json.Unmarshal(raw, &body); err != nil {
		return nil, malformedError(err)
	}
	return &body.Catalog, nil
}

// Log is GET /v1/logs/:id. An explicit limit is honored up to the current file
// cap; omitting it stays the 16 KiB tail.
func (c *Client) Log(ctx context.Context, serviceID string, cursor, generation, limit *uint64) (*state.LogSlice, error) {
	raw, err := c.requestRaw(ctx, "/v1/logs/"+EncodePathSegment(serviceID)+logQuery(cursor, generation, limit), http.MethodGet, nil, nil, &defaultTimeout)
	if err != nil {
		return nil, err
	}
	var slice state.LogSlice
	if err := json.Unmarshal(raw, &slice); err != nil {
		return nil, malformedError(err)
	}
	return &slice, nil
}

// DaemonLog is GET /v1/daemon/log — the daemon's own log, not a service id.
// bytes nil uses the server default (128 KiB).
func (c *Client) DaemonLog(ctx context.Context, bytes *uint64) (*state.LogSlice, error) {
	query := ""
	if bytes != nil {
		query = "?bytes=" + strconv.FormatUint(*bytes, 10)
	}
	raw, err := c.requestRaw(ctx, "/v1/daemon/log"+query, http.MethodGet, nil, nil, &defaultTimeout)
	if err != nil {
		return nil, err
	}
	var slice state.LogSlice
	if err := json.Unmarshal(raw, &slice); err != nil {
		return nil, malformedError(err)
	}
	return &slice, nil
}

// Operation is GET /v1/operations/:id.
func (c *Client) Operation(ctx context.Context, id string) (*state.Operation, error) {
	validated, err := OperationID(id)
	if err != nil {
		return nil, err
	}
	raw, err := c.requestRaw(ctx, "/v1/operations/"+EncodePathSegment(validated), http.MethodGet, nil, nil, &defaultTimeout)
	if err != nil {
		return nil, err
	}
	var body struct {
		Operation state.Operation `json:"operation"`
	}
	if err := json.Unmarshal(raw, &body); err != nil {
		return nil, malformedError(err)
	}
	return &body.Operation, nil
}

// Submit is POST /v1/operations. killUnowned must only ever be set after an
// explicit user confirmation, and only for start — the daemon rejects it
// otherwise.
func (c *Client) Submit(ctx context.Context, action state.ServiceOperationKind, serviceID string, killUnowned bool, requestID string) (*state.Operation, error) {
	body := map[string]any{
		"requestId": requestID,
		"serviceId": serviceID,
		"action":    string(action),
	}
	if killUnowned {
		body["killUnowned"] = true
	}
	return c.postOperation(ctx, "/v1/operations", body)
}

// BulkStart is POST /v1/operations/bulk-start. killUnowned applies to every
// target.
func (c *Client) BulkStart(ctx context.Context, targets []string, killUnowned bool, requestID string) (*state.Operation, error) {
	body := map[string]any{
		"requestId": requestID,
		"targets":   targets,
	}
	if killUnowned {
		body["killUnowned"] = true
	}
	return c.postOperation(ctx, "/v1/operations/bulk-start", body)
}

func (c *Client) postOperation(ctx context.Context, path string, body map[string]any) (*state.Operation, error) {
	raw, err := c.requestRaw(ctx, path, http.MethodPost, body, nil, &defaultTimeout)
	if err != nil {
		return nil, err
	}
	var response struct {
		Operation state.Operation `json:"operation"`
	}
	if err := json.Unmarshal(raw, &response); err != nil {
		return nil, malformedError(err)
	}
	return &response.Operation, nil
}

// Wait polls an operation until it is terminal, or until deadline passes — in
// which case the last (still queued/running) snapshot is returned so the
// caller can hand out its id.
func (c *Client) Wait(ctx context.Context, id string, deadline *time.Time) (*state.Operation, error) {
	for {
		operation, err := c.Operation(ctx, id)
		if err != nil {
			return nil, err
		}
		if operation.Status == state.OpStatusSucceeded || operation.Status == state.OpStatusFailed {
			return operation, nil
		}
		if deadline != nil && !time.Now().Before(*deadline) {
			return operation, nil
		}
		if err := sleepCtx(ctx, operationPollInterval); err != nil {
			return nil, err
		}
	}
}

// EnsurePayload is the `manager ensure --json` contract: everything a generic
// HTTP+SSE client needs to talk to the daemon directly.
func (c *Client) EnsurePayload() map[string]any {
	return map[string]any{
		"instanceId":       c.Metadata.InstanceID,
		"port":             c.Metadata.Port,
		"token":            c.Token,
		"protocolVersion":  c.Metadata.ProtocolVersion,
		"runtimeDirectory": c.RuntimeDirectory,
		"root":             c.Root,
	}
}

func logQuery(cursor, generation, limit *uint64) string {
	var parts []string
	if cursor != nil {
		parts = append(parts, "cursor="+strconv.FormatUint(*cursor, 10))
	}
	if generation != nil {
		parts = append(parts, "generation="+strconv.FormatUint(*generation, 10))
	}
	if limit != nil {
		parts = append(parts, "limit="+strconv.FormatUint(*limit, 10))
	}
	if len(parts) == 0 {
		return ""
	}
	return "?" + strings.Join(parts, "&")
}
