// Package mcpserver is the MCP tool surface for a hearth project's daemon.
//
// Ported from rust/crates/hearth-mcp/src/server.rs (the manual `ServerHandler`
// tool surface) and rust/crates/hearth-mcp/src/client.rs (the default
// `ManagerApiClient`). Rust hand-implements rmcp's `ServerHandler`; Go has no
// rmcp, so the MCP protocol subset is implemented directly over stdio:
// newline-delimited JSON-RPC 2.0 (the MCP stdio transport, no Content-Length
// headers) with the methods `initialize`, `notifications/initialized`,
// `tools/list`, `tools/call` and `ping`.
//
// The tool set is only known at runtime: tool names carry a configurable
// prefix, the `manage`/`status`/`logs` schemas embed an `enum` of the daemon's
// current service ids, and `manage`'s required fields depend on
// `requireConfirm`.
package mcpserver

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"regexp"
	"strings"
	"sync"

	"github.com/ngosangns/hearth/go/internal/state"
)

// secretKeyPattern matches any object key that looks like a credential; a
// matching key is dropped before the value reaches the model.
var secretKeyPattern = regexp.MustCompile(`(?i)authorization|token|ownership(?:key|proof)|secret|password|api[_-]?key`)

// bearerPattern matches a `Bearer <token>` credential in an error message.
var bearerPattern = regexp.MustCompile(`(?i)Bearer\s+\S+`)

// redactDepthCap bounds how deep redact recurses before truncating.
const redactDepthCap = 12

// maxSafeInteger is JavaScript's `Number.isSafeInteger` bound — what every MCP
// host's JSON layer produces.
const maxSafeInteger int64 = 9_007_199_254_740_991

// Options configures a Server. It mirrors the Rust
// `CreateHearthMcpServerOptions`.
type Options struct {
	// Name is the MCP server name registered with the SDK, e.g. "hearth".
	Name string
	// Version is the server version; empty means "1.0.0".
	Version string
	// ToolPrefix is prepended to every tool name: `${ToolPrefix}status`,
	// `${ToolPrefix}logs`, etc.
	ToolPrefix string
	// RequireConfirm requires a schema-enforced `confirm: true` argument on the
	// manage tool (start/stop/restart) and the daemon-lifecycle tools — the
	// safer default. nil means true; set it to false to disable the gate.
	RequireConfirm *bool
	// KnownServiceIDs is the service ids to start with. Refreshed from
	// HearthMcpClient.ServiceIDs on every `tools/list` and whenever a call
	// names an id not in the list.
	KnownServiceIDs []string
}

// DefaultOptions is the Rust `CreateHearthMcpServerOptions::default()`: the
// name "hearth", no tool prefix, and the confirm gate on.
func DefaultOptions() Options {
	return Options{Name: "hearth", RequireConfirm: new(true)}
}

// Server is the MCP tool surface. It holds the client, the options and the
// current service ids (refreshed from the client).
type Server struct {
	client  HearthMcpClient
	options Options

	mu         sync.Mutex
	serviceIDs []string
	// protocolVersion is the version negotiated by `initialize`; it decides
	// whether results carry the SEP-2322 `resultType` field.
	protocolVersion string
}

// New builds a Server. Empty Name/Version fall back to "hearth"/"1.0.0", and a
// nil RequireConfirm means true.
func New(c HearthMcpClient, opts Options) *Server {
	if opts.Name == "" {
		opts.Name = "hearth"
	}
	ids := opts.KnownServiceIDs
	if ids == nil {
		ids = []string{}
	}
	return &Server{client: c, options: opts, serviceIDs: ids}
}

// version is the server version reported to the client.
func (s *Server) version() string {
	if s.options.Version == "" {
		return "1.0.0"
	}
	return s.options.Version
}

// requireConfirm reports whether the confirm gate is on (nil means true).
func (s *Server) requireConfirm() bool {
	return s.options.RequireConfirm == nil || *s.options.RequireConfirm
}

// knownServiceIDs is a copy of the current service ids.
func (s *Server) knownServiceIDs() []string {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]string, len(s.serviceIDs))
	copy(out, s.serviceIDs)
	return out
}

// hasServiceID reports whether id is in the current list.
func (s *Server) hasServiceID(id string) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, known := range s.serviceIDs {
		if known == id {
			return true
		}
	}
	return false
}

// refreshServiceIDs re-reads the daemon's current service ids (after a
// `manager reload`), keeping the old list when the client can't answer.
func (s *Server) refreshServiceIDs(ctx context.Context) {
	ids, ok := s.client.ServiceIDs(ctx)
	if !ok {
		return
	}
	if ids == nil {
		ids = []string{}
	}
	s.mu.Lock()
	s.serviceIDs = ids
	s.mu.Unlock()
}

// sep2322 reports whether the negotiated protocol version carries the SEP-2322
// `resultType` field (protocol version 2026-07-28 or newer).
func (s *Server) sep2322() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.protocolVersion >= protocolVersion2026_07_28
}

// ---------------------------------------------------------------------------
// JSON-RPC over stdio
// ---------------------------------------------------------------------------

// rpcRequest is one newline-delimited JSON-RPC 2.0 message. ID is nil for a
// notification.
type rpcRequest struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params"`
}

// Serve runs the MCP stdio transport: it reads newline-delimited JSON-RPC
// messages from stdin, dispatches them, and writes the responses to stdout. It
// returns nil when stdin reaches EOF, or ctx's error when ctx is cancelled.
func (s *Server) Serve(ctx context.Context, stdin io.Reader, stdout io.Writer) error {
	reader := bufio.NewReader(stdin)
	writer := bufio.NewWriter(stdout)
	for {
		if err := ctx.Err(); err != nil {
			return err
		}
		line, err := reader.ReadBytes('\n')
		if trimmed := bytes.TrimSpace(line); len(trimmed) > 0 {
			s.handleMessage(ctx, trimmed, writer)
			if flushErr := writer.Flush(); flushErr != nil {
				return flushErr
			}
		}
		if err != nil {
			if errors.Is(err, io.EOF) {
				return nil
			}
			return err
		}
	}
}

// handleMessage dispatches one JSON-RPC message. A notification (no id)
// produces no response.
func (s *Server) handleMessage(ctx context.Context, raw []byte, w *bufio.Writer) {
	var request rpcRequest
	if err := json.Unmarshal(raw, &request); err != nil {
		writeRPCError(w, nil, -32700, "Parse error")
		return
	}
	if request.Method == "" {
		if len(request.ID) > 0 {
			writeRPCError(w, request.ID, -32600, "Invalid Request")
		}
		return
	}
	isNotification := len(request.ID) == 0
	switch request.Method {
	case "initialize":
		if isNotification {
			return
		}
		s.handleInitialize(request, w)
	case "tools/list":
		if isNotification {
			return
		}
		s.handleListTools(ctx, request, w)
	case "tools/call":
		if isNotification {
			return
		}
		s.handleCallTool(ctx, request, w)
	case "ping":
		if isNotification {
			return
		}
		writeRPCResult(w, request.ID, map[string]any{})
	default:
		// Unknown notifications (e.g. notifications/initialized,
		// notifications/cancelled) are ignored; unknown requests are an error.
		if isNotification {
			return
		}
		writeRPCError(w, request.ID, -32601, "Method not found: "+request.Method)
	}
}

// handleInitialize negotiates the protocol version and echoes the server's
// capabilities and info.
func (s *Server) handleInitialize(request rpcRequest, w *bufio.Writer) {
	var params struct {
		ProtocolVersion string `json:"protocolVersion"`
	}
	_ = json.Unmarshal(request.Params, &params)
	negotiated := negotiateProtocolVersion(params.ProtocolVersion)
	s.mu.Lock()
	s.protocolVersion = negotiated
	s.mu.Unlock()
	writeRPCResult(w, request.ID, map[string]any{
		"protocolVersion": negotiated,
		"capabilities":    map[string]any{"tools": map[string]any{}},
		"serverInfo":      map[string]any{"name": s.options.Name, "version": s.version()},
	})
}

// handleListTools refreshes the service ids and returns the tool definitions.
func (s *Server) handleListTools(ctx context.Context, request rpcRequest, w *bufio.Writer) {
	s.refreshServiceIDs(ctx)
	result := map[string]any{"tools": s.toolDefinitions()}
	if s.sep2322() {
		result["resultType"] = "complete"
	}
	writeRPCResult(w, request.ID, result)
}

// handleCallTool strips the tool prefix, dispatches, and wraps the result (or
// the error) in a text content block.
func (s *Server) handleCallTool(ctx context.Context, request rpcRequest, w *bufio.Writer) {
	var params struct {
		Name      string          `json:"name"`
		Arguments json.RawMessage `json:"arguments"`
	}
	if err := json.Unmarshal(request.Params, &params); err != nil {
		writeRPCError(w, request.ID, -32602, "Invalid params")
		return
	}
	arguments, err := decodeArguments(params.Arguments)
	if err != nil {
		writeRPCError(w, request.ID, -32602, "Invalid params")
		return
	}
	tool, matched := strings.CutPrefix(params.Name, s.options.ToolPrefix)
	var value any
	var dispatchErr error
	if matched {
		value, dispatchErr = s.dispatch(ctx, tool, arguments)
	} else {
		dispatchErr = fmt.Errorf("unknown tool: %s", params.Name)
	}
	result := map[string]any{}
	if dispatchErr == nil {
		text, err := json.MarshalIndent(redactToolResult(tool, value), "", "  ")
		if err != nil {
			text = []byte("null")
		}
		result["content"] = []any{map[string]any{"type": "text", "text": string(text)}}
	} else {
		result["content"] = []any{map[string]any{"type": "text", "text": safeErrorMessage(dispatchErr.Error())}}
		result["isError"] = true
	}
	if s.sep2322() {
		result["resultType"] = "complete"
	}
	writeRPCResult(w, request.ID, result)
}

// writeRPCResult writes a JSON-RPC success response.
func writeRPCResult(w *bufio.Writer, id json.RawMessage, result any) {
	writeJSONLine(w, map[string]any{"jsonrpc": "2.0", "id": idValue(id), "result": result})
}

// writeRPCError writes a JSON-RPC error response.
func writeRPCError(w *bufio.Writer, id json.RawMessage, code int, message string) {
	writeJSONLine(w, map[string]any{
		"jsonrpc": "2.0",
		"id":      idValue(id),
		"error":   map[string]any{"code": code, "message": message},
	})
}

// idValue renders a request id, or nil (JSON null) when absent.
func idValue(id json.RawMessage) any {
	if len(id) == 0 {
		return nil
	}
	return id
}

// writeJSONLine marshals value and writes it followed by a newline.
func writeJSONLine(w *bufio.Writer, value any) {
	data, err := json.Marshal(value)
	if err != nil {
		return
	}
	_, _ = w.Write(data)
	_ = w.WriteByte('\n')
}

// ---------------------------------------------------------------------------
// Protocol version negotiation
// ---------------------------------------------------------------------------

// The protocol versions rmcp 3.4.0 knows, oldest first.
const (
	protocolVersion2024_11_05 = "2024-11-05"
	protocolVersion2025_03_26 = "2025-03-26"
	protocolVersion2025_06_18 = "2025-06-18"
	protocolVersion2025_11_25 = "2025-11-25"
	protocolVersion2026_07_28 = "2026-07-28"
)

// knownProtocolVersions is rmcp's `ProtocolVersion::KNOWN_VERSIONS`.
var knownProtocolVersions = []string{
	protocolVersion2024_11_05,
	protocolVersion2025_03_26,
	protocolVersion2025_06_18,
	protocolVersion2025_11_25,
	protocolVersion2026_07_28,
}

// serverFallbackProtocolVersion is rmcp's `ProtocolVersion::LATEST` — the
// server's own default, and the fallback for a client that names a version the
// server cannot serve over `initialize`.
const serverFallbackProtocolVersion = protocolVersion2025_11_25

// isLegacyVersion reports whether version predates 2026-07-28, the revision
// that replaced the `initialize` handshake with per-request metadata. ISO
// `YYYY-MM-DD` versions compare lexically the same as chronologically.
func isLegacyVersion(version string) bool {
	return version < protocolVersion2026_07_28
}

// negotiateProtocolVersion ports rmcp's `negotiate_protocol_version`: echo a
// legacy version the server supports, else fall back to the server's newest
// legacy version.
func negotiateProtocolVersion(requested string) string {
	if isLegacyVersion(requested) && containsString(knownProtocolVersions, requested) {
		return requested
	}
	return serverFallbackProtocolVersion
}

// containsString reports whether values contains target.
func containsString(values []string, target string) bool {
	for _, value := range values {
		if value == target {
			return true
		}
	}
	return false
}

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

// arguments is a decoded JSON object that remembers its key order. serde_json
// is built with `preserve_order`, so the Rust server lists unexpected keys in
// the order the client sent them; a plain Go map would sort them.
type arguments struct {
	keys   []string
	values map[string]any
}

// has reports whether key is present.
func (a arguments) has(key string) bool {
	_, ok := a.values[key]
	return ok
}

// value is the value for key, or nil.
func (a arguments) value(key string) any { return a.values[key] }

// decodeArguments decodes a JSON-RPC `arguments` payload into an ordered
// object. An absent or null payload is an empty object.
func decodeArguments(raw json.RawMessage) (arguments, error) {
	out := arguments{values: map[string]any{}}
	if len(raw) == 0 || string(raw) == "null" {
		return out, nil
	}
	decoder := json.NewDecoder(bytes.NewReader(raw))
	token, err := decoder.Token()
	if err != nil {
		return arguments{}, err
	}
	if delim, ok := token.(json.Delim); !ok || delim != '{' {
		return arguments{}, fmt.Errorf("arguments must be an object")
	}
	for decoder.More() {
		keyToken, err := decoder.Token()
		if err != nil {
			return arguments{}, err
		}
		key, ok := keyToken.(string)
		if !ok {
			return arguments{}, fmt.Errorf("arguments must be an object")
		}
		var value any
		if err := decoder.Decode(&value); err != nil {
			return arguments{}, err
		}
		out.keys = append(out.keys, key)
		out.values[key] = value
	}
	return out, nil
}

// requireOnlyKeys rejects any argument key outside allowed, listing the
// unexpected keys in the order the client sent them.
func requireOnlyKeys(value arguments, allowed ...string) error {
	var unexpected []string
	for _, key := range value.keys {
		if !containsString(allowed, key) {
			unexpected = append(unexpected, key)
		}
	}
	if len(unexpected) == 0 {
		return nil
	}
	suffix := ""
	if len(unexpected) > 1 {
		suffix = "s"
	}
	return fmt.Errorf("unexpected argument%s: %s", suffix, strings.Join(unexpected, ", "))
}

// optionalString validates an optional non-empty string argument. present
// distinguishes an absent key from a present null.
func optionalString(value any, present bool, name string) (*string, error) {
	if !present {
		return nil, nil
	}
	text, ok := value.(string)
	if !ok || text == "" {
		return nil, fmt.Errorf("%s must be a non-empty string", name)
	}
	return &text, nil
}

// requiredString validates a required non-empty string argument.
func requiredString(value any, present bool, name string) (string, error) {
	text, err := optionalString(value, present, name)
	if err != nil {
		return "", err
	}
	if text == nil {
		return "", fmt.Errorf("%s is required", name)
	}
	return *text, nil
}

// optionalInteger validates an optional integer argument against a range,
// applying JavaScript's `Number.isSafeInteger` rule: an integral float
// (`1000.0`, from a host that serializes numbers as floats) is accepted, and
// anything beyond 2^53 is rejected as unrepresentable.
func optionalInteger(value any, present bool, name string, minimum int64, maximum *int64) (*int64, error) {
	rangeError := func() error {
		if maximum != nil {
			return fmt.Errorf("%s must be an integer between %d and %d", name, minimum, *maximum)
		}
		return fmt.Errorf("%s must be an integer at least %d", name, minimum)
	}
	if !present {
		return nil, nil
	}
	var candidate int64
	switch number := value.(type) {
	case float64:
		if number != math.Trunc(number) || math.IsInf(number, 0) || math.IsNaN(number) ||
			math.Abs(number) > float64(maxSafeInteger) {
			return nil, rangeError()
		}
		candidate = int64(number)
	case int:
		candidate = int64(number)
	case int64:
		candidate = number
	case json.Number:
		if integer, err := number.Int64(); err == nil {
			candidate = integer
		} else if float, err := number.Float64(); err == nil &&
			float == math.Trunc(float) && math.Abs(float) <= float64(maxSafeInteger) {
			candidate = int64(float)
		} else {
			return nil, rangeError()
		}
	default:
		return nil, rangeError()
	}
	if abs64(candidate) > maxSafeInteger || candidate < minimum || (maximum != nil && candidate > *maximum) {
		return nil, rangeError()
	}
	return &candidate, nil
}

// abs64 is the absolute value of v.
func abs64(v int64) int64 {
	if v < 0 {
		return -v
	}
	return v
}

// toU64 converts a validated non-negative integer to uint64.
func toU64(value *int64) *uint64 {
	if value == nil {
		return nil
	}
	out := uint64(*value)
	return &out
}

// ---------------------------------------------------------------------------
// Tool parsing
// ---------------------------------------------------------------------------

// requireService validates a service argument against the known ids.
func (s *Server) requireService(value any, present bool) (string, error) {
	service, err := requiredString(value, present, "service")
	if err != nil {
		return "", err
	}
	if s.hasServiceID(service) {
		return service, nil
	}
	return "", fmt.Errorf("unknown service: %s. Known services: %s", service, strings.Join(s.knownServiceIDs(), ", "))
}

// parseStatus parses the `status` tool arguments.
func (s *Server) parseStatus(arguments arguments) (StatusArguments, error) {
	if err := requireOnlyKeys(arguments, "service"); err != nil {
		return StatusArguments{}, err
	}
	var service *string
	if arguments.has("service") {
		value, err := s.requireService(arguments.value("service"), true)
		if err != nil {
			return StatusArguments{}, err
		}
		service = &value
	}
	return StatusArguments{Service: service}, nil
}

// parseLogs parses the `logs` tool arguments.
func (s *Server) parseLogs(arguments arguments) (LogsArguments, error) {
	if err := requireOnlyKeys(arguments, "service", "cursor", "generation", "limit"); err != nil {
		return LogsArguments{}, err
	}
	service, err := s.requireService(arguments.value("service"), arguments.has("service"))
	if err != nil {
		return LogsArguments{}, err
	}
	cursor, err := optionalInteger(arguments.value("cursor"), arguments.has("cursor"), "cursor", 0, nil)
	if err != nil {
		return LogsArguments{}, err
	}
	generation, err := optionalInteger(arguments.value("generation"), arguments.has("generation"), "generation", 0, nil)
	if err != nil {
		return LogsArguments{}, err
	}
	limitMax := int64(64 * 1024)
	limit, err := optionalInteger(arguments.value("limit"), arguments.has("limit"), "limit", 1, &limitMax)
	if err != nil {
		return LogsArguments{}, err
	}
	return LogsArguments{
		Service:    service,
		Cursor:     toU64(cursor),
		Generation: toU64(generation),
		Limit:      toU64(limit),
	}, nil
}

// parseTrace parses the `trace` tool arguments.
func (s *Server) parseTrace(arguments arguments) (TraceArguments, error) {
	if err := requireOnlyKeys(arguments, "operationId"); err != nil {
		return TraceArguments{}, err
	}
	operationID, err := requiredString(arguments.value("operationId"), arguments.has("operationId"), "operationId")
	if err != nil {
		return TraceArguments{}, err
	}
	return TraceArguments{OperationID: operationID}, nil
}

// parseEvents parses the `events` tool arguments.
func (s *Server) parseEvents(arguments arguments) (EventsArguments, error) {
	if err := requireOnlyKeys(arguments, "after", "epoch"); err != nil {
		return EventsArguments{}, err
	}
	after, err := optionalInteger(arguments.value("after"), arguments.has("after"), "after", 0, nil)
	if err != nil {
		return EventsArguments{}, err
	}
	epoch, err := optionalString(arguments.value("epoch"), arguments.has("epoch"), "epoch")
	if err != nil {
		return EventsArguments{}, err
	}
	return EventsArguments{After: toU64(after), Epoch: epoch}, nil
}

// parseManage parses the `manage` tool arguments, enforcing the confirm gate.
func (s *Server) parseManage(arguments arguments) (ManageArguments, error) {
	allowed := []string{"service", "action", "killUnowned"}
	if s.requireConfirm() {
		allowed = append(allowed, "confirm")
	}
	if err := requireOnlyKeys(arguments, allowed...); err != nil {
		return ManageArguments{}, err
	}
	if s.requireConfirm() {
		if value, ok := arguments.values["confirm"]; !ok || value != true {
			return ManageArguments{}, fmt.Errorf("manage requires confirm=true (explicit user approval) — never call this speculatively")
		}
	}
	service, err := s.requireService(arguments.value("service"), arguments.has("service"))
	if err != nil {
		return ManageArguments{}, err
	}
	var action state.ServiceOperationKind
	switch value, _ := arguments.value("action").(string); value {
	case "start":
		action = state.OpStart
	case "stop":
		action = state.OpStop
	case "restart":
		action = state.OpRestart
	default:
		return ManageArguments{}, fmt.Errorf("action must be one of: start, stop, restart")
	}
	killUnowned := false
	if value, ok := arguments.values["killUnowned"]; ok {
		boolean, ok := value.(bool)
		if !ok {
			return ManageArguments{}, fmt.Errorf("killUnowned must be a boolean")
		}
		killUnowned = boolean
	}
	if killUnowned && action != state.OpStart {
		return ManageArguments{}, fmt.Errorf("killUnowned only applies to action=start")
	}
	return ManageArguments{Service: service, Action: action, KillUnowned: killUnowned}, nil
}

// parseDaemonLifecycle parses the daemon-lifecycle tools' arguments — no
// arguments beyond the confirm gate, because one daemon per project root means
// there is nothing to select.
func (s *Server) parseDaemonLifecycle(tool string, arguments arguments) error {
	var allowed []string
	if s.requireConfirm() {
		allowed = []string{"confirm"}
	}
	if err := requireOnlyKeys(arguments, allowed...); err != nil {
		return err
	}
	if s.requireConfirm() {
		if value, ok := arguments.values["confirm"]; !ok || value != true {
			return fmt.Errorf("%s requires confirm=true (explicit user approval) — never call this speculatively", tool)
		}
	}
	return nil
}

// dispatch routes one tool call to the client.
func (s *Server) dispatch(ctx context.Context, tool string, arguments arguments) (any, error) {
	if tool == "status" || tool == "logs" || tool == "manage" {
		if service, ok := arguments.value("service").(string); ok && !s.hasServiceID(service) {
			s.refreshServiceIDs(ctx)
		}
	}
	switch tool {
	case "status":
		parsed, err := s.parseStatus(arguments)
		if err != nil {
			return nil, err
		}
		return s.client.Status(ctx, parsed)
	case "logs":
		parsed, err := s.parseLogs(arguments)
		if err != nil {
			return nil, err
		}
		return s.client.Logs(ctx, parsed)
	case "trace":
		parsed, err := s.parseTrace(arguments)
		if err != nil {
			return nil, err
		}
		return s.client.Trace(ctx, parsed)
	case "events":
		parsed, err := s.parseEvents(arguments)
		if err != nil {
			return nil, err
		}
		return s.client.Events(ctx, parsed)
	case "manage":
		parsed, err := s.parseManage(arguments)
		if err != nil {
			return nil, err
		}
		return s.client.Manage(ctx, parsed)
	case "restart_daemon":
		if err := s.parseDaemonLifecycle("restart_daemon", arguments); err != nil {
			return nil, err
		}
		return s.client.RestartDaemon(ctx)
	case "stop_daemon":
		if err := s.parseDaemonLifecycle("stop_daemon", arguments); err != nil {
			return nil, err
		}
		return s.client.StopDaemon(ctx)
	case "shared_list":
		if err := requireOnlyKeys(arguments); err != nil {
			return nil, err
		}
		return s.client.SharedList(ctx)
	case "shared_status":
		if err := requireOnlyKeys(arguments); err != nil {
			return nil, err
		}
		return s.client.SharedStatus(ctx)
	case "shared_connection":
		if err := requireOnlyKeys(arguments, "service"); err != nil {
			return nil, err
		}
		service, err := requiredString(arguments.value("service"), arguments.has("service"), "service")
		if err != nil {
			return nil, err
		}
		return s.client.SharedConnection(ctx, service)
	default:
		return nil, fmt.Errorf("unknown tool: %s%s", s.options.ToolPrefix, tool)
	}
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

// toolDefinitions builds the ten tools, embedding the current service ids in
// the `manage`/`status`/`logs` schemas.
func (s *Server) toolDefinitions() []any {
	prefix := s.options.ToolPrefix
	requireConfirm := s.requireConfirm()
	serviceIDs := s.knownServiceIDs()

	manageProperties := map[string]any{
		"service": map[string]any{"type": "string", "enum": serviceIDs},
		"action":  map[string]any{"type": "string", "enum": []string{"start", "stop", "restart"}},
		"killUnowned": map[string]any{
			"type":        "boolean",
			"description": "Only for action=start: when the service's port is held by a process this manager does not own, kill that process and continue starting. Never set unless the user explicitly asked to reclaim the port.",
		},
	}
	manageRequired := []string{"service", "action"}
	manageDescription := "Start/stop/restart a local dev service. MCP hosts should require approval for this tool."
	if requireConfirm {
		manageProperties["confirm"] = map[string]any{
			"type":        "boolean",
			"description": "Must be true; the user must have explicitly asked for this action",
		}
		manageRequired = append(manageRequired, "confirm")
		manageDescription = "Start/stop/restart a local dev service. Requires confirm=true (explicit user approval) — never call this speculatively."
	}

	restartProperties := map[string]any{}
	restartRequired := []string{}
	restartDescription := "Restart this project's hearth daemon. Running services are left alone and re-adopted by the new daemon. MCP hosts should require approval for this tool."
	if requireConfirm {
		restartProperties["confirm"] = confirmProperty()
		restartRequired = append(restartRequired, "confirm")
		restartDescription = "Restart this project's hearth daemon. Running services are left alone and re-adopted by the new daemon. Requires confirm=true (explicit user approval) — never call this speculatively."
	}

	stopDaemonProperties := map[string]any{}
	stopDaemonRequired := []string{}
	stopDaemonDescription := "Stop this project's hearth daemon and the services it runs: its own processes, its external services that declare a `stop` command (e.g. a docker compose unit), and its shared services, which are detached from this project; a shared instance is stopped only when no other project is still attached. Externally owned services with no `stop` command keep running. MCP hosts should require approval for this tool."
	if requireConfirm {
		stopDaemonProperties["confirm"] = confirmProperty()
		stopDaemonRequired = append(stopDaemonRequired, "confirm")
		stopDaemonDescription = "Stop this project's hearth daemon and the services it runs: its own processes, its external services that declare a `stop` command (e.g. a docker compose unit), and its shared services, which are detached from this project; a shared instance is stopped only when no other project is still attached. Externally owned services with no `stop` command keep running. Every tool call then fails until a new daemon is started by a client that can spawn one. Requires confirm=true (explicit user approval) — never call this speculatively."
	}

	return []any{
		toolDefinition(prefix+"status", "Read-only status of local dev services. No approval needed.", map[string]any{
			"type": "object",
			"properties": map[string]any{
				"service": map[string]any{
					"type":        "string",
					"enum":        serviceIDs,
					"description": "Service id. Omit to list all.",
				},
			},
			"additionalProperties": false,
		}),
		toolDefinition(prefix+"logs", "Read-only bounded log chunk for one service. No approval needed.", map[string]any{
			"type": "object",
			"properties": map[string]any{
				"service": map[string]any{"type": "string", "enum": serviceIDs},
				"cursor":  map[string]any{"type": "integer", "minimum": 0},
				"generation": map[string]any{
					"type":        "integer",
					"minimum":     0,
					"description": "Log cursor generation. 0 is valid: a service with no state row reports lifecycle generation 0.",
				},
				"limit": map[string]any{"type": "integer", "minimum": 1, "maximum": 64 * 1024},
			},
			"required":             []string{"service"},
			"additionalProperties": false,
		}),
		toolDefinition(prefix+"trace", "Read-only trace/status of a start/stop/restart operation by id. No approval needed.", map[string]any{
			"type": "object",
			"properties": map[string]any{
				"operationId": map[string]any{"type": "string"},
			},
			"required":             []string{"operationId"},
			"additionalProperties": false,
		}),
		toolDefinition(prefix+"events", "Read-only recent manager events since a sequence number. Not a follow/SSE stream. No approval needed.", map[string]any{
			"type": "object",
			"properties": map[string]any{
				"after": map[string]any{"type": "integer", "minimum": 0},
				"epoch": map[string]any{"type": "string"},
			},
			"additionalProperties": false,
		}),
		toolDefinition(prefix+"manage", manageDescription, map[string]any{
			"type":                 "object",
			"properties":           manageProperties,
			"required":             manageRequired,
			"additionalProperties": false,
		}),
		toolDefinition(prefix+"restart_daemon", restartDescription, map[string]any{
			"type":                 "object",
			"properties":           restartProperties,
			"required":             restartRequired,
			"additionalProperties": false,
		}),
		toolDefinition(prefix+"stop_daemon", stopDaemonDescription, map[string]any{
			"type":                 "object",
			"properties":           stopDaemonProperties,
			"required":             stopDaemonRequired,
			"additionalProperties": false,
		}),
		toolDefinition(prefix+"shared_list", "Read-only: services and versions available from the shared-services registry (installed on this machine by smp on demand, shared across projects). No approval needed.", map[string]any{
			"type":                 "object",
			"properties":           map[string]any{},
			"additionalProperties": false,
		}),
		toolDefinition(prefix+"shared_status", "Read-only: shared service instances on this machine — ports, install state, and which projects are attached. No approval needed.", map[string]any{
			"type":                 "object",
			"properties":           map[string]any{},
			"additionalProperties": false,
		}),
		toolDefinition(prefix+"shared_connection", "Read-only: this project's connection info (url/env) for a shared service it has attached via `shared:` in hearth.yaml. Pass the service id (e.g. \"postgres\") or the instance id (\"postgres@16.4\"). No approval needed.", map[string]any{
			"type": "object",
			"properties": map[string]any{
				"service": map[string]any{"type": "string"},
			},
			"required":             []string{"service"},
			"additionalProperties": false,
		}),
	}
}

// confirmProperty is the shared `confirm` schema property.
func confirmProperty() map[string]any {
	return map[string]any{
		"type":        "boolean",
		"description": "Must be true; the user must have explicitly asked for this action",
	}
}

// toolDefinition builds one tool's wire object.
func toolDefinition(name, description string, inputSchema map[string]any) map[string]any {
	return map[string]any{"name": name, "description": description, "inputSchema": inputSchema}
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

// redact strips anything that looks like a credential out of a value before it
// reaches the model — it recurses through arrays/objects, dropping any object
// key matching secretKeyPattern.
func redact(value any) any {
	return redactAtDepth(value, 0)
}

// redactToolResult is redact, except for `shared_connection`'s `connection`
// subtree: handing the agent a shared service's env values
// (`AWS_SECRET_ACCESS_KEY`, a `DATABASE_URL` password) is that tool's whole
// purpose, and they are local-dev constants from the shared catalog, not real
// credentials.
func redactToolResult(tool string, value any) any {
	redacted := redact(value)
	if tool != "shared_connection" {
		return redacted
	}
	object, ok := redacted.(map[string]any)
	if !ok {
		return redacted
	}
	original, ok := value.(map[string]any)
	if !ok {
		return redacted
	}
	if connection, ok := original["connection"]; ok {
		object["connection"] = connection
	}
	return redacted
}

// redactAtDepth recurses through value, truncating past redactDepthCap.
func redactAtDepth(value any, depth int) any {
	if depth > redactDepthCap {
		return "[truncated]"
	}
	switch v := value.(type) {
	case []any:
		out := make([]any, len(v))
		for i, entry := range v {
			out[i] = redactAtDepth(entry, depth+1)
		}
		return out
	case map[string]any:
		out := make(map[string]any, len(v))
		for key, entry := range v {
			if secretKeyPattern.MatchString(key) {
				continue
			}
			out[key] = redactAtDepth(entry, depth+1)
		}
		return out
	default:
		return value
	}
}

// safeErrorMessage replaces any `Bearer <token>` credential in an error
// message before it reaches the model.
func safeErrorMessage(message string) string {
	return bearerPattern.ReplaceAllString(message, "Bearer [redacted]")
}
