package shared

import (
	"os"
	"path/filepath"
	"strings"
	"sync"

	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/syncx"
)

// Context is everything the smp daemon needs beyond HearthManager: instance
// registry, remote catalog client, per-instance serialization, and port
// allocation.
type Context struct {
	Root       string
	Registry   *SharedRegistry
	Remote     *RemoteCatalog
	CatalogURL string

	io             fileio.FileIO
	instanceLocks  *syncx.KeyedLock[string]
	portAllocation sync.Mutex
}

// NewContext opens the smp context at root. Catalog URL precedence: explicit
// argument (HEARTH_SHARED_CATALOG_URL), then catalog-url under root, then
// SharedCatalogURL.
func NewContext(root string, catalogURL *string) (*Context, error) {
	io := fileio.New(true)
	if err := io.EnsureDirectory(root); err != nil {
		return nil, &SharedError{Message: err.Error()}
	}
	url := ""
	if catalogURL != nil {
		url = *catalogURL
	}
	if url == "" {
		if text, err := os.ReadFile(filepath.Join(root, "catalog-url")); err == nil {
			url = strings.TrimSpace(string(text))
		}
	}
	if url == "" {
		url = SharedCatalogURL
	}
	registry, err := LoadRegistry(io, root)
	if err != nil {
		return nil, err
	}
	return &Context{
		Root:          root,
		Registry:      registry,
		Remote:        NewRemoteCatalog(root, &url),
		CatalogURL:    url,
		io:            io,
		instanceLocks: syncx.NewKeyedLock[string](),
	}, nil
}

func (c *Context) RuntimeDirectory() string {
	return filepath.Join(c.Root, SharedRuntimeDirectoryName)
}

func (c *Context) DownloadsDir() string { return filepath.Join(c.Root, "downloads") }

func (c *Context) InstanceLock(id string) func() { return c.instanceLocks.Lock(id) }

func (c *Context) PortAllocationLock() func() {
	c.portAllocation.Lock()
	return func() { c.portAllocation.Unlock() }
}
