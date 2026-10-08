// HTTP plumbing for HearthManager — ported from the axum router/middleware in
// rust/crates/hearth-core/src/manager/http/routes.rs. A tiny method+path
// dispatcher replaces axum; every route, status code and JSON shape is
// preserved. Unmatched paths AND wrong methods both answer the same
// `{error:{code,message}}` envelope with 404 `not_found`, matching axum's
// `fallback` + `method_not_allowed_fallback`.
package manager

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/ngosangns/hearth/go/internal/state"
)

// ---------------------------------------------------------------------------------------------
// HTTP error envelope
// ---------------------------------------------------------------------------------------------

// ManagerHttpError is the `{error:{code,message}}` envelope every failure uses.
type ManagerHttpError struct {
	Status  int
	Code    string
	Message string
}

func (e *ManagerHttpError) Error() string { return e.Message }

func newHTTPError(status int, code, message string) *ManagerHttpError {
	return &ManagerHttpError{Status: status, Code: code, Message: message}
}

type errorBody struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

type errorEnvelope struct {
	Error errorBody `json:"error"`
}

// marshalNoEscape marshals without HTML escaping so `<`, `>`, `&` stay literal
// — serde_json does not escape them, and the SSE `data:` payload must match.
func marshalNoEscape(v any) ([]byte, error) {
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimRight(buf.Bytes(), "\n"), nil
}

// writeJSON writes a JSON body with `cache-control: no-store` (axum's
// json_response).
func writeJSON(w http.ResponseWriter, status int, body any) {
	data, err := marshalNoEscape(body)
	if err != nil {
		w.Header().Set("content-type", "application/json")
		w.Header().Set("cache-control", "no-store")
		w.WriteHeader(http.StatusInternalServerError)
		_, _ = w.Write([]byte(`{"error":{"code":"internal_error","message":"failed to encode response"}}`))
		return
	}
	w.Header().Set("content-type", "application/json")
	w.Header().Set("cache-control", "no-store")
	w.WriteHeader(status)
	_, _ = w.Write(data)
}

func writeHTTPError(w http.ResponseWriter, e *ManagerHttpError) {
	writeJSON(w, e.Status, errorEnvelope{Error: errorBody{Code: e.Code, Message: e.Message}})
}

// ---------------------------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------------------------

type routeHandler func(w http.ResponseWriter, r *http.Request, params map[string]string)

type routeEntry struct {
	method  string
	pattern []string
	auth    bool
	handler routeHandler
}

// buildRoutes assembles the route table. More specific literal routes must
// precede a wildcard at the same depth (`/v1/operations/bulk-start` before
// `/v1/operations/:id`).
func (m *HearthManager) buildRoutes() []routeEntry {
	routes := []routeEntry{
		{"GET", []string{"healthz"}, false, m.getHealthz},
		{"GET", []string{"v1", "manager"}, true, m.getManagerInfo},
		{"GET", []string{"v1", "catalog"}, true, m.getCatalog},
		{"GET", []string{"v1", "urls"}, true, m.getUrls},
		{"POST", []string{"v1", "manager", "reload"}, true, m.postReload},
		{"GET", []string{"v1", "services"}, true, m.getServices},
		{"POST", []string{"v1", "operations"}, true, m.postOperation},
		{"POST", []string{"v1", "operations", "bulk-start"}, true, m.postBulkStart},
		{"GET", []string{"v1", "operations", ":id"}, true, m.getOperation},
		{"GET", []string{"v1", "events"}, true, m.getEvents},
		{"GET", []string{"v1", "events", "stream"}, true, m.getEventsStream},
		{"GET", []string{"v1", "logs", ":id"}, true, m.getLogs},
		{"GET", []string{"v1", "daemon", "log"}, true, m.getDaemonLog},
		{"POST", []string{"v1", "manager", "shutdown"}, true, m.postShutdown},
	}
	if m.shared != nil {
		routes = append(routes, sharedRoutes(m)...)
	}
	return routes
}

// Router returns the HTTP handler for the manager.
func (m *HearthManager) Router() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		segs := splitPath(r.URL.Path)
		for i := range m.routes {
			rt := &m.routes[i]
			if rt.method != r.Method {
				continue
			}
			params, ok := matchRoute(rt.pattern, segs)
			if !ok {
				continue
			}
			if rt.auth && !m.authorized(w, r) {
				return
			}
			rt.handler(w, r, params)
			return
		}
		writeHTTPError(w, newHTTPError(http.StatusNotFound, "not_found", "Not found"))
	})
}

// splitPath splits a URL path into segments without dropping a trailing empty
// segment, so `/v1/services/` (like axum) does not match `/v1/services`.
func splitPath(p string) []string {
	if p == "" || p == "/" {
		return nil
	}
	p = strings.TrimPrefix(p, "/")
	return strings.Split(p, "/")
}

func matchRoute(pattern, segs []string) (map[string]string, bool) {
	if len(pattern) != len(segs) {
		return nil, false
	}
	var params map[string]string
	for i, p := range pattern {
		if strings.HasPrefix(p, ":") {
			if segs[i] == "" {
				return nil, false
			}
			if params == nil {
				params = map[string]string{}
			}
			params[p[1:]] = segs[i]
			continue
		}
		if p != segs[i] {
			return nil, false
		}
	}
	return params, true
}

// authorized enforces the bearer token (401) then the protocol header (426),
// in that order.
func (m *HearthManager) authorized(w http.ResponseWriter, r *http.Request) bool {
	if !m.isAuthorized(r) {
		writeHTTPError(w, newHTTPError(http.StatusUnauthorized, "unauthorized", "Bearer authentication is required"))
		return false
	}
	pv, err := strconv.ParseUint(r.Header.Get("x-hearth-protocol"), 10, 32)
	if err != nil || uint32(pv) != state.ProtocolVersion {
		writeHTTPError(w, newHTTPError(426, "incompatible_protocol", fmt.Sprintf("Expected protocol %d", state.ProtocolVersion)))
		return false
	}
	return true
}

// isAuthorized checks only the bearer token (used by /healthz to decide whether
// to include instanceId).
func (m *HearthManager) isAuthorized(r *http.Request) bool {
	return ConstantTimeEq(r.Header.Get("authorization"), "Bearer "+m.token)
}

// ---------------------------------------------------------------------------------------------
// Request helpers
// ---------------------------------------------------------------------------------------------

// strictBody parses a JSON object body, rejecting unknown keys and missing
// required ones.
func strictBody(r *http.Request, allowed, required []string) (map[string]any, *ManagerHttpError) {
	raw, err := io.ReadAll(r.Body)
	if err != nil {
		return nil, newHTTPError(http.StatusBadRequest, "invalid_json", "Request body must be JSON")
	}
	var value any
	if err := json.Unmarshal(raw, &value); err != nil {
		return nil, newHTTPError(http.StatusBadRequest, "invalid_json", "Request body must be JSON")
	}
	obj, ok := value.(map[string]any)
	if !ok {
		return nil, newHTTPError(http.StatusBadRequest, "invalid_request", "Request schema is invalid")
	}
	for k := range obj {
		if !containsString(allowed, k) {
			return nil, newHTTPError(http.StatusBadRequest, "invalid_request", "Request schema is invalid")
		}
	}
	for _, k := range required {
		if _, ok := obj[k]; !ok {
			return nil, newHTTPError(http.StatusBadRequest, "invalid_request", "Request schema is invalid")
		}
	}
	return obj, nil
}

func containsString(list []string, v string) bool {
	for _, s := range list {
		if s == v {
			return true
		}
	}
	return false
}

// requireRequestID validates `requestId`: a non-empty string of at most 128
// bytes, the same bound on every endpoint.
func requireRequestID(body map[string]any) (string, *ManagerHttpError) {
	v, ok := body["requestId"]
	s, isStr := v.(string)
	if !ok || !isStr || s == "" || len(s) > 128 {
		return "", newHTTPError(http.StatusBadRequest, "invalid_request", "requestId must be a non-empty string of at most 128 bytes")
	}
	return s, nil
}

// parseBoolFlag reads an optional boolean flag (`killUnowned`, `force`):
// absent means false, anything but a bool is a 400.
func parseBoolFlag(body map[string]any, key string) (bool, *ManagerHttpError) {
	v, ok := body[key]
	if !ok {
		return false, nil
	}
	b, isBool := v.(bool)
	if !isBool {
		return false, newHTTPError(http.StatusBadRequest, "invalid_request", key+" must be a boolean")
	}
	return b, nil
}

// ensureNotClosing refuses every mutation once shutdown has begun.
func (m *HearthManager) ensureNotClosing() *ManagerHttpError {
	if m.closing.Load() {
		return newHTTPError(http.StatusConflict, "manager_closing", "Manager is shutting down")
	}
	return nil
}

// parseU64 mirrors Rust's `u64::from_str`: an optional leading `+`, then
// digits.
func parseU64(s string) (uint64, bool) {
	if strings.HasPrefix(s, "+") {
		s = s[1:]
	}
	if s == "" {
		return 0, false
	}
	n, err := strconv.ParseUint(s, 10, 64)
	return n, err == nil
}

func parseAfter(q url.Values) (*uint64, *ManagerHttpError) {
	vals, ok := q["after"]
	if !ok {
		return nil, nil
	}
	n, ok2 := parseU64(vals[0])
	if !ok2 {
		return nil, newHTTPError(http.StatusBadRequest, "invalid_cursor", "after must be a non-negative integer")
	}
	return &n, nil
}

func optionalU64(q url.Values, key, code, message string) (*uint64, *ManagerHttpError) {
	vals, ok := q[key]
	if !ok {
		return nil, nil
	}
	n, ok2 := parseU64(vals[0])
	if !ok2 {
		return nil, newHTTPError(http.StatusBadRequest, code, message)
	}
	return &n, nil
}

// ---------------------------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------------------------

type operationResponse struct {
	Operation *state.Operation `json:"operation"`
}

type servicesResponse struct {
	Services []state.ServiceLifecycleState `json:"services"`
}

type catalogResponse struct {
	Catalog any `json:"catalog"`
}

type eventsReplayResponse struct {
	Epoch          string               `json:"epoch"`
	Reset          bool                 `json:"reset"`
	Events         []state.ManagerEvent `json:"events"`
	LatestSequence uint64               `json:"latestSequence"`
}

type reloadResponse struct {
	Stopped []string `json:"stopped"`
	Changed []string `json:"changed"`
}

// ---------------------------------------------------------------------------------------------
// SSE
// ---------------------------------------------------------------------------------------------

const (
	sseMaxQueueFrames    = 64
	sseKeepAliveInterval = 15 * time.Second
)

// sseSink is the bounded per-client frame queue. A full queue drops the live
// sender and signals overflow — the stream is CLOSED, not silently thinned, so
// the client reconnects with its cursor and takes a `reset` replay.
type sseSink struct {
	mu           sync.Mutex
	ch           chan string
	closed       bool
	overflow     chan struct{}
	overflowOnce sync.Once
}

func newSSESink(capacity int) *sseSink {
	return &sseSink{ch: make(chan string, capacity), overflow: make(chan struct{})}
}

func (s *sseSink) send(frame string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return
	}
	select {
	case s.ch <- frame:
	default:
		s.closed = true
		close(s.ch)
		s.overflowOnce.Do(func() { close(s.overflow) })
	}
}

func (s *sseSink) close() {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed {
		return
	}
	s.closed = true
	close(s.ch)
}

// ssePrefixGate buffers live events until the snapshot frames are queued, then
// drains them under the same lock as the gate flips. Otherwise a publish that
// lands after subscribe and before the snapshot sends is delivered ahead of the
// replay the client resynchronizes from.
type ssePrefixGate struct {
	mu      sync.Mutex
	open    bool
	pending []state.ManagerEvent
}

// eventToSSE renders one manager event as an SSE frame:
//
//	id: <sequence>
//	event: <type>
//	data: <json>
func eventToSSE(event *state.ManagerEvent) string {
	data, _ := marshalNoEscape(event)
	var b strings.Builder
	b.WriteString("id: ")
	b.WriteString(strconv.FormatUint(event.Sequence, 10))
	b.WriteString("\nevent: ")
	b.WriteString(event.Type)
	b.WriteString("\ndata: ")
	b.Write(data)
	b.WriteString("\n\n")
	return b.String()
}

// replayEvent renders the `replay` frame that opens every stream.
func replayEvent(epoch string, reset bool, latestSequence uint64) string {
	data, _ := marshalNoEscape(struct {
		Epoch          string `json:"epoch"`
		Reset          bool   `json:"reset"`
		LatestSequence uint64 `json:"latestSequence"`
	}{epoch, reset, latestSequence})
	return "event: replay\ndata: " + string(data) + "\n\n"
}

// writeSSEStream drains frames to the client, emitting axum's default
// keep-alive comment (`:\n\n`) 15s after the last write.
func writeSSEStream(w http.ResponseWriter, flusher http.Flusher, frames <-chan string, ctx context.Context) {
	timer := time.NewTimer(sseKeepAliveInterval)
	defer timer.Stop()
	for {
		select {
		case frame, ok := <-frames:
			if !ok {
				return
			}
			if _, err := io.WriteString(w, frame); err != nil {
				return
			}
			flusher.Flush()
			if !timer.Stop() {
				select {
				case <-timer.C:
				default:
				}
			}
			timer.Reset(sseKeepAliveInterval)
		case <-timer.C:
			if _, err := io.WriteString(w, ":\n\n"); err != nil {
				return
			}
			flusher.Flush()
			timer.Reset(sseKeepAliveInterval)
		case <-ctx.Done():
			return
		}
	}
}
