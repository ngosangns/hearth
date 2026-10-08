// The caching daemon connection.
//
// Ported from rust/crates/hearth-cli/src/client.rs's `ManagerClient`: a
// project's daemon, discovered lazily and cached between calls. It never
// spawns a daemon — callers that may need one Ensure first. A failure that
// says the connection itself is gone (dead port, stale token after an outside
// `manager restart`) invalidates the cache and retries ONCE on a freshly
// discovered connection, so a long-lived MCP session does not hand its agent a
// spurious error for every first call after a restart.
package client

import (
	"context"
	"net/http"
	"sync"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

// ManagerClient is a project's daemon, discovered lazily and cached between
// calls.
type ManagerClient struct {
	root    string
	catalog *catalog.ServiceCatalog
	// Doer overrides the HTTP adapter for tests.
	Doer HTTPDoer

	mu     sync.Mutex
	cached *Client
}

// NewManagerClient builds a client for root. catalog only locates the daemon
// (its runtime directory); it is never sent anywhere.
func NewManagerClient(root string, cat *catalog.ServiceCatalog) *ManagerClient {
	return &ManagerClient{root: root, catalog: cat}
}

// Root is the project root this client talks to.
func (m *ManagerClient) Root() string { return m.root }

// Connection is the cached connection, or a freshly discovered (and cached)
// one.
func (m *ManagerClient) Connection(ctx context.Context) (*Client, error) {
	m.mu.Lock()
	cached := m.cached
	m.mu.Unlock()
	if cached != nil {
		return cached, nil
	}
	client, err := requireClientForWith(ctx, newFileIO(m.catalog), m.Doer, m.root, m.catalog)
	if err != nil {
		return nil, err
	}
	m.mu.Lock()
	m.cached = client
	m.mu.Unlock()
	return client, nil
}

// Invalidate drops the cached connection so the next call re-discovers the
// daemon.
func (m *ManagerClient) Invalidate() {
	m.mu.Lock()
	m.cached = nil
	m.mu.Unlock()
}

// withClient runs call against the cached connection, invalidating and
// retrying once on a connection failure.
func withClient[T any](m *ManagerClient, ctx context.Context, call func(*Client) (T, error)) (T, error) {
	var zero T
	client, err := m.Connection(ctx)
	if err != nil {
		return zero, err
	}
	result, err := call(client)
	if err != nil && IsConnectionFailure(err) {
		m.Invalidate()
		client, err = m.Connection(ctx)
		if err != nil {
			return zero, err
		}
		return call(client)
	}
	return result, err
}

// ManagerInfo is GET /v1/manager.
func (m *ManagerClient) ManagerInfo(ctx context.Context) (*state.ManagerInfo, error) {
	return withClient(m, ctx, func(c *Client) (*state.ManagerInfo, error) {
		return c.ManagerInfo(ctx)
	})
}

// Services is GET /v1/services.
func (m *ManagerClient) Services(ctx context.Context) ([]state.ServiceLifecycleState, error) {
	return withClient(m, ctx, func(c *Client) ([]state.ServiceLifecycleState, error) {
		return c.Services(ctx)
	})
}

// URLs is GET /v1/urls.
func (m *ManagerClient) URLs(ctx context.Context) (*URLs, error) {
	return withClient(m, ctx, func(c *Client) (*URLs, error) {
		return c.URLs(ctx)
	})
}

// Catalog is GET /v1/catalog.
func (m *ManagerClient) Catalog(ctx context.Context) (*catalog.ServiceCatalog, error) {
	return withClient(m, ctx, func(c *Client) (*catalog.ServiceCatalog, error) {
		return c.Catalog(ctx)
	})
}

// Log is GET /v1/logs/:id.
func (m *ManagerClient) Log(ctx context.Context, serviceID string, cursor, generation, limit *uint64) (*state.LogSlice, error) {
	return withClient(m, ctx, func(c *Client) (*state.LogSlice, error) {
		return c.Log(ctx, serviceID, cursor, generation, limit)
	})
}

// DaemonLog is GET /v1/daemon/log.
func (m *ManagerClient) DaemonLog(ctx context.Context, bytes *uint64) (*state.LogSlice, error) {
	return withClient(m, ctx, func(c *Client) (*state.LogSlice, error) {
		return c.DaemonLog(ctx, bytes)
	})
}

// Operation is GET /v1/operations/:id.
func (m *ManagerClient) Operation(ctx context.Context, id string) (*state.Operation, error) {
	return withClient(m, ctx, func(c *Client) (*state.Operation, error) {
		return c.Operation(ctx, id)
	})
}

// Submit is POST /v1/operations. The request id is minted once so the retry
// after a timeout reuses it — the daemon dedupes on requestId, and a second id
// would start the service twice when the first request actually landed.
func (m *ManagerClient) Submit(ctx context.Context, action state.ServiceOperationKind, serviceID string, killUnowned bool) (*state.Operation, error) {
	requestID := NewRequestID()
	return withClient(m, ctx, func(c *Client) (*state.Operation, error) {
		return c.Submit(ctx, action, serviceID, killUnowned, requestID)
	})
}

// BulkStart is POST /v1/operations/bulk-start.
func (m *ManagerClient) BulkStart(ctx context.Context, targets []string, killUnowned bool) (*state.Operation, error) {
	requestID := NewRequestID()
	return withClient(m, ctx, func(c *Client) (*state.Operation, error) {
		return c.BulkStart(ctx, targets, killUnowned, requestID)
	})
}

// Wait polls an operation until it is terminal or deadline passes.
func (m *ManagerClient) Wait(ctx context.Context, id string, deadline *time.Time) (*state.Operation, error) {
	return withClient(m, ctx, func(c *Client) (*state.Operation, error) {
		return c.Wait(ctx, id, deadline)
	})
}

// EventStream opens GET /v1/events/stream on the cached connection.
func (m *ManagerClient) EventStream(ctx context.Context, after *uint64, epoch *string) (*http.Response, error) {
	return withClient(m, ctx, func(c *Client) (*http.Response, error) {
		return c.EventStream(ctx, after, epoch)
	})
}

// Request is a raw request against the cached connection, for endpoints
// without a typed method.
func (m *ManagerClient) Request(ctx context.Context, path, method string, body any) (map[string]any, error) {
	return withClient(m, ctx, func(c *Client) (map[string]any, error) {
		return c.Request(ctx, path, method, body)
	})
}

// DaemonPID is the pid from the daemon lock. It fails when no daemon is up;
// callers leave the header blank.
func (m *ManagerClient) DaemonPID(ctx context.Context) (int64, error) {
	connection, err := m.Connection(ctx)
	if err != nil {
		return 0, err
	}
	return connection.Metadata.Pid, nil
}

// Events opens the SSE stream on the cached connection and returns a channel
// of decoded frames (see Client.Events).
func (m *ManagerClient) Events(ctx context.Context, after *uint64, epoch *string) <-chan Event {
	out := make(chan Event, eventChannelBuffer)
	go func() {
		defer close(out)
		client, err := m.Connection(ctx)
		if err != nil {
			_ = sendEvent(ctx, out, Event{Kind: EventKindUnavailable, Message: err.Error()})
			return
		}
		for ev := range client.Events(ctx, after, epoch) {
			if sendEvent(ctx, out, ev) != nil {
				return
			}
		}
	}()
	return out
}

// Watch runs the reconnect loop on the cached connection, re-discovering after
// a connection failure (see Client.Watch).
func (m *ManagerClient) Watch(ctx context.Context, after *uint64, epoch *string) <-chan Event {
	out := make(chan Event, eventChannelBuffer)
	go func() {
		defer close(out)
		var cursor *uint64
		if after != nil {
			v := *after
			cursor = &v
		}
		var epochVal *string
		if epoch != nil {
			v := *epoch
			epochVal = &v
		}
		for {
			if ctx.Err() != nil {
				return
			}
			if sendEvent(ctx, out, Event{Kind: EventKindBeginConnection}) != nil {
				return
			}
			client, err := m.Connection(ctx)
			if err != nil {
				if sendEvent(ctx, out, Event{Kind: EventKindUnavailable, Message: err.Error()}) != nil {
					return
				}
			} else if err := client.watchOnce(ctx, &cursor, &epochVal, out); err != nil {
				if ctx.Err() != nil {
					return
				}
				if IsConnectionFailure(err) {
					m.Invalidate()
				}
				if sendEvent(ctx, out, Event{Kind: EventKindUnavailable, Message: err.Error()}) != nil {
					return
				}
			}
			if ctx.Err() != nil {
				return
			}
			if err := sleepCtx(ctx, reconnectDelay); err != nil {
				return
			}
		}
	}()
	return out
}
