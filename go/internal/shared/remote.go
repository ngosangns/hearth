// Remote catalog types — the document fetched over HTTPS listing available
// shared-service recipes. Tarball sha256s live inside it, so the file's own
// integrity is exactly TLS's.
package shared

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
)

const fetchTimeout = 20 * time.Second
const cacheFileName = "catalog.json"

// SharedArtifact is where the darwin-arm64 tarball comes from. Exactly one of
// URL or Script.
type SharedArtifact struct {
	Sha256 string `json:"sha256"`
	URL    string `json:"url,omitempty"`
	// A path relative to the catalog file — or an absolute http(s)/file URL —
	// run as `bash <script> <scriptArgs...> <out.tar.gz>`.
	Script     string   `json:"script,omitempty"`
	ScriptArgs []string `json:"scriptArgs,omitempty"`
}

func (a *SharedArtifact) Validate(what string) error {
	url := a.URL != ""
	script := a.Script != ""
	switch {
	case url && script:
		return Errorf("%s: artifact must set url or script, not both", what)
	case !url && !script:
		return Errorf("%s: artifact needs a url or a script", what)
	}
	return nil
}

// SharedConnection describes how to connect to a provisioned instance. `url`
// and every `env` value are templates rendered per-attachment.
type SharedConnection struct {
	URL *string           `json:"url,omitempty"`
	Env map[string]string `json:"env,omitempty"`
}

// RecipeReadiness is a recipe's readiness before the allocated port exists:
// either a normal ReadinessSpec or `{"kind":"tcp"}` meaning "the instance's
// allocated primary port".
type RecipeReadiness struct {
	PrimaryTcp bool // {"kind":"tcp"} with no port
	Spec       catalog.ReadinessSpec
}

func (r *RecipeReadiness) Resolve(primaryPort uint16) catalog.ReadinessSpec {
	if r.PrimaryTcp {
		return catalog.ReadinessSpec{Kind: "tcp", Port: &primaryPort}
	}
	return r.Spec
}

func (r RecipeReadiness) MarshalJSON() ([]byte, error) {
	if r.PrimaryTcp {
		return json.Marshal(map[string]string{"kind": "tcp"})
	}
	return json.Marshal(r.Spec)
}

func (r *RecipeReadiness) UnmarshalJSON(data []byte) error {
	var value map[string]json.RawMessage
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	var spec catalog.ReadinessSpec
	if err := json.Unmarshal(data, &spec); err != nil {
		return err
	}
	kind := spec.Kind
	portPresent := false
	if p, ok := value["port"]; ok && string(p) != "null" {
		portPresent = true
	}
	if kind == "tcp" && !portPresent {
		r.PrimaryTcp = true
		return nil
	}
	if kind == "http" {
		var v struct {
			Path *string `json:"path"`
			Port *uint16 `json:"port"`
			URL  *string `json:"url"`
		}
		if err := json.Unmarshal(data, &v); err != nil {
			return err
		}
		hasPath := v.Path != nil
		hasPort := v.Port != nil
		hasURL := v.URL != nil && *v.URL != ""
		if hasURL && (hasPath || hasPort) {
			return fmt.Errorf("readiness http must not set url together with path or port")
		}
		if hasPort {
			return fmt.Errorf("readiness http on a shared recipe uses path or url, not a fixed port")
		}
		if hasURL {
			r.Spec = spec
			return nil
		}
		path := "/health"
		if hasPath {
			if !validHealthPath(*v.Path) {
				return fmt.Errorf("readiness path must be an absolute path")
			}
			path = *v.Path
		}
		r.Spec = catalog.ReadinessSpec{Kind: "http", URL: "http://127.0.0.1:{port}" + path}
		return nil
	}
	r.Spec = spec
	return nil
}

func validHealthPath(path string) bool {
	return strings.HasPrefix(path, "/") &&
		!strings.ContainsAny(path, " \t\n\r") &&
		!strings.Contains(path, "://") &&
		!strings.ContainsAny(path, "{}")
}

// SharedRecipe is everything needed to install+run+provision one
// `name@version`, snapshotted into registry.json at registration time.
type SharedRecipe struct {
	Artifacts map[string]SharedArtifact `json:"artifacts"`
	Run       catalog.CommandSpec       `json:"run"`
	Stop      *catalog.CommandSpec      `json:"stop,omitempty"`
	Readiness RecipeReadiness           `json:"readiness"`
	// Per-project provisioning, run once per attaching project. Commands must
	// be idempotent (re-attach reuses the cached result).
	Provision   []catalog.CommandSpec `json:"provision,omitempty"`
	Deprovision []catalog.CommandSpec `json:"deprovision,omitempty"`
	Connection  *SharedConnection     `json:"connection,omitempty"`
	Env         map[string]string     `json:"env,omitempty"`
	// Idempotent setup run before every start (mapped onto the supervisor's
	// preparationCommand). Exit 0 when already initialized.
	Prepare         *catalog.CommandSpec `json:"prepare,omitempty"`
	AdditionalPorts uint16               `json:"additionalPorts,omitempty"`
	// Explicitly pinned ports — bypasses hash allocation entirely. Mutually
	// exclusive with additionalPorts; capped by MaxSharedPorts.
	Ports           []uint16 `json:"ports,omitempty"`
	ExtraPortLabels []string `json:"extraPortLabels,omitempty"`
}

func (r *SharedRecipe) ExtraPortCount() int {
	if len(r.Ports) > 0 {
		return len(r.Ports) - 1
	}
	return int(r.AdditionalPorts)
}

type SharedServiceFamily struct {
	Versions map[string]SharedRecipe `json:"versions"`
}

type SharedCatalogDocument struct {
	Version  uint32                          `json:"version"`
	Services map[string]SharedServiceFamily  `json:"services"`
}

func (d *SharedCatalogDocument) Recipe(name, version string) *SharedRecipe {
	f, ok := d.Services[name]
	if !ok {
		return nil
	}
	r, ok := f.Versions[version]
	if !ok {
		return nil
	}
	return &r
}

// RemoteCatalog caches and serves the catalog document.
type RemoteCatalog struct {
	root       string
	catalogURL *string
}

func NewRemoteCatalog(root string, catalogURL *string) *RemoteCatalog {
	return &RemoteCatalog{root: root, catalogURL: catalogURL}
}

func (rc *RemoteCatalog) cachePath() string { return filepath.Join(rc.root, cacheFileName) }

func parseDocument(text string) (*SharedCatalogDocument, error) {
	var doc SharedCatalogDocument
	if err := json.Unmarshal([]byte(text), &doc); err != nil {
		return nil, Errorf("shared catalog is not valid JSON: %v", err)
	}
	if doc.Version != 1 {
		return nil, Errorf("shared catalog version must be 1, got %d", doc.Version)
	}
	for name, family := range doc.Services {
		for version, recipe := range family.Versions {
			for platform, artifact := range recipe.Artifacts {
				a := artifact
				if err := a.Validate(fmt.Sprintf("%s@%s %s", name, version, platform)); err != nil {
					return nil, err
				}
			}
			if len(recipe.Ports) > 0 && recipe.AdditionalPorts > 0 {
				return nil, Errorf("%s@%s declares both ports and additionalPorts", name, version)
			}
			if len(recipe.Ports) > int(MaxSharedPorts) {
				return nil, Errorf("%s@%s pins %d ports; the maximum is %d", name, version, len(recipe.Ports), MaxSharedPorts)
			}
		}
	}
	return &doc, nil
}

// mergeMissingRecipes adds every name@version from other that doc doesn't
// already have.
func mergeMissingRecipes(doc, other *SharedCatalogDocument) {
	for name, family := range other.Services {
		target, ok := doc.Services[name]
		if !ok {
			target = SharedServiceFamily{Versions: map[string]SharedRecipe{}}
			doc.Services[name] = target
		}
		for version, recipe := range family.Versions {
			if _, ok := target.Versions[version]; !ok {
				target.Versions[version] = recipe
			}
		}
	}
}

// Document returns the catalog: fetched (cached on success) or cache/embedded
// fallback.
func (rc *RemoteCatalog) Document() (*SharedCatalogDocument, error) {
	url := SharedCatalogURL
	if rc.catalogURL != nil && *rc.catalogURL != "" {
		url = *rc.catalogURL
	}
	var fetchedErr error
	client := &http.Client{Timeout: fetchTimeout}
	if resp, err := client.Get(url); err == nil {
		if resp.StatusCode == http.StatusOK {
			body, err := io.ReadAll(resp.Body)
			resp.Body.Close()
			if err == nil {
				if doc, perr := parseDocument(string(body)); perr == nil {
					_ = os.WriteFile(rc.cachePath(), body, 0o644)
					return doc, nil
				} else {
					fetchedErr = perr
				}
			} else {
				fetchedErr = err
			}
		} else {
			resp.Body.Close()
			fetchedErr = fmt.Errorf("status %d", resp.StatusCode)
		}
	} else {
		fetchedErr = err
	}
	if data, err := os.ReadFile(rc.cachePath()); err == nil {
		if doc, perr := parseDocument(string(data)); perr == nil {
			return doc, nil
		}
	}
	return nil, Errorf("could not load the shared catalog: %v", fetchedErr)
}
