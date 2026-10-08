// Ported from rust/crates/hearth-mcp/tests/end_to_end.rs: a real bootstrapped
// HearthManager, a real `nc -lk`-backed TCP service, driven entirely over the
// real MCP tool surface — an in-process stdio client/server pair, the Go
// stand-in for rmcp's `tokio::io::duplex` transport.
package mcpserver

import (
	"encoding/json"
	"fmt"
	"net"
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/manager"
)

// freePort reserves and releases a loopback port for the test service.
func freePort(t *testing.T) uint16 {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("free port: %v", err)
	}
	port := uint16(listener.Addr().(*net.TCPAddr).Port)
	_ = listener.Close()
	return port
}

// tcpService is a daemon-owned service whose run command is `exec nc -lk
// <port>` and whose readiness is that TCP port.
func tcpService(id string, port uint16) catalog.ServiceDefinition {
	kind := catalog.KindApplication
	label := "app"
	exec := true
	timeout := uint64(5_000)
	return catalog.ServiceDefinition{
		ID:   id,
		Kind: &kind,
		Profiles: catalog.ServiceProfiles{
			Run: catalog.ServiceRunProfile{
				CommandStatus: "verified",
				Command: &catalog.ServiceCommand{
					Command: catalog.CommandSpec{Shell: fmt.Sprintf("exec nc -lk %d", port), Exec: &exec},
					// The supervisor joins root and cwd, so "." runs in the
					// project root.
					Cwd: ".",
				},
				Readiness:          catalog.ReadinessSpec{Kind: "tcp", Port: &port},
				ReadinessTimeoutMs: &timeout,
			},
		},
		URLs: []catalog.ServiceURL{{URL: "http://127.0.0.1:18090/", Label: &label}},
	}
}

// parseToolJSON decodes a tool result's text block as a JSON object.
func parseToolJSON(t *testing.T, result map[string]any) map[string]any {
	t.Helper()
	var out map[string]any
	if err := json.Unmarshal([]byte(toolText(t, result)), &out); err != nil {
		t.Fatalf("parse tool text: %v", err)
	}
	return out
}

// serviceRow digs `services.services[0]` out of a status/manage reply.
func serviceRow(t *testing.T, payload map[string]any) map[string]any {
	t.Helper()
	services, _ := payload["services"].(map[string]any)
	rows, _ := services["services"].([]any)
	if len(rows) == 0 {
		t.Fatalf("no service rows in %v", payload)
	}
	row, _ := rows[0].(map[string]any)
	return row
}

func TestFullToolLifecycleOverARealBootstrappedManager(t *testing.T) {
	root := t.TempDir()
	port := freePort(t)
	cat := catalog.ServiceCatalog{
		Services:           []catalog.ServiceDefinition{tcpService("api", port)},
		Groups:             map[string][]string{},
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
		PrivateFileGuard:   new(false),
	}
	mgr, err := manager.Bootstrap(manager.HearthManagerOptions{
		Root:    &root,
		Catalog: cat,
	})
	if err != nil {
		t.Fatalf("bootstrap: %v", err)
	}
	defer (&manager.StopServicesOnDrop{Manager: mgr}).Stop()

	mcpClient := NewManagerApiClient(root, ClientOptions{
		Catalog:     cat,
		SpawnDaemon: func(string) { t.Error("a running manager should never need spawning") },
	})
	server := New(mcpClient, Options{
		Name:            "hearth",
		RequireConfirm:  new(true),
		KnownServiceIDs: []string{"api"},
	})
	client := startServer(t, server)

	// status — fresh catalog, nothing started yet.
	status := client.callTool("status", nil)
	if isError(status) {
		t.Fatalf("status: %s", toolText(t, status))
	}
	statusJSON := parseToolJSON(t, status)
	if state := serviceRow(t, statusJSON)["actualState"]; state != "stopped" {
		t.Errorf("status actualState = %v, want stopped", state)
	}
	// `urls` rides along on `status`, so an agent can tell a user where a
	// service lives.
	urls, _ := statusJSON["urls"].(map[string]any)
	urlRows, _ := urls["urls"].([]any)
	if len(urlRows) == 0 {
		t.Fatalf("no urls in %v", statusJSON)
	}
	urlRow, _ := urlRows[0].(map[string]any)
	if urlRow["url"] != "http://127.0.0.1:18090/" {
		t.Errorf("url = %v", urlRow["url"])
	}
	if urlRow["label"] != "app" {
		t.Errorf("label = %v", urlRow["label"])
	}

	// manage start — waits for the real nc-backed process to answer real TCP
	// readiness, then returns the post-start status inline.
	manage := client.callTool("manage", map[string]any{"service": "api", "action": "start", "confirm": true})
	if isError(manage) {
		t.Fatalf("manage start: %s", toolText(t, manage))
	}
	ready := serviceRow(t, parseToolJSON(t, manage))
	if state := ready["actualState"]; state != "ready" {
		t.Errorf("manage start actualState = %v, want ready", state)
	}
	identity, _ := ready["identity"].(map[string]any)
	if pid, _ := identity["pid"].(float64); pid <= 0 {
		t.Errorf("identity.pid = %v, want > 0", identity["pid"])
	}

	// logs — real, bounded log chunk for the started service.
	logs := client.callTool("logs", map[string]any{"service": "api"})
	if isError(logs) {
		t.Fatalf("logs: %s", toolText(t, logs))
	}

	// events — real recent manager events since sequence 0.
	events := client.callTool("events", map[string]any{"after": 0})
	if isError(events) {
		t.Fatalf("events: %s", toolText(t, events))
	}
	eventsJSON := parseToolJSON(t, events)
	eventRows, _ := eventsJSON["events"].([]any)
	foundLifecycle := false
	for _, event := range eventRows {
		if object, ok := event.(map[string]any); ok && object["type"] == "service.lifecycle" {
			foundLifecycle = true
		}
	}
	if !foundLifecycle {
		t.Errorf("no service.lifecycle event in %v", eventsJSON)
	}

	// trace — a made-up operation id should surface as a tool-level error, not
	// a protocol error.
	trace := client.callTool("trace", map[string]any{"operationId": "does-not-exist"})
	if !isError(trace) {
		t.Error("trace of an unknown operation must be a tool error")
	}

	// manage stop — real process should really exit.
	stop := client.callTool("manage", map[string]any{"service": "api", "action": "stop", "confirm": true})
	if isError(stop) {
		t.Fatalf("manage stop: %s", toolText(t, stop))
	}
	if state := serviceRow(t, parseToolJSON(t, stop))["actualState"]; state != "stopped" {
		t.Errorf("manage stop actualState = %v, want stopped", state)
	}

	mgr.Close()
}
