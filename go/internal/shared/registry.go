// registry.json — smp's instance table. One writer (the smp daemon); CLI
// processes only ever mutate it through the daemon's HTTP API. Follows the
// same discipline as state.json: atomic writes, corrupt file quarantined.
package shared

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sync"

	"github.com/ngosangns/hearth/go/internal/fileio"
)

const registryFileName = "registry.json"
const registryVersion = 1

// InstallState is kebab-case on the wire.
type InstallState string

const (
	InstallPending    InstallState = "pending"
	InstallInstalling InstallState = "installing"
	InstallInstalled  InstallState = "installed"
	InstallFailed     InstallState = "failed"
)

// SharedAttachment is one attached project. `connection` is the recipe's
// connection block rendered for this project's template vars, cached at
// provision time so re-attaches are cheap and stable.
type SharedAttachment struct {
	ProjectRoot string          `json:"projectRoot"`
	Provisioned bool            `json:"provisioned"`
	Connection  json.RawMessage `json:"connection,omitempty"`
	Error       *string         `json:"error,omitempty"`
	AttachedAt  string          `json:"attachedAt"`
}

// SharedInstance is a registered `name@version` singleton. `recipe` is the
// snapshot taken at registration — installs never consult the remote registry
// again for this instance.
type SharedInstance struct {
	Name        string  `json:"name"`
	Version     string  `json:"version"`
	// Resolved listen port — derived deterministically, collision-shifted at
	// registration. Extra listeners live in ExtraPorts, contiguous after this.
	Port       uint16   `json:"port"`
	ExtraPorts []uint16 `json:"extraPorts,omitempty"`
	InstallState InstallState `json:"installState"`
	InstallError *string      `json:"installError,omitempty"`
	Recipe       SharedRecipe `json:"recipe"`
	// Keyed by project id.
	Attachments map[string]SharedAttachment `json:"attachments"`
}

func (i *SharedInstance) ID() string { return InstanceID(i.Name, i.Version) }
func (i *SharedInstance) InstallDir(root string) string {
	return filepath.Join(root, "installs", i.Name, i.Version)
}
func (i *SharedInstance) DataDir(root string) string {
	return filepath.Join(root, "instances", i.ID())
}

// AllPorts returns the primary port, then every extra port.
func (i *SharedInstance) AllPorts() []uint16 {
	return append([]uint16{i.Port}, i.ExtraPorts...)
}

type SharedRegistryData struct {
	Instances map[string]SharedInstance `json:"instances"`
}

type registryFile struct {
	Version uint32 `json:"version"`
	SharedRegistryData
}

type SharedRegistry struct {
	io   fileio.FileIO
	path string
	mu   sync.Mutex
	data SharedRegistryData
}

// Load reads registry.json; a present-but-corrupt file is renamed aside and
// the registry starts empty — same quarantine discipline as state.json,
// because a panic here would kill the daemon at bootstrap.
func LoadRegistry(io fileio.FileIO, root string) (*SharedRegistry, error) {
	path := filepath.Join(root, registryFileName)
	data := SharedRegistryData{Instances: map[string]SharedInstance{}}
	if text, err := io.ReadFile(path); err != nil {
		return nil, err
	} else if text != nil {
		var file registryFile
		if err := json.Unmarshal([]byte(*text), &file); err == nil && file.Version == registryVersion {
			if file.Instances == nil {
				file.Instances = map[string]SharedInstance{}
			}
			data = file.SharedRegistryData
		} else {
			_ = io.Quarantine(path, "corrupt")
		}
	}
	return &SharedRegistry{io: io, path: path, data: data}, nil
}

func (r *SharedRegistry) List() []SharedInstance {
	r.mu.Lock()
	defer r.mu.Unlock()
	out := make([]SharedInstance, 0, len(r.data.Instances))
	for _, inst := range r.data.Instances {
		out = append(out, inst)
	}
	return out
}

func (r *SharedRegistry) Get(id string) *SharedInstance {
	r.mu.Lock()
	defer r.mu.Unlock()
	if inst, ok := r.data.Instances[id]; ok {
		copy := inst
		return &copy
	}
	return nil
}

// Update mutates + persists atomically. The closure runs under the registry
// mutex; keep it small. The file write happens under the same mutex: writing
// after unlocking let two concurrent updates land on disk out of order.
func (r *SharedRegistry) Update(fn func(*SharedRegistryData) error) error {
	r.mu.Lock()
	defer r.mu.Unlock()
	if err := fn(&r.data); err != nil {
		return err
	}
	text, err := json.MarshalIndent(&registryFile{registryVersion, r.data}, "", "  ")
	if err != nil {
		return err
	}
	return r.io.WriteFile(r.path, string(text))
}

func (r *SharedRegistry) Remove(id string) (*SharedInstance, error) {
	var removed *SharedInstance
	err := r.Update(func(d *SharedRegistryData) error {
		if inst, ok := d.Instances[id]; ok {
			copy := inst
			removed = &copy
			delete(d.Instances, id)
		}
		return nil
	})
	return removed, err
}

// For tests: persist directly.
func SaveRegistryFile(root string, data *SharedRegistryData) error {
	text, err := json.MarshalIndent(&registryFile{registryVersion, *data}, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(root, registryFileName), text, 0o644)
}
