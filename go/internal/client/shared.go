// The shared-services (smp) half of the daemon client.
//
// Ported from rust/crates/hearth-cli/src/shared.rs (the smp discovery/ensure
// and the attach/detach/probe/remove/install request shapes) and the
// shared-services methods of rust/crates/hearth-mcp/src/client.rs
// (shared_list/shared_status/shared_connection). These commands deliberately
// do not require a project hearth.yaml: the smp daemon's catalog is
// synthesized from its registry, so only its runtime directory matters.
package client

import (
	"context"
	"net/http"
	"strings"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/shared"
)

// SMPCatalog is the catalog Discover/Ensure run against for smp — no services
// (they are synthesized daemon-side); only runtime_directory matters, pointing
// the lock/token search at ~/.hearth/shared/runtime-v1.
func SMPCatalog() *catalog.ServiceCatalog {
	runtimeDirectory := shared.SharedRuntimeDirectoryName
	return &catalog.ServiceCatalog{
		RuntimeDirectory:   &runtimeDirectory,
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
	}
}

// DiscoverSMP probes for the machine-global smp daemon. It never spawns one.
func DiscoverSMP(ctx context.Context) Discovery {
	return Discover(ctx, shared.SharedRoot(), SMPCatalog())
}

// EnsureSMP returns a live smp client, spawning `hearth smp` (via spawnSMP)
// when none is running.
func EnsureSMP(ctx context.Context, spawnSMP func(root string)) (*Client, error) {
	return Ensure(ctx, shared.SharedRoot(), &Options{Catalog: SMPCatalog(), SpawnDaemon: spawnSMP})
}

// SMPRequest is a normal-timeout request against an smp client.
func SMPRequest(ctx context.Context, client *Client, path, method string, body any) (map[string]any, error) {
	return client.Request(ctx, path, method, body)
}

// SMPRequestSlow is an unbounded POST — attach/install may download and extract
// a tarball on first use.
func SMPRequestSlow(ctx context.Context, client *Client, path string, body any) (map[string]any, error) {
	return client.RequestWithTimeout(ctx, path, http.MethodPost, body, nil, nil)
}

// SharedInstance is the smp `/v1/shared` instance row for id, or nil when the
// id is unknown or smp is down.
func SharedInstance(ctx context.Context, client *Client, id string) (map[string]any, error) {
	body, err := SMPRequest(ctx, client, "/v1/shared", http.MethodGet, nil)
	if err != nil {
		return nil, err
	}
	for _, entry := range asArray(body["instances"]) {
		instance, ok := entry.(map[string]any)
		if !ok {
			continue
		}
		if value, _ := instance["id"].(string); value == id {
			return instance, nil
		}
	}
	return nil, nil
}

// SharedInstances is GET /v1/shared — every instance smp knows about.
func SharedInstances(ctx context.Context, client *Client) (map[string]any, error) {
	return SMPRequest(ctx, client, "/v1/shared", http.MethodGet, nil)
}

// SharedAttach is POST /v1/shared/attach. args are forwarded verbatim to the
// recipe's provision argv.
func SharedAttach(ctx context.Context, client *Client, id, projectRoot string, args []string) (map[string]any, error) {
	if args == nil {
		args = []string{}
	}
	return SMPRequestSlow(ctx, client, "/v1/shared/attach", map[string]any{
		"service":     id,
		"projectRoot": projectRoot,
		"args":        args,
	})
}

// SharedDetach is POST /v1/shared/detach. An smp from before stopIfUnused
// rejects the unknown field; detaching still matters more than the stop, so it
// falls back to a plain detach.
func SharedDetach(ctx context.Context, client *Client, id, projectRoot string, stopIfUnused bool) (map[string]any, error) {
	body := map[string]any{"service": id, "projectRoot": projectRoot}
	if stopIfUnused {
		body["stopIfUnused"] = true
	}
	result, err := SMPRequest(ctx, client, "/v1/shared/detach", http.MethodPost, body)
	if stopIfUnused && err != nil && strings.HasPrefix(err.Error(), "invalid_request") {
		plain := map[string]any{"service": id, "projectRoot": projectRoot}
		return SMPRequest(ctx, client, "/v1/shared/detach", http.MethodPost, plain)
	}
	return result, err
}

// SharedProbe is the readiness probe the generated `shared:` service polls:
// true iff the instance is ready AND this project is attached and provisioned.
func SharedProbe(ctx context.Context, client *Client, id, projectRoot string) bool {
	instance, err := SharedInstance(ctx, client, id)
	if err != nil || instance == nil {
		return false
	}
	stateObj, _ := instance["state"].(map[string]any)
	if stateObj == nil {
		return false
	}
	if actual, _ := stateObj["actualState"].(string); actual != "ready" {
		return false
	}
	projectID := shared.ProjectID(projectRoot)
	for _, entry := range asArray(instance["attachments"]) {
		attachment, ok := entry.(map[string]any)
		if !ok {
			continue
		}
		if value, _ := attachment["projectId"].(string); value != projectID {
			continue
		}
		if provisioned, _ := attachment["provisioned"].(bool); provisioned {
			return true
		}
	}
	return false
}

// SharedRemove is POST /v1/shared/remove. The server refuses (409
// shared_service_attached) an instance that still has project attachments
// unless force confirms the data wipe.
func SharedRemove(ctx context.Context, client *Client, id string, force bool) (map[string]any, error) {
	return SMPRequest(ctx, client, "/v1/shared/remove", http.MethodPost, map[string]any{
		"service": id,
		"force":   force,
	})
}

// SharedInstall is POST /v1/shared/install — register and install a recipe.
func SharedInstall(ctx context.Context, client *Client, id string) (map[string]any, error) {
	return SMPRequestSlow(ctx, client, "/v1/shared/install", map[string]any{"service": id})
}

// SharedConnection is this project's rendered connection info for a shared
// service it has attached — the way an agent learns the DATABASE_URL-style
// values without env injection.
func SharedConnection(ctx context.Context, client *Client, id, projectRoot string) (map[string]any, error) {
	instance, err := SharedInstance(ctx, client, id)
	if err != nil {
		return nil, err
	}
	if instance == nil {
		return nil, sharedError(id + " is not a registered shared instance")
	}
	projectID := shared.ProjectID(projectRoot)
	for _, entry := range asArray(instance["attachments"]) {
		attachment, ok := entry.(map[string]any)
		if !ok {
			continue
		}
		if value, _ := attachment["projectId"].(string); value != projectID {
			continue
		}
		if provisioned, _ := attachment["provisioned"].(bool); !provisioned {
			return nil, sharedError(id + " is attached but not yet provisioned")
		}
		return map[string]any{
			"service":    id,
			"projectId":  projectID,
			"connection": attachment["connection"],
		}, nil
	}
	return nil, sharedError("this project has not attached " + id + " — start the service first")
}

func sharedError(message string) *Error {
	return &Error{Kind: KindHTTP, Message: message, ExitCode: ExitFailed}
}

func asArray(value any) []any {
	array, _ := value.([]any)
	return array
}
