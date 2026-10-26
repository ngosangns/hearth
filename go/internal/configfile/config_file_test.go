package configfile

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
)

func loadYAML(t *testing.T, body string) (*catalog.ServiceCatalog, error) {
	t.Helper()
	dir := t.TempDir()
	path := filepath.Join(dir, "hearth.yaml")
	if err := os.WriteFile(path, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
	loaded, err := LoadCatalogFromFile(path, dir)
	if err != nil {
		return nil, err
	}
	return loaded.Catalog, nil
}

const sleepService = `    run: { argv: [sleep, "60"] }
    readiness: { kind: process }
`

func TestGroupsComeFromEachServicesGroupFieldInDocumentOrder(t *testing.T) {
	body := `version: 1
services:
  api:
    group: backend
` + sleepService + `  web:
` + sleepService + `  db:
    group: backend
` + sleepService + `  cache:
    group: data
` + sleepService
	cat, err := loadYAML(t, body)
	if err != nil {
		t.Fatal(err)
	}
	wantGroups := map[string][]string{
		"backend": {"api", "db"},
		"data":    {"cache"},
		"all":     {"api", "web", "db", "cache"},
	}
	if !reflect.DeepEqual(cat.Groups, wantGroups) {
		t.Fatalf("groups: got %v, want %v", cat.Groups, wantGroups)
	}
	wantTree := []catalog.CatalogGroup{
		{Name: "backend", Members: []string{"api", "db"}},
		{Name: "data", Members: []string{"cache"}},
	}
	if !reflect.DeepEqual(cat.GroupTree, wantTree) {
		t.Fatalf("groupTree: got %v, want %v", cat.GroupTree, wantTree)
	}
}

func TestDisabledServicesKeepTheirGroupInTheTreeButLeaveTargets(t *testing.T) {
	body := `version: 1
services:
  api:
    group: backend
` + sleepService + `  db:
    group: backend
    disabled: true
` + sleepService
	cat, err := loadYAML(t, body)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := cat.Groups["backend"], []string{"api"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("backend: got %v, want %v", got, want)
	}
	if got, want := cat.Groups["all"], []string{"api"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("all: got %v, want %v", got, want)
	}
	wantTree := []catalog.CatalogGroup{{Name: "backend", Members: []string{"api", "db"}}}
	if !reflect.DeepEqual(cat.GroupTree, wantTree) {
		t.Fatalf("groupTree: got %v, want %v", cat.GroupTree, wantTree)
	}
}

func TestSharedEntriesMayDeclareAGroup(t *testing.T) {
	body := `version: 1
shared:
  postgres:
    version: "16.4"
    group: backend
services:
  api:
    group: backend
` + sleepService
	cat, err := loadYAML(t, body)
	if err != nil {
		t.Fatal(err)
	}
	if got, want := cat.Groups["backend"], []string{"api", "postgres"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("backend: got %v, want %v", got, want)
	}
	if got, want := cat.Groups["all"], []string{"api", "postgres"}; !reflect.DeepEqual(got, want) {
		t.Fatalf("all: got %v, want %v", got, want)
	}
}

func TestTheTopLevelGroupsBlockIsNoLongerAccepted(t *testing.T) {
	body := `version: 1
groups:
  all: [api]
services:
  api:
` + sleepService
	_, err := loadYAML(t, body)
	if err == nil || !strings.Contains(err.Error(), "no longer a top-level key") {
		t.Fatalf("expected a migration error, got %v", err)
	}
}

func TestGroupAllIsReserved(t *testing.T) {
	body := `version: 1
services:
  api:
    group: all
` + sleepService
	_, err := loadYAML(t, body)
	if err == nil || !strings.Contains(err.Error(), `must not be "all"`) {
		t.Fatalf("expected a reserved-group error, got %v", err)
	}
}

func TestGroupMustBeANonEmptyToken(t *testing.T) {
	for _, value := range []string{`""`, `"two words"`, `[]`} {
		body := `version: 1
services:
  api:
    group: ` + value + "\n" + sleepService
		_, err := loadYAML(t, body)
		if err == nil || !strings.Contains(err.Error(), "must be a non-empty string without spaces") {
			t.Fatalf("group %s: expected a shape error, got %v", value, err)
		}
	}
}

func TestTheRemovedTimeoutKeysAreRejected(t *testing.T) {
	cases := []struct{ name, body, unknown string }{
		{
			name:    "service readinessTimeoutMs",
			unknown: `"readinessTimeoutMs"`,
			body: `version: 1
services:
  api:
` + sleepService + `    readinessTimeoutMs: 30000
`,
		},
		{
			name:    "build timeoutMs",
			unknown: `"timeoutMs"`,
			body: `version: 1
services:
  api:
    build: { argv: [go, build], timeoutMs: 120000 }
` + sleepService,
		},
	}
	for _, tc := range cases {
		_, err := loadYAML(t, tc.body)
		if err == nil || !strings.Contains(err.Error(), "has unknown key "+tc.unknown) {
			t.Fatalf("%s: expected an unknown-key error, got %v", tc.name, err)
		}
	}
}
