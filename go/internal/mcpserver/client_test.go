// Ported from rust/crates/hearth-mcp/src/client.rs's `mod tests`, plus focused
// coverage for the argument-validation and redaction helpers.
package mcpserver

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/shared"
)

func catalogWith(definition catalog.ServiceDefinition) *catalog.ServiceCatalog {
	return &catalog.ServiceCatalog{
		Services:           []catalog.ServiceDefinition{definition},
		Groups:             map[string][]string{},
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
	}
}

// viclass/infra's shared nginx passes `attachArgs`, which land after the
// instance id.
func TestResolvesTheInstanceIDEvenWhenAttachArgsFollowIt(t *testing.T) {
	exe := "hearth"
	withArgs := shared.ProjectServiceEntry("nginx", "nginx@1.27", exe, nil, []string{"--conf", "/tmp/conf"}, nil)
	if got, err := resolveSharedInstanceID(catalogWith(withArgs), "nginx"); err != nil || got != "nginx@1.27" {
		t.Errorf("nginx = %q, %v", got, err)
	}
	bare := shared.ProjectServiceEntry("postgres", "postgres@16.4", exe, nil, nil, nil)
	if got, err := resolveSharedInstanceID(catalogWith(bare), "postgres"); err != nil || got != "postgres@16.4" {
		t.Errorf("postgres = %q, %v", got, err)
	}
	third := shared.ProjectServiceEntry("x", "x@1", exe, nil, nil, nil)
	if got, err := resolveSharedInstanceID(catalogWith(third), "x@2"); err != nil || got != "x@2" {
		t.Errorf("x@2 = %q, %v", got, err)
	}
}

func TestResolveSharedInstanceIDRejectsNonSharedServices(t *testing.T) {
	kind := catalog.KindApplication
	plain := catalog.ServiceDefinition{
		ID:   "api",
		Kind: &kind,
		Profiles: catalog.ServiceProfiles{
			Run: catalog.ServiceRunProfile{
				CommandStatus: "verified",
				Command: &catalog.ServiceCommand{
					Command: catalog.CommandSpec{Shell: "exec nc -lk 1"},
					Cwd:     ".",
				},
			},
		},
	}
	if _, err := resolveSharedInstanceID(catalogWith(plain), "api"); err == nil {
		t.Error("a shell-run service is not a shared service")
	}
	if _, err := resolveSharedInstanceID(catalogWith(plain), "missing"); err == nil {
		t.Error("an unknown service must be rejected")
	}
}

func TestQueryStringOmitsNilPairs(t *testing.T) {
	after := "0"
	if got := queryString([]queryPair{{"after", &after}, {"epoch", nil}}); got != "?after=0" {
		t.Errorf("got %q, want ?after=0", got)
	}
	if got := queryString([]queryPair{{"epoch", nil}}); got != "" {
		t.Errorf("got %q, want empty", got)
	}
}

func TestFilterServiceStatesNarrowsToOneService(t *testing.T) {
	value := map[string]any{"services": []any{
		map[string]any{"serviceId": "a"},
		map[string]any{"serviceId": "b"},
	}}
	service := "b"
	filtered, ok := filterServiceStates(value, &service).(map[string]any)
	if !ok {
		t.Fatalf("filtered = %T", filterServiceStates(value, &service))
	}
	rows, _ := filtered["services"].([]any)
	if len(rows) != 1 {
		t.Fatalf("rows = %v, want one", rows)
	}
	if row, _ := rows[0].(map[string]any); row["serviceId"] != "b" {
		t.Errorf("row = %v, want serviceId b", rows[0])
	}
}

func TestFilterServiceURLsNarrowsToOneService(t *testing.T) {
	value := map[string]any{
		"urls":       []any{map[string]any{"serviceId": "a"}, map[string]any{"serviceId": "b"}},
		"unresolved": []any{map[string]any{"serviceId": "a"}},
	}
	service := "a"
	filtered := filterServiceURLs(value, &service)
	if rows, _ := filtered["urls"].([]any); len(rows) != 1 {
		t.Errorf("urls = %v, want one", filtered["urls"])
	}
	if rows, _ := filtered["unresolved"].([]any); len(rows) != 1 {
		t.Errorf("unresolved = %v, want one", filtered["unresolved"])
	}
}

// JavaScript's `Number.isSafeInteger` — an integral float is accepted, and
// anything beyond 2^53 is rejected as unrepresentable.
func TestOptionalIntegerAcceptsIntegralFloatsAndRejectsUnsafeIntegers(t *testing.T) {
	if value, err := optionalInteger(float64(1000), true, "limit", 1, nil); err != nil || value == nil || *value != 1000 {
		t.Errorf("1000.0: value=%v err=%v", value, err)
	}
	if _, err := optionalInteger(1.5, true, "limit", 1, nil); err == nil {
		t.Error("1.5 must be rejected")
	}
	if _, err := optionalInteger(float64(9007199254740992), true, "limit", 1, nil); err == nil {
		t.Error("2^53 must be rejected")
	}
	if value, err := optionalInteger(nil, false, "limit", 1, nil); err != nil || value != nil {
		t.Errorf("absent: value=%v err=%v", value, err)
	}
	if _, err := optionalInteger(float64(0), true, "limit", 1, nil); err == nil {
		t.Error("0 with minimum 1 must be rejected")
	}
	maximum := int64(10)
	if _, err := optionalInteger(float64(11), true, "limit", 1, &maximum); err == nil {
		t.Error("11 with maximum 10 must be rejected")
	}
}

func TestRequireOnlyKeysListsUnexpectedKeys(t *testing.T) {
	value := arguments{
		keys:   []string{"service", "profile", "extra"},
		values: map[string]any{"service": "x", "profile": "dev", "extra": 1},
	}
	err := requireOnlyKeys(value, "service")
	if err == nil {
		t.Fatal("expected an error")
	}
	// The keys keep the client's order (serde_json's preserve_order), not a
	// sorted one.
	if err.Error() != "unexpected arguments: profile, extra" {
		t.Errorf("message = %q", err.Error())
	}
	if err := requireOnlyKeys(arguments{keys: []string{"service"}, values: map[string]any{"service": "x"}}, "service"); err != nil {
		t.Errorf("unexpected error: %v", err)
	}
}

func TestRedactDropsSecretKeys(t *testing.T) {
	value := map[string]any{
		"token":  "secret",
		"nested": map[string]any{"apiKey": "x", "keep": "y"},
		"list":   []any{map[string]any{"password": "p", "ok": 1}},
	}
	redacted, ok := redact(value).(map[string]any)
	if !ok {
		t.Fatalf("redacted = %T", redact(value))
	}
	if _, present := redacted["token"]; present {
		t.Error("token must be dropped")
	}
	nested, _ := redacted["nested"].(map[string]any)
	if _, present := nested["apiKey"]; present {
		t.Error("apiKey must be dropped")
	}
	if nested["keep"] != "y" {
		t.Error("keep must survive")
	}
	list, _ := redacted["list"].([]any)
	entry, _ := list[0].(map[string]any)
	if _, present := entry["password"]; present {
		t.Error("password must be dropped")
	}
}

func TestRedactCapsDepth(t *testing.T) {
	var value any = "leaf"
	for range 20 {
		value = map[string]any{"level": value}
	}
	data, err := json.Marshal(redact(value))
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if !strings.Contains(string(data), "[truncated]") {
		t.Errorf("deep value not truncated: %s", data)
	}
}

func TestSafeErrorMessageRedactsBearerTokens(t *testing.T) {
	got := safeErrorMessage("unauthorized: Bearer abc123 is invalid")
	if strings.Contains(got, "abc123") {
		t.Errorf("token leaked: %s", got)
	}
	if !strings.Contains(got, "Bearer [redacted]") {
		t.Errorf("got %q", got)
	}
}
