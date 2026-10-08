// The default MCP client, backed by the daemon's HTTP API through the shared
// go/internal/client primitives and hearth-cli's target/daemon-lifecycle
// helpers.
//
// Ported from rust/crates/hearth-mcp/src/client.rs: the `HearthMcpClient`
// trait (the dependency-injected, transport-agnostic shape the tool dispatcher
// holds) and `ManagerApiClient`, its default implementation. It reuses
// go/internal/client's `ManagerClient`/`Ensure`/shared helpers and
// go/internal/cli's `RunnableTargets`/`RestartManager`/`StopManager` rather
// than re-implementing HTTP.
package mcpserver

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"strings"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/cli"
	"github.com/ngosangns/hearth/go/internal/client"
	"github.com/ngosangns/hearth/go/internal/shared"
	"github.com/ngosangns/hearth/go/internal/state"
)

// manageWaitTimeout is the Rust MANAGE_WAIT_TIMEOUT: how long `manage` waits
// for its operation before handing the agent the operation id to `trace`
// instead. A probed start settles when the process is up (`ready` or
// `running-unready`), not when the probe first passes.
const manageWaitTimeout = 120 * time.Second

// StatusArguments is the parsed `status` tool arguments.
type StatusArguments struct {
	Service *string
}

// LogsArguments is the parsed `logs` tool arguments.
type LogsArguments struct {
	Service    string
	Cursor     *uint64
	Generation *uint64
	Limit      *uint64
}

// TraceArguments is the parsed `trace` tool arguments.
type TraceArguments struct {
	OperationID string
}

// EventsArguments is the parsed `events` tool arguments.
type EventsArguments struct {
	After *uint64
	Epoch *string
}

// ManageArguments is the parsed `manage` tool arguments.
type ManageArguments struct {
	Service string
	// Action is `start`, `stop` or `restart` — never `status`.
	Action state.ServiceOperationKind
	// KillUnowned is `action: start` only — the host-approved "kill the process
	// holding my port" reclaim, forwarded verbatim to the daemon's
	// `killUnowned` operation flag.
	KillUnowned bool
}

// HearthMcpClient is the dependency-injected MCP client shape the tool
// dispatcher holds: transport-agnostic and unit-testable with a fake, rather
// than reaching for HTTP inline. It mirrors the Rust `HearthMcpClient` trait.
//
// Every method returns a decoded JSON value (the Rust `serde_json::Value`) or
// an error whose message is handed to the model.
type HearthMcpClient interface {
	Status(ctx context.Context, arguments StatusArguments) (any, error)
	Logs(ctx context.Context, arguments LogsArguments) (any, error)
	Trace(ctx context.Context, arguments TraceArguments) (any, error)
	Events(ctx context.Context, arguments EventsArguments) (any, error)
	Manage(ctx context.Context, arguments ManageArguments) (any, error)
	// RestartDaemon restarts this project's daemon. Takes no arguments: there
	// is exactly one daemon per project root, so there is nothing to select.
	RestartDaemon(ctx context.Context) (any, error)
	// StopDaemon stops this project's daemon and its services
	// (`stop-services`): daemon-owned processes, external services with a
	// `stop` command, and `shared:` entries, which detach. Takes no arguments
	// for the same reason — afterwards every other tool fails until a new
	// daemon is ensured by a client that can spawn one (this MCP server
	// deliberately cannot).
	StopDaemon(ctx context.Context) (any, error)
	// SharedList is the remote shared-services registry (`catalog.json`):
	// which services/versions this machine can install. Read-only; the smp
	// daemon is not required.
	SharedList(ctx context.Context) (any, error)
	// SharedStatus is the instances the machine-global smp daemon knows about
	// (ports, install state, attachments).
	SharedStatus(ctx context.Context) (any, error)
	// SharedConnection is this project's rendered connection info for a shared
	// service it has attached — the way an agent learns the `DATABASE_URL`-style
	// values without env injection.
	SharedConnection(ctx context.Context, service string) (any, error)
	// ServiceIDs is the service ids the daemon currently serves, when the
	// client can tell. ok is false when it cannot (the Rust `None`), in which
	// case the server keeps the ids it started with.
	ServiceIDs(ctx context.Context) (ids []string, ok bool)
}

// NoSharedServices implements the shared-services half of HearthMcpClient for
// a client that has none — the Rust trait's default methods. Embed it to get
// those defaults.
type NoSharedServices struct{}

// SharedList reports that the client has no shared-services support.
func (NoSharedServices) SharedList(context.Context) (any, error) {
	return nil, fmt.Errorf("shared services are not supported by this client")
}

// SharedStatus reports that the client has no shared-services support.
func (NoSharedServices) SharedStatus(context.Context) (any, error) {
	return nil, fmt.Errorf("shared services are not supported by this client")
}

// SharedConnection reports that the client has no shared-services support.
func (NoSharedServices) SharedConnection(context.Context, string) (any, error) {
	return nil, fmt.Errorf("shared services are not supported by this client")
}

// ServiceIDs reports that the client cannot tell which services the daemon
// serves.
func (NoSharedServices) ServiceIDs(context.Context) ([]string, bool) { return nil, false }

// ClientOptions configures ManagerApiClient. It mirrors hearth-cli's
// `LocalctlOptions`: Catalog locates the daemon (its runtime directory) and
// answers `runnable_targets`/`resolve_shared_instance_id`; SpawnDaemon starts a
// detached daemon when `restart_daemon` needs a fresh one; Doer overrides the
// HTTP adapter for tests.
type ClientOptions struct {
	Catalog     catalog.ServiceCatalog
	SpawnDaemon func(root string)
	Doer        client.HTTPDoer
}

// cliOptions is the hearth-cli options these helpers take.
func (o ClientOptions) cliOptions() *cli.Options {
	return &cli.Options{Catalog: o.Catalog, SpawnDaemon: o.SpawnDaemon}
}

// ManagerApiClient is the default HearthMcpClient, backed by the daemon's HTTP
// API.
type ManagerApiClient struct {
	root    string
	options ClientOptions
	api     *client.ManagerClient
}

// NewManagerApiClient builds a client for root. catalog only locates the
// daemon; it is never sent anywhere.
func NewManagerApiClient(root string, options ClientOptions) *ManagerApiClient {
	m := &ManagerApiClient{root: root, options: options}
	m.api = client.NewManagerClient(root, &m.options.Catalog)
	m.api.Doer = options.Doer
	return m
}

// call is a raw GET against the cached connection.
func (m *ManagerApiClient) call(ctx context.Context, path string) (map[string]any, error) {
	return m.api.Request(ctx, path, http.MethodGet, nil)
}

// currentCatalog is the catalog the daemon serves now (it may have been
// reloaded since this process started), falling back to the startup catalog
// when the daemon can't be asked.
func (m *ManagerApiClient) currentCatalog(ctx context.Context) *catalog.ServiceCatalog {
	cat, err := m.api.Catalog(ctx)
	if err != nil {
		return &m.options.Catalog
	}
	return cat
}

// Status is the `status` tool: `/v1/manager` + `/v1/services`, with `/v1/urls`
// merged in best-effort (a daemon from before `/v1/urls` existed still answers
// `status`, just without `urls`, rather than failing the whole call).
func (m *ManagerApiClient) Status(ctx context.Context, arguments StatusArguments) (any, error) {
	manager, err := m.call(ctx, "/v1/manager")
	if err != nil {
		return nil, err
	}
	services, err := m.call(ctx, "/v1/services")
	if err != nil {
		return nil, err
	}
	result := map[string]any{
		"manager":  manager,
		"services": filterServiceStates(services, arguments.Service),
	}
	if urls, err := m.call(ctx, "/v1/urls"); err == nil {
		result["urls"] = filterServiceURLs(urls, arguments.Service)
	}
	return result, nil
}

// Logs is the `logs` tool: one bounded log chunk for a service.
func (m *ManagerApiClient) Logs(ctx context.Context, arguments LogsArguments) (any, error) {
	slice, err := m.api.Log(ctx, arguments.Service, arguments.Cursor, arguments.Generation, arguments.Limit)
	if err != nil {
		return nil, err
	}
	return jsonValue(slice), nil
}

// Trace is the `trace` tool: `/v1/operations/:id`.
func (m *ManagerApiClient) Trace(ctx context.Context, arguments TraceArguments) (any, error) {
	return m.call(ctx, "/v1/operations/"+client.EncodePathSegment(arguments.OperationID))
}

// Events is the `events` tool: `/v1/events?after=&epoch=`.
func (m *ManagerApiClient) Events(ctx context.Context, arguments EventsArguments) (any, error) {
	query := queryString([]queryPair{
		{"after", u64String(arguments.After)},
		{"epoch", arguments.Epoch},
	})
	return m.call(ctx, "/v1/events"+query)
}

// Manage submits the operation and waits up to manageWaitTimeout. Still
// running after that is not an error: the reply carries the operation so the
// agent can `trace` it.
func (m *ManagerApiClient) Manage(ctx context.Context, arguments ManageArguments) (any, error) {
	if _, err := cli.RunnableTargets(m.currentCatalog(ctx), &arguments.Service); err != nil {
		return nil, err
	}
	accepted, err := m.api.Submit(ctx, arguments.Action, arguments.Service, arguments.KillUnowned)
	if err != nil {
		return nil, err
	}
	deadline := time.Now().Add(manageWaitTimeout)
	operation, err := m.api.Wait(ctx, accepted.ID, &deadline)
	if err != nil {
		// A lost poll doesn't lose the operation — hand its id back rather
		// than an opaque error.
		return nil, fmt.Errorf("%s (operation %s may still be running — trace it)", err.Error(), accepted.ID)
	}
	switch operation.Status {
	case state.OpStatusFailed:
		if operation.Error != nil {
			return nil, fmt.Errorf("%s", operation.Error.Message)
		}
		return nil, fmt.Errorf("operation failed")
	case state.OpStatusSucceeded:
		return m.Status(ctx, StatusArguments{Service: &arguments.Service})
	default:
		return map[string]any{
			"operation": jsonValue(operation),
			"message": fmt.Sprintf(
				"still %s after %ds — call trace with operationId %s to follow it",
				string(operation.Status), int(manageWaitTimeout.Seconds()), operation.ID,
			),
		}, nil
	}
}

// RestartDaemon is the same `hearth manager restart` the CLI runs, in-process:
// shut the daemon down leaving its services running, wait for it to exit,
// ensure a fresh one.
func (m *ManagerApiClient) RestartDaemon(ctx context.Context) (any, error) {
	result, err := cli.RestartManager(ctx, m.root, m.options.cliOptions())
	m.api.Invalidate()
	if err != nil {
		return nil, err
	}
	return result, nil
}

// StopDaemon is the same `hearth manager stop` the CLI runs, in-process:
// `stop-services` shutdown, then the wait for the daemon pid to exit —
// returning early would let the caller reconnect into a still-draining daemon
// that answers every request with `manager_closing`.
func (m *ManagerApiClient) StopDaemon(ctx context.Context) (any, error) {
	result, err := cli.StopManager(ctx, m.root, m.options.cliOptions())
	m.api.Invalidate()
	if err != nil {
		return nil, err
	}
	return result, nil
}

// ServiceIDs is the daemon's current service ids, from `/v1/catalog`.
func (m *ManagerApiClient) ServiceIDs(ctx context.Context) ([]string, bool) {
	cat, err := m.api.Catalog(ctx)
	if err != nil {
		return nil, false
	}
	ids := make([]string, 0, len(cat.Services))
	for _, service := range cat.Services {
		ids = append(ids, service.ID)
	}
	return ids, true
}

// SharedList is the remote shared-services registry, with a best-effort merge
// of what's already installed/running under smp.
func (m *ManagerApiClient) SharedList(ctx context.Context) (any, error) {
	var catalogURL *string
	if value := os.Getenv("HEARTH_SHARED_CATALOG_URL"); value != "" {
		catalogURL = &value
	}
	remote := shared.NewRemoteCatalog(shared.SharedRoot(), catalogURL)
	doc, err := remote.Document()
	if err != nil {
		return nil, err
	}
	result := map[string]any{"catalog": jsonValue(doc)}
	if discovered := client.DiscoverSMP(ctx); discovered.Kind == client.DiscoveryLive {
		if installed, err := client.SharedInstances(ctx, discovered.Client); err == nil {
			result["instances"] = installed["instances"]
		}
	}
	return result, nil
}

// SharedStatus is the instances the machine-global smp daemon knows about.
func (m *ManagerApiClient) SharedStatus(ctx context.Context) (any, error) {
	switch discovered := client.DiscoverSMP(ctx); discovered.Kind {
	case client.DiscoveryLive:
		return client.SharedInstances(ctx, discovered.Client)
	case client.DiscoveryIncompatible:
		return nil, fmt.Errorf("smp protocol is incompatible")
	default:
		return map[string]any{"running": false, "instances": []any{}}, nil
	}
}

// SharedConnection resolves the instance id, then returns this project's
// rendered connection info for it.
func (m *ManagerApiClient) SharedConnection(ctx context.Context, service string) (any, error) {
	instanceID, err := resolveSharedInstanceID(m.currentCatalog(ctx), service)
	if err != nil {
		return nil, err
	}
	discovered := client.DiscoverSMP(ctx)
	if discovered.Kind != client.DiscoveryLive {
		return nil, fmt.Errorf("smp is not running")
	}
	return client.SharedConnection(ctx, discovered.Client, instanceID, m.root)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// resolveSharedInstanceID maps `"postgres"` → `"postgres@16.4"` via this
// project catalog's generated `shared:` service (its run command is
// `hearth shared attach <name@version> [attach-args…]`); `"postgres@16.4"`
// passes through.
func resolveSharedInstanceID(cat *catalog.ServiceCatalog, service string) (string, error) {
	if strings.Contains(service, "@") {
		return service, nil
	}
	definition := findService(cat, service)
	if definition == nil {
		return "", fmt.Errorf("unknown service: %s", service)
	}
	profile := &definition.Profiles.Run
	if !profile.IsVerified() || profile.Command == nil || !profile.Command.Command.IsArgv() {
		return "", fmt.Errorf("%s is not a shared service", service)
	}
	argv := profile.Command.Command.Argv
	// The id follows the `shared attach` pair; `attachArgs` may follow the id.
	for i := 0; i+2 < len(argv); i++ {
		if argv[i] == "shared" && argv[i+1] == "attach" {
			return argv[i+2], nil
		}
	}
	return "", fmt.Errorf("%s is not a shared service", service)
}

// findService returns the definition for id, or nil.
func findService(cat *catalog.ServiceCatalog, id string) *catalog.ServiceDefinition {
	if cat == nil {
		return nil
	}
	for i := range cat.Services {
		if cat.Services[i].ID == id {
			return &cat.Services[i]
		}
	}
	return nil
}

// filterServiceURLs narrows `/v1/urls`' body to one service's entries when
// `status` was asked about one service.
func filterServiceURLs(value map[string]any, service *string) map[string]any {
	if service == nil {
		return value
	}
	for _, key := range []string{"urls", "unresolved"} {
		entries, ok := value[key].([]any)
		if !ok {
			continue
		}
		filtered := make([]any, 0, len(entries))
		for _, entry := range entries {
			object, ok := entry.(map[string]any)
			if !ok {
				continue
			}
			if id, _ := object["serviceId"].(string); id == *service {
				filtered = append(filtered, entry)
			}
		}
		value[key] = filtered
	}
	return value
}

// filterServiceStates narrows `/v1/services`' body to one service's row when
// `status` was asked about one service.
func filterServiceStates(value any, service *string) any {
	if service == nil {
		return value
	}
	switch v := value.(type) {
	case []any:
		filtered := make([]any, 0, len(v))
		for _, entry := range v {
			object, ok := entry.(map[string]any)
			if !ok {
				continue
			}
			if id, _ := object["serviceId"].(string); id == *service {
				filtered = append(filtered, entry)
			}
		}
		return filtered
	case map[string]any:
		if services, ok := v["services"]; ok {
			v["services"] = filterServiceStates(services, service)
		}
		return v
	default:
		return value
	}
}

// queryPair is one `key=value` of a query string; a nil value is omitted.
type queryPair struct {
	key   string
	value *string
}

// queryString builds `?k=v&…` from the non-nil pairs, percent-encoding both
// sides. It is empty when every pair is nil.
func queryString(pairs []queryPair) string {
	parts := make([]string, 0, len(pairs))
	for _, pair := range pairs {
		if pair.value == nil {
			continue
		}
		parts = append(parts, client.EncodePathSegment(pair.key)+"="+client.EncodePathSegment(*pair.value))
	}
	if len(parts) == 0 {
		return ""
	}
	return "?" + strings.Join(parts, "&")
}

// u64String renders an optional integer as a decimal string, or nil.
func u64String(value *uint64) *string {
	if value == nil {
		return nil
	}
	text := fmt.Sprintf("%d", *value)
	return &text
}

// jsonValue round-trips a Go value through JSON so the result is the plain
// `map[string]any`/`[]any` shape redaction recurses through — the Rust
// `serde_json::to_value`.
func jsonValue(value any) any {
	data, err := json.Marshal(value)
	if err != nil {
		return nil
	}
	var out any
	if err := json.Unmarshal(data, &out); err != nil {
		return nil
	}
	return out
}
