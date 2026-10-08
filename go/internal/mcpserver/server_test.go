// Ported from rust/crates/hearth-mcp/src/server.rs's `mod tests`: the tool
// surface driven over an in-process stdio client/server pair (the Go stand-in
// for rmcp's `tokio::io::duplex` transport).
package mcpserver

import (
	"bufio"
	"context"
	"encoding/json"
	"io"
	"strings"
	"sync"
	"testing"

	"github.com/ngosangns/hearth/go/internal/state"
)

// ---------------------------------------------------------------------------
// In-process stdio client
// ---------------------------------------------------------------------------

// testClient drives a Server over newline-delimited JSON-RPC, the way an MCP
// host does.
type testClient struct {
	t       *testing.T
	toSrv   *io.PipeWriter
	fromSrv *bufio.Reader
	nextID  int
}

// startServer runs srv on a pair of pipes and completes the initialize
// handshake.
func startServer(t *testing.T, srv *Server) *testClient {
	t.Helper()
	toSrvR, toSrvW := io.Pipe()
	fromSrvR, fromSrvW := io.Pipe()
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	go func() {
		defer close(done)
		_ = srv.Serve(ctx, toSrvR, fromSrvW)
	}()
	t.Cleanup(func() {
		_ = toSrvW.Close()
		cancel()
		<-done
	})
	client := &testClient{t: t, toSrv: toSrvW, fromSrv: bufio.NewReader(fromSrvR)}
	client.initialize()
	return client
}

// initialize performs the MCP handshake: `initialize`, then the
// `notifications/initialized` notification.
func (c *testClient) initialize() {
	c.t.Helper()
	response := c.request("initialize", map[string]any{
		"protocolVersion": "2025-06-18",
		"capabilities":    map[string]any{},
		"clientInfo":      map[string]any{"name": "test", "version": "1.0"},
	})
	result, ok := response["result"].(map[string]any)
	if !ok {
		c.t.Fatalf("initialize: no result: %v", response)
	}
	if result["protocolVersion"] != "2025-06-18" {
		c.t.Fatalf("initialize: protocolVersion = %v, want 2025-06-18", result["protocolVersion"])
	}
	capabilities, _ := result["capabilities"].(map[string]any)
	if _, ok := capabilities["tools"]; !ok {
		c.t.Fatalf("initialize: capabilities.tools missing: %v", result)
	}
	serverInfo, _ := result["serverInfo"].(map[string]any)
	if serverInfo["name"] == nil || serverInfo["version"] == nil {
		c.t.Fatalf("initialize: serverInfo incomplete: %v", result)
	}
	c.notify("notifications/initialized", nil)
}

// request sends a JSON-RPC request and returns the decoded response.
func (c *testClient) request(method string, params any) map[string]any {
	c.t.Helper()
	c.nextID++
	id := c.nextID
	message := map[string]any{"jsonrpc": "2.0", "id": id, "method": method}
	if params != nil {
		message["params"] = params
	}
	c.write(message)
	return c.readResponse(id)
}

// notify sends a JSON-RPC notification (no id, no response).
func (c *testClient) notify(method string, params any) {
	c.t.Helper()
	message := map[string]any{"jsonrpc": "2.0", "method": method}
	if params != nil {
		message["params"] = params
	}
	c.write(message)
}

func (c *testClient) write(message map[string]any) {
	c.t.Helper()
	data, err := json.Marshal(message)
	if err != nil {
		c.t.Fatalf("marshal: %v", err)
	}
	if _, err := c.toSrv.Write(append(data, '\n')); err != nil {
		c.t.Fatalf("write: %v", err)
	}
}

func (c *testClient) readResponse(id int) map[string]any {
	c.t.Helper()
	line, err := c.fromSrv.ReadBytes('\n')
	if err != nil {
		c.t.Fatalf("read: %v", err)
	}
	var response map[string]any
	if err := json.Unmarshal(line, &response); err != nil {
		c.t.Fatalf("unmarshal response %q: %v", line, err)
	}
	if got, _ := response["id"].(float64); int(got) != id {
		c.t.Fatalf("response id = %v, want %d (%s)", response["id"], id, line)
	}
	return response
}

// callTool sends a `tools/call` and returns its result object.
func (c *testClient) callTool(name string, arguments map[string]any) map[string]any {
	c.t.Helper()
	params := map[string]any{"name": name}
	if arguments != nil {
		params["arguments"] = arguments
	}
	response := c.request("tools/call", params)
	result, ok := response["result"].(map[string]any)
	if !ok {
		c.t.Fatalf("tools/call %s: no result: %v", name, response)
	}
	return result
}

// listTools sends `tools/list` and returns the tool objects in order.
func (c *testClient) listTools() []map[string]any {
	c.t.Helper()
	response := c.request("tools/list", nil)
	result, _ := response["result"].(map[string]any)
	tools, _ := result["tools"].([]any)
	out := make([]map[string]any, 0, len(tools))
	for _, tool := range tools {
		if object, ok := tool.(map[string]any); ok {
			out = append(out, object)
		}
	}
	return out
}

// toolText is the first text content block of a tool result.
func toolText(t *testing.T, result map[string]any) string {
	t.Helper()
	content, _ := result["content"].([]any)
	if len(content) == 0 {
		t.Fatalf("no content: %v", result)
	}
	block, _ := content[0].(map[string]any)
	text, _ := block["text"].(string)
	return text
}

// isError reports whether a tool result is an error.
func isError(result map[string]any) bool {
	value, _ := result["isError"].(bool)
	return value
}

// ---------------------------------------------------------------------------
// Fake client
// ---------------------------------------------------------------------------

type fakeClient struct {
	NoSharedServices

	mu         sync.Mutex
	managed    *ManageArguments
	restarts   int
	stops      int
	serviceIDs []string
	hasIDs     bool
}

func (f *fakeClient) Status(context.Context, StatusArguments) (any, error) {
	return map[string]any{}, nil
}

func (f *fakeClient) Logs(context.Context, LogsArguments) (any, error) {
	return map[string]any{}, nil
}

func (f *fakeClient) Trace(context.Context, TraceArguments) (any, error) {
	return map[string]any{}, nil
}

func (f *fakeClient) Events(context.Context, EventsArguments) (any, error) {
	return map[string]any{}, nil
}

func (f *fakeClient) Manage(_ context.Context, arguments ManageArguments) (any, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.managed = &arguments
	return map[string]any{"operation": "accepted"}, nil
}

func (f *fakeClient) RestartDaemon(context.Context) (any, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.restarts++
	return map[string]any{"instanceId": "restarted-instance"}, nil
}

func (f *fakeClient) StopDaemon(context.Context) (any, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.stops++
	return map[string]any{"operation": "accepted"}, nil
}

func (f *fakeClient) SharedConnection(_ context.Context, service string) (any, error) {
	return map[string]any{
		"service":    service,
		"projectId":  "p",
		"connection": map[string]any{"env": map[string]any{"AWS_SECRET_ACCESS_KEY": "minioadmin"}},
		"token":      "must-not-leak",
	}, nil
}

func (f *fakeClient) ServiceIDs(context.Context) ([]string, bool) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if !f.hasIDs {
		return nil, false
	}
	return f.serviceIDs, true
}

func (f *fakeClient) managedArguments() *ManageArguments {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.managed
}

func (f *fakeClient) setServiceIDs(ids []string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.serviceIDs = ids
	f.hasIDs = true
}

func (f *fakeClient) restartCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.restarts
}

func (f *fakeClient) stopCount() int {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.stops
}

// baseOptions is the Rust `base_options()`.
func baseOptions() Options {
	return Options{
		Name:            "test-hearth",
		ToolPrefix:      "local_services_",
		RequireConfirm:  new(true),
		KnownServiceIDs: []string{"metadata", "mongo"},
	}
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

func TestRoutesFocusedApplicationManagementThroughTheOrdinaryManagerClient(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	result := client.callTool("local_services_manage", map[string]any{
		"service": "metadata", "action": "restart", "confirm": true,
	})
	if isError(result) {
		t.Fatalf("unexpected error: %s", toolText(t, result))
	}
	managed := fake.managedArguments()
	if managed == nil {
		t.Fatal("manage never reached the client")
	}
	if managed.Service != "metadata" {
		t.Errorf("service = %q, want metadata", managed.Service)
	}
	if managed.Action != state.OpRestart {
		t.Errorf("action = %q, want restart", managed.Action)
	}
}

func TestRoutesFocusedInfrastructureManagementThroughTheOrdinaryManagerClient(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	result := client.callTool("local_services_manage", map[string]any{
		"service": "mongo", "action": "restart", "confirm": true,
	})
	if isError(result) {
		t.Fatalf("unexpected error: %s", toolText(t, result))
	}
	managed := fake.managedArguments()
	if managed == nil {
		t.Fatal("manage never reached the client")
	}
	if managed.Service != "mongo" {
		t.Errorf("service = %q, want mongo", managed.Service)
	}
	if managed.Action != state.OpRestart {
		t.Errorf("action = %q, want restart", managed.Action)
	}
}

// killUnowned is the MCP echo of a user's "yes, kill the port-holder" — it
// must parse, pass the same confirm gate, and reach the client verbatim.
func TestManageAcceptsKillUnownedForStart(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	result := client.callTool("local_services_manage", map[string]any{
		"service": "metadata", "action": "start", "confirm": true, "killUnowned": true,
	})
	if isError(result) {
		t.Fatalf("unexpected error: %s", toolText(t, result))
	}
	managed := fake.managedArguments()
	if managed == nil {
		t.Fatal("manage never reached the client")
	}
	if !managed.KillUnowned {
		t.Error("killUnowned did not reach the client")
	}
}

// Reclaim is a start-only operation — killUnowned on stop/restart is a usage
// error, and it must never reach the client.
func TestManageRejectsKillUnownedForStopAndRestart(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	for _, action := range []string{"stop", "restart"} {
		result := client.callTool("local_services_manage", map[string]any{
			"service": "metadata", "action": action, "confirm": true, "killUnowned": true,
		})
		if !isError(result) {
			t.Errorf("killUnowned on action=%s must be rejected", action)
		}
	}
	if fake.managedArguments() != nil {
		t.Error("manage reached the client")
	}
}

func TestRejectsUnexpectedExtraArguments(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	result := client.callTool("local_services_manage", map[string]any{
		"service": "metadata", "action": "restart", "confirm": true, "profile": "dev",
	})
	if !isError(result) {
		t.Fatal("extra argument must be rejected")
	}
	if text := toolText(t, result); text != "unexpected argument: profile" {
		t.Errorf("text = %q, want %q", text, "unexpected argument: profile")
	}
}

// The unexpected-argument message keeps the client's key order (serde_json's
// preserve_order), not a sorted one.
func TestUnexpectedArgumentMessageKeepsClientKeyOrder(t *testing.T) {
	client := startServer(t, New(&fakeClient{}, baseOptions()))
	line := `{"jsonrpc":"2.0","id":99,"method":"tools/call","params":{"name":"local_services_manage","arguments":{"service":"metadata","action":"restart","confirm":true,"zeta":1,"alpha":2}}}`
	if _, err := client.toSrv.Write([]byte(line + "\n")); err != nil {
		t.Fatalf("write: %v", err)
	}
	response := client.readResponse(99)
	result, _ := response["result"].(map[string]any)
	if text := toolText(t, result); text != "unexpected arguments: zeta, alpha" {
		t.Errorf("text = %q, want %q", text, "unexpected arguments: zeta, alpha")
	}
}

func TestRejectsAnUnknownServiceID(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	result := client.callTool("local_services_manage", map[string]any{
		"service": "not-a-real-service", "action": "restart", "confirm": true,
	})
	if !isError(result) {
		t.Fatal("unknown service must be rejected")
	}
}

func TestManageRequiresConfirmTrueByDefault(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	result := client.callTool("local_services_manage", map[string]any{
		"service": "metadata", "action": "restart",
	})
	if !isError(result) {
		t.Fatal("manage without confirm must be rejected")
	}
	if fake.managedArguments() != nil {
		t.Error("manage reached the client")
	}
}

func TestManageSkipsTheConfirmRequirementWhenDisabled(t *testing.T) {
	fake := &fakeClient{}
	options := baseOptions()
	options.RequireConfirm = new(false)
	client := startServer(t, New(fake, options))
	result := client.callTool("local_services_manage", map[string]any{
		"service": "metadata", "action": "restart",
	})
	if isError(result) {
		t.Fatalf("unexpected error: %s", toolText(t, result))
	}
	managed := fake.managedArguments()
	if managed == nil {
		t.Fatal("manage never reached the client")
	}
	if managed.Service != "metadata" || managed.Action != state.OpRestart {
		t.Errorf("managed = %+v", managed)
	}
}

func TestReadOnlyToolsNeedNoConfirmAndAreRegisteredUnderTheConfiguredPrefix(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	tools := client.listTools()
	names := make([]string, 0, len(tools))
	for _, tool := range tools {
		name, _ := tool["name"].(string)
		names = append(names, name)
	}
	want := []string{
		"local_services_status",
		"local_services_logs",
		"local_services_trace",
		"local_services_events",
		"local_services_manage",
		"local_services_restart_daemon",
		"local_services_stop_daemon",
		"local_services_shared_list",
		"local_services_shared_status",
		"local_services_shared_connection",
	}
	if len(names) != len(want) {
		t.Fatalf("tools = %v, want %v", names, want)
	}
	for i := range want {
		if names[i] != want[i] {
			t.Fatalf("tools = %v, want %v", names, want)
		}
	}
	result := client.callTool("local_services_status", nil)
	if isError(result) {
		t.Fatalf("status without confirm must succeed: %s", toolText(t, result))
	}
}

func TestRestartDaemonRoutesThroughTheClientAndReturnsItsPayload(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	result := client.callTool("local_services_restart_daemon", map[string]any{"confirm": true})
	if isError(result) {
		t.Fatalf("unexpected error: %s", toolText(t, result))
	}
	if fake.restartCount() != 1 {
		t.Errorf("restarts = %d, want 1", fake.restartCount())
	}
	if text := toolText(t, result); !contains(text, "restarted-instance") {
		t.Errorf("text = %q, want it to contain restarted-instance", text)
	}
}

func TestRestartDaemonRequiresConfirmTrueByDefault(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	result := client.callTool("local_services_restart_daemon", nil)
	if !isError(result) {
		t.Fatal("restart_daemon without confirm must be rejected")
	}
	if fake.restartCount() != 0 {
		t.Error("an unconfirmed call must never reach the daemon")
	}
}

func TestRestartDaemonSkipsTheConfirmRequirementWhenDisabled(t *testing.T) {
	fake := &fakeClient{}
	options := baseOptions()
	options.RequireConfirm = new(false)
	client := startServer(t, New(fake, options))
	result := client.callTool("local_services_restart_daemon", nil)
	if isError(result) {
		t.Fatalf("unexpected error: %s", toolText(t, result))
	}
	if fake.restartCount() != 1 {
		t.Errorf("restarts = %d, want 1", fake.restartCount())
	}
}

// stop_daemon is the destructive counterpart: daemon AND services go down, so
// it sits behind the same confirm gate and routes through the same client.
func TestStopDaemonRoutesThroughTheClient(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	result := client.callTool("local_services_stop_daemon", map[string]any{"confirm": true})
	if isError(result) {
		t.Fatalf("unexpected error: %s", toolText(t, result))
	}
	if fake.stopCount() != 1 {
		t.Errorf("stops = %d, want 1", fake.stopCount())
	}
}

func TestStopDaemonRequiresConfirmTrueByDefault(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	result := client.callTool("local_services_stop_daemon", nil)
	if !isError(result) {
		t.Fatal("stop_daemon without confirm must be rejected")
	}
	if fake.stopCount() != 0 {
		t.Error("an unconfirmed call must never reach the daemon")
	}
}

func TestStopDaemonSkipsTheConfirmRequirementWhenDisabled(t *testing.T) {
	fake := &fakeClient{}
	options := baseOptions()
	options.RequireConfirm = new(false)
	client := startServer(t, New(fake, options))
	result := client.callTool("local_services_stop_daemon", nil)
	if isError(result) {
		t.Fatalf("unexpected error: %s", toolText(t, result))
	}
	if fake.stopCount() != 1 {
		t.Errorf("stops = %d, want 1", fake.stopCount())
	}
}

// A service added by `manager reload` after the server started is accepted
// without an MCP server restart, and shows up in the tool schemas.
func TestAReloadedCatalogIsPickedUpWithoutARestart(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	fake.setServiceIDs([]string{"metadata", "mongo", "search"})
	result := client.callTool("local_services_manage", map[string]any{
		"service": "search", "action": "start", "confirm": true,
	})
	if isError(result) {
		t.Fatalf("unexpected error: %s", toolText(t, result))
	}
	managed := fake.managedArguments()
	if managed == nil || managed.Service != "search" {
		t.Fatalf("managed = %+v, want service search", managed)
	}
	var manage map[string]any
	for _, tool := range client.listTools() {
		if tool["name"] == "local_services_manage" {
			manage = tool
		}
	}
	if manage == nil {
		t.Fatal("manage tool missing")
	}
	schema, err := json.Marshal(manage["inputSchema"])
	if err != nil {
		t.Fatalf("marshal schema: %v", err)
	}
	if !contains(string(schema), "search") {
		t.Errorf("manage schema = %s, want it to contain search", schema)
	}
}

// shared_connection exists to hand the agent a shared service's env — its
// `connection` subtree is not redacted, while the rest of the reply still is.
func TestSharedConnectionKeepsItsConnectionEnvButRedactsEverythingElse(t *testing.T) {
	fake := &fakeClient{}
	client := startServer(t, New(fake, baseOptions()))
	result := client.callTool("local_services_shared_connection", map[string]any{"service": "minio"})
	if isError(result) {
		t.Fatalf("unexpected error: %s", toolText(t, result))
	}
	text := toolText(t, result)
	if !contains(text, "AWS_SECRET_ACCESS_KEY") {
		t.Errorf("text = %q, want it to contain AWS_SECRET_ACCESS_KEY", text)
	}
	if contains(text, "must-not-leak") {
		t.Errorf("text = %q, must not leak the token", text)
	}
}

// ping is a liveness probe: an empty result, no error.
func TestPingReturnsAnEmptyResult(t *testing.T) {
	client := startServer(t, New(&fakeClient{}, baseOptions()))
	response := client.request("ping", nil)
	result, ok := response["result"].(map[string]any)
	if !ok {
		t.Fatalf("ping: no result: %v", response)
	}
	if len(result) != 0 {
		t.Errorf("ping result = %v, want {}", result)
	}
}

// The daemon-lifecycle schemas demand `confirm` only when the gate is on, and
// their `required` is `[]` (never `null`) when it is off.
func TestDaemonLifecycleSchemasRequireConfirmOnlyWhenEnabled(t *testing.T) {
	names := []string{"local_services_restart_daemon", "local_services_stop_daemon"}

	client := startServer(t, New(&fakeClient{}, baseOptions()))
	for _, name := range names {
		required, _ := schemaOf(t, client.listTools(), name)["required"].([]any)
		if len(required) != 1 || required[0] != "confirm" {
			t.Errorf("%s required = %v, want [confirm]", name, required)
		}
	}

	options := baseOptions()
	options.RequireConfirm = new(false)
	client = startServer(t, New(&fakeClient{}, options))
	for _, name := range names {
		required, ok := schemaOf(t, client.listTools(), name)["required"].([]any)
		if !ok || len(required) != 0 {
			t.Errorf("%s required = %v, want []", name, schemaOf(t, client.listTools(), name)["required"])
		}
	}
}

// schemaOf finds a tool's input schema by name.
func schemaOf(t *testing.T, tools []map[string]any, name string) map[string]any {
	t.Helper()
	for _, tool := range tools {
		if tool["name"] == name {
			schema, _ := tool["inputSchema"].(map[string]any)
			return schema
		}
	}
	t.Fatalf("tool %s not found", name)
	return nil
}

// An unknown method is a JSON-RPC method-not-found error, not a tool error.
func TestUnknownMethodIsAMethodNotFoundError(t *testing.T) {
	client := startServer(t, New(&fakeClient{}, baseOptions()))
	response := client.request("does/not/exist", nil)
	errObject, ok := response["error"].(map[string]any)
	if !ok {
		t.Fatalf("expected an error response: %v", response)
	}
	if code, _ := errObject["code"].(float64); int(code) != -32601 {
		t.Errorf("code = %v, want -32601", errObject["code"])
	}
}

// A client naming a version the server cannot serve over `initialize` is
// answered with the server's newest legacy version.
func TestInitializeFallsBackForAnUnsupportedProtocolVersion(t *testing.T) {
	client := startServer(t, New(&fakeClient{}, baseOptions()))
	response := client.request("initialize", map[string]any{
		"protocolVersion": "2026-07-28",
		"capabilities":    map[string]any{},
		"clientInfo":      map[string]any{"name": "test", "version": "1.0"},
	})
	result, _ := response["result"].(map[string]any)
	if result["protocolVersion"] != "2025-11-25" {
		t.Errorf("protocolVersion = %v, want 2025-11-25", result["protocolVersion"])
	}
}

// contains is a tiny substring helper for assertions.
func contains(haystack, needle string) bool {
	return strings.Contains(haystack, needle)
}
