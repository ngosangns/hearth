// Adapter interfaces and the shared wire types for the daemon client.
//
// Ported from rust/crates/hearth-cli/src/lib.rs (the `Client`, `Discovery`,
// `LocalctlError` and `LocalctlOptions` shapes) plus the `EventReplay` /
// `WatchEvent` shapes from rust/crates/hearth-tui/src/client.rs.

package client

import (
	"net/http"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/state"
)

// Exit codes, matching the Rust CLI's `LocalctlError.exit_code`.
const (
	ExitUsage        = 2
	ExitUnavailable  = 3
	ExitProtocol     = 4
	ExitFailed       = 5
	ExitUnauthorized = 7
)

// HTTPDoer is the adapter every request goes through. net/http's *http.Client
// satisfies it; tests substitute a fake so no real socket is opened.
type HTTPDoer interface {
	Do(req *http.Request) (*http.Response, error)
}

// Client is one discovered daemon connection: the project root it belongs to,
// the runtime directory its lock lives in, the daemon's published metadata
// (port, pid, protocol version) and the bearer token every request carries.
//
// Port and PID are the metadata's, exposed as accessors so the common
// `{root, token, port}` surface is one call away.
type Client struct {
	Root             string
	RuntimeDirectory string
	Metadata         state.ManagerMetadata
	Token            string
	// Doer performs HTTP requests. Nil uses the package's shared client.
	Doer HTTPDoer
}

// NewClient builds a connection from already-discovered artifacts. Prefer
// Discover, RequireClientFor or Ensure, which validate the lock's ownership
// proof before handing back a Client.
func NewClient(root, runtimeDirectory string, metadata state.ManagerMetadata, token string) *Client {
	return &Client{
		Root:             root,
		RuntimeDirectory: runtimeDirectory,
		Metadata:         metadata,
		Token:            token,
	}
}

// Port is the daemon's loopback port.
func (c *Client) Port() uint16 { return c.Metadata.Port }

// PID is the daemon's process id.
func (c *Client) PID() int64 { return c.Metadata.Pid }

// DiscoveryKind classifies what Discover found for a root.
type DiscoveryKind int

const (
	// DiscoveryAbsent: no lock directory, no metadata, or no token.
	DiscoveryAbsent DiscoveryKind = iota
	// DiscoveryMalformed: artifacts exist but fail validation.
	DiscoveryMalformed
	// DiscoveryIncompatible: a live daemon with a different protocol version.
	DiscoveryIncompatible
	// DiscoveryStale: artifacts are valid but /v1/manager did not answer.
	DiscoveryStale
	// DiscoveryLive: the daemon answered /v1/manager.
	DiscoveryLive
)

// Discovery is the result of probing a root for a daemon. Client is nil for
// Absent and Malformed.
type Discovery struct {
	Kind   DiscoveryKind
	Client *Client
}

// Options configures Ensure. Catalog only locates the daemon (its runtime
// directory); SpawnDaemon starts one when none is found. Doer overrides the
// HTTP adapter for tests.
type Options struct {
	Catalog     *catalog.ServiceCatalog
	SpawnDaemon func(root string)
	Doer        HTTPDoer
}

// ErrorKind classifies a client failure so callers can match on it instead of
// parsing the message.
type ErrorKind int

const (
	// KindUnavailable: the connection itself is unusable (refused, reset).
	KindUnavailable ErrorKind = iota
	// KindTimeout: the request exceeded its transport timeout.
	KindTimeout
	// KindHTTP: an API-level rejection; Code carries the daemon's error code.
	KindHTTP
	// KindProtocol: the daemon's protocol version is incompatible.
	KindProtocol
	// KindUnauthorized: the daemon rejected the bearer token.
	KindUnauthorized
	// KindMalformed: a response body did not parse.
	KindMalformed
	// KindUsage: a caller-supplied value was invalid.
	KindUsage
)

// Error is the typed client error, mirroring the Rust `LocalctlError`: an exit
// code plus a message. For an HTTP-level rejection Code carries the daemon's
// error code and Error() renders "<code>:<message>", the exact string the Rust
// client produced.
type Error struct {
	Kind     ErrorKind
	Code     string
	Message  string
	ExitCode int
}

func (e *Error) Error() string {
	if e.Code != "" {
		return e.Code + ":" + e.Message
	}
	return e.Message
}

// Is lets errors.Is match on Kind, so callers write
// errors.Is(err, client.ErrUnavailable).
func (e *Error) Is(target error) bool {
	t, ok := target.(*Error)
	return ok && e.Kind == t.Kind
}

// Sentinel errors for the typed failure classes callers match on.
var (
	ErrUnavailable  = &Error{Kind: KindUnavailable, Message: "manager unavailable", ExitCode: ExitUnavailable}
	ErrTimeout      = &Error{Kind: KindTimeout, Message: "manager request timed out", ExitCode: ExitUnavailable}
	ErrProtocol     = &Error{Kind: KindProtocol, Message: "hearth manager protocol is incompatible", ExitCode: ExitProtocol}
	ErrUnauthorized = &Error{Kind: KindUnauthorized, Message: "hearth manager authentication failed", ExitCode: ExitUnauthorized}
)

// URLs is the `{ urls, unresolved }` body of GET /v1/urls.
type URLs struct {
	URLs       []catalog.ResolvedServiceURL   `json:"urls"`
	Unresolved []catalog.UnresolvedServiceURL `json:"unresolved"`
}

// EventKind discriminates the frames Events and Watch emit.
type EventKind int

const (
	// EventKindBeginConnection: a new connection attempt is starting (Watch only).
	EventKindBeginConnection EventKind = iota
	// EventKindSnapshot: a fresh /v1/services snapshot (Watch only).
	EventKindSnapshot
	// EventKindReplay: the SSE replay frame. Reset means the client's cursor is
	// unusable and it must resync from a fresh /v1/services snapshot.
	EventKindReplay
	// EventKindManagerEvent: one live manager event.
	EventKindManagerEvent
	// EventKindUnavailable: the stream failed; Message carries the reason.
	EventKindUnavailable
)

// Event is one frame from the daemon's event stream, mapping the old TUI's
// `WatchEvent` union. Exactly the field matching Kind is set.
type Event struct {
	Kind     EventKind
	Snapshot []state.ServiceLifecycleState
	Replay   *EventReplay
	Manager  *state.ManagerEvent
	Message  string
}

// EventReplay is the `replay` SSE frame: the epoch the stream is on, whether
// the client's cursor was unusable (Reset), and the latest sequence.
type EventReplay struct {
	Epoch          string `json:"epoch"`
	Reset          bool   `json:"reset"`
	LatestSequence uint64 `json:"latestSequence"`
}
