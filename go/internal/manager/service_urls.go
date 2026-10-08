// Values for the placeholders a catalog's service URLs may contain
// (SERVICE_URL_PLACEHOLDERS).
package manager

import (
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"strings"
	"sync"
	"time"
)

// How long a looked-up tailnet host is reused — /v1/urls is fetched on every
// client connect and catalog reload; spawning tailscale each time is
// needless, but the host can change (re-login, rename), so it isn't cached
// forever either.
const tailnetHostTTL = 60 * time.Second
const tailnetQueryTimeout = 3 * time.Second

var tailnetHostCache struct {
	mu    sync.Mutex
	at    time.Time
	valid bool
	value *string
}

// LookupPlaceholder answers one placeholder name for ResolveServiceURLs.
func LookupPlaceholder(name string) *string {
	switch name {
	case "tailnetHost":
		return TailnetHost()
	default:
		return nil
	}
}

// TailnetHost — this machine's Tailscale DNS name, from $HEARTH_TAILNET_HOST
// if set, otherwise `tailscale status --json`'s Self.DNSName. nil when
// Tailscale is not installed, not running, or not logged in.
func TailnetHost() *string {
	if v, ok := os.LookupEnv("HEARTH_TAILNET_HOST"); ok {
		if trimmed := strings.TrimRight(strings.TrimSpace(v), "."); trimmed != "" {
			return &trimmed
		}
	}
	// The cache lock is never held across the tailscale spawn — a slow query
	// would otherwise stall every other caller behind it. Two concurrent
	// misses both query, which is harmless.
	tailnetHostCache.mu.Lock()
	if tailnetHostCache.valid && time.Since(tailnetHostCache.at) < tailnetHostTTL {
		v := tailnetHostCache.value
		tailnetHostCache.mu.Unlock()
		return v
	}
	tailnetHostCache.mu.Unlock()
	value := queryTailnetHost()
	tailnetHostCache.mu.Lock()
	tailnetHostCache.at = time.Now()
	tailnetHostCache.valid = true
	tailnetHostCache.value = value
	tailnetHostCache.mu.Unlock()
	return value
}

func queryTailnetHost() *string {
	ctx, cancel := context.WithTimeout(context.Background(), tailnetQueryTimeout)
	defer cancel()
	out, err := exec.CommandContext(ctx, "tailscale", "status", "--json").Output()
	if err != nil {
		return nil
	}
	return ParseTailnetHost(string(out))
}

// ParseTailnetHost — Self.DNSName without its trailing root dot.
func ParseTailnetHost(statusJSON string) *string {
	var value struct {
		Self struct {
			DNSName string `json:"DNSName"`
		} `json:"Self"`
	}
	if err := json.Unmarshal([]byte(statusJSON), &value); err != nil {
		return nil
	}
	name := strings.TrimRight(value.Self.DNSName, ".")
	if name == "" {
		return nil
	}
	return &name
}
