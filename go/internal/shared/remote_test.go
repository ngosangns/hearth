// Ported from rust/crates/hearth-core/src/shared/remote.rs's `mod tests`: the
// cache/embedded fallback chain, file:// fetch, and the shipped-catalog
// template-var contract.
package shared

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"unicode"

	"github.com/ngosangns/hearth/go/internal/catalog"
)

func sampleDoc() string {
	return `{
  "version": 1,
  "services": {
    "postgres": { "versions": { "16.4": {
      "artifacts": { "darwin-arm64": { "url": "http://example.invalid/p.tgz", "sha256": "abc" } },
      "run": { "argv": ["postgres"] },
      "readiness": { "kind": "tcp" }
    } } }
  }
}`
}

func TestFallsBackToCacheWhenRemoteIsUnreachable(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, cacheFileName), []byte(sampleDoc()), 0o644); err != nil {
		t.Fatal(err)
	}
	url := "http://127.0.0.1:1/unreachable"
	remote := NewRemoteCatalog(dir, &url)
	doc, err := remote.Document()
	if err != nil {
		t.Fatal(err)
	}
	recipe := doc.Recipe("postgres", "16.4")
	if recipe == nil || recipe.Artifacts["darwin-arm64"].Sha256 != "abc" {
		t.Fatal("a cached recipe wins over nothing")
	}
	// A stale cache doesn't hide recipes this build ships.
	if _, ok := doc.Services["redis"]; !ok {
		t.Fatal("embedded catalog did not top up the stale cache")
	}
}

func TestFallsBackToEmbeddedCatalogWithoutRemoteOrCache(t *testing.T) {
	// No cached document: a private repo makes the pinned raw URL answer 404
	// forever, so the binary's own copy of catalog.json is what answers.
	dir := t.TempDir()
	url := "http://127.0.0.1:1/unreachable"
	remote := NewRemoteCatalog(dir, &url)
	doc, err := remote.Document()
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := doc.Services["redis"]; !ok {
		t.Fatal("embedded catalog missing redis")
	}
}

func TestFileURLFetchesAndWritesThroughCache(t *testing.T) {
	dir := t.TempDir()
	source := filepath.Join(dir, "source-catalog.json")
	if err := os.WriteFile(source, []byte(sampleDoc()), 0o644); err != nil {
		t.Fatal(err)
	}
	url := "file://" + source
	remote := NewRemoteCatalog(dir, &url)
	doc, err := remote.Document()
	if err != nil {
		t.Fatal(err)
	}
	if doc.Recipe("postgres", "16.4") == nil {
		t.Fatal("file:// document not served")
	}
	cached, err := os.ReadFile(filepath.Join(dir, cacheFileName))
	if err != nil {
		t.Fatalf("fetch did not write the cache through: %v", err)
	}
	if string(cached) != sampleDoc() {
		t.Fatal("cache holds different bytes than the fetched document")
	}
}

func walkStrings(value any, path string, out *[]struct{ Path, Text string }) {
	switch v := value.(type) {
	case string:
		*out = append(*out, struct{ Path, Text string }{path, v})
	case []any:
		for i, item := range v {
			walkStrings(item, path+"["+string(rune('0'+i))+"]", out)
		}
	case map[string]any:
		for key, item := range v {
			walkStrings(item, path+"."+key, out)
		}
	}
}

func isExtraPort(name string) bool {
	if !strings.HasPrefix(name, "port") || len(name) <= 4 {
		return false
	}
	for _, c := range name[4:] {
		if !unicode.IsDigit(c) {
			return false
		}
	}
	return true
}

// The shipped catalog must only reference template vars the daemon renders,
// and the embedded copy must not drift from the repo-root document.
func TestShippedCatalogTemplatesOnlyUseKnownVars(t *testing.T) {
	path := filepath.Join("..", "..", "..", "catalog.json")
	text, err := os.ReadFile(path)
	if err != nil {
		t.Skipf("repo-root catalog.json not readable: %v", err)
	}
	if string(text) != embeddedCatalog {
		t.Fatal("go/internal/shared/catalog.json drifted from repo-root catalog.json — run `task catalog:write`")
	}
	doc, err := parseDocument(embeddedCatalog)
	if err != nil {
		t.Fatalf("catalog.json parses: %v", err)
	}
	for _, name := range []string{"redis", "mongodb", "minio", "nginx", "kafka"} {
		if _, ok := doc.Services[name]; !ok {
			t.Fatalf("%s missing", name)
		}
	}
	var value map[string]any
	if err := json.Unmarshal([]byte(embeddedCatalog), &value); err != nil {
		t.Fatal(err)
	}
	var stringsList []struct{ Path, Text string }
	walkStrings(value["services"], "services", &stringsList)
	instance := map[string]bool{
		"installDir": true, "dataDir": true, "port": true,
		"name": true, "version": true, "instanceId": true,
	}
	project := map[string]bool{
		"projectId": true, "projectDb": true, "projectUser": true,
		"projectBucket": true, "projectRoot": true,
	}
	for _, s := range stringsList {
		inAttachment := strings.Contains(s.Path, ".provision") ||
			strings.Contains(s.Path, ".deprovision") ||
			strings.Contains(s.Path, ".connection")
		for _, name := range catalog.TemplateVars(s.Text) {
			known := instance[name] ||
				isExtraPort(name) ||
				(inAttachment && project[name]) ||
				(strings.Contains(s.Path, ".connection") && name == "url")
			if !known {
				t.Fatalf("%s uses unknown template {%s}", s.Path, name)
			}
		}
	}
	for service, family := range doc.Services {
		for version, recipe := range family.Versions {
			blob, _ := json.Marshal(recipe)
			usesPort2 := false
			for _, name := range catalog.TemplateVars(string(blob)) {
				if name == "port2" {
					usesPort2 = true
				}
			}
			if usesPort2 && recipe.ExtraPortCount() < 1 {
				t.Fatalf("%s@%s uses {port2} without a second port (ports/additionalPorts)", service, version)
			}
			if recipe.AdditionalPorts > 0 && len(recipe.Ports) > 0 {
				t.Fatalf("%s@%s declares both ports and additionalPorts", service, version)
			}
			sha := recipe.Artifacts["darwin-arm64"].Sha256
			if len(sha) != 64 {
				t.Fatalf("%s@%s sha256", service, version)
			}
			for _, c := range sha {
				if !unicode.Is(unicode.Hex_Digit, c) || unicode.IsUpper(c) {
					t.Fatalf("%s@%s sha256", service, version)
				}
			}
		}
	}
	for _, recipe := range doc.Services["minio"].Versions {
		if recipe.AdditionalPorts != 1 {
			t.Fatal("minio recipe must have additionalPorts == 1")
		}
	}
	for _, recipe := range doc.Services["kafka"].Versions {
		if recipe.AdditionalPorts != 1 {
			t.Fatal("kafka recipe must have additionalPorts == 1")
		}
	}
}
