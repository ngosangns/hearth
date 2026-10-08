// The smp-only HTTP surface (`/v1/shared/*`) — ported from
// rust/crates/hearth-core/src/manager/shared.rs, mounted by buildRoutes when
// the manager was built with a SharedContext. Orchestrates register → install →
// catalog-insert → start → provision per docs/shared-services.md. All mutations
// serialize on a per-instance lock so two projects racing `postgres@16.4`
// converge on one install.
//
// SharedContext itself is the Go port of `crate::shared::SharedContext`
// (rust/crates/hearth-core/src/shared/mod.rs): the instance registry, the
// remote catalog client, per-instance serialization, and one machine-wide port
// allocation lock. It lives here because the Go shared package exposes the
// pieces (SharedRegistry, RemoteCatalog, …) but not the context that binds
// them; a later agent wiring cmd/hearth constructs it with NewSharedContext.
package manager

import (
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/render"
	"github.com/ngosangns/hearth/go/internal/shared"
	"github.com/ngosangns/hearth/go/internal/state"
	"github.com/ngosangns/hearth/go/internal/supervisor"
	"github.com/ngosangns/hearth/go/internal/syncx"
)

const recipeCommandTimeout = 60 * time.Second

// SharedContext is everything the smp daemon's HTTP handlers need that isn't
// already on HearthManager: the instance registry, the remote catalog client,
// per-instance serialization for install/attach so two projects racing the same
// `name@version` can't double-install, and one machine-wide port allocation
// lock so two *different* new instances can't be handed the same port.
type SharedContext struct {
	Root       string
	Registry   *shared.SharedRegistry
	Remote     *shared.RemoteCatalog
	CatalogURL string

	io             fileio.FileIO
	instanceLocks  *syncx.KeyedLock[string]
	portAllocation sync.Mutex
}

// NewSharedContext opens the smp context at root. Catalog URL precedence: the
// explicit argument (HEARTH_SHARED_CATALOG_URL) first, then a `catalog-url`
// file under the shared root, then the pinned shared.SharedCatalogURL.
func NewSharedContext(root string, catalogURL *string) (*SharedContext, error) {
	io := fileio.New(true)
	if err := io.EnsureDirectory(root); err != nil {
		return nil, &shared.SharedError{Message: err.Error()}
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
		url = shared.SharedCatalogURL
	}
	registry, err := shared.LoadRegistry(io, root)
	if err != nil {
		return nil, err
	}
	return &SharedContext{
		Root:          root,
		Registry:      registry,
		Remote:        shared.NewRemoteCatalog(root, &url),
		CatalogURL:    url,
		io:            io,
		instanceLocks: syncx.NewKeyedLock[string](),
	}, nil
}

// RuntimeDirectory is the smp runtime directory (`<root>/runtime-v1`).
func (c *SharedContext) RuntimeDirectory() string {
	return filepath.Join(c.Root, shared.SharedRuntimeDirectoryName)
}

// DownloadsDir is where installers stage archives.
func (c *SharedContext) DownloadsDir() string { return filepath.Join(c.Root, "downloads") }

// InstanceLock serializes install/start/provision for one instance across
// concurrent attaches. The returned func releases the lock.
func (c *SharedContext) InstanceLock(id string) func() { return c.instanceLocks.Lock(id) }

// ---------------------------------------------------------------------------------------------
// Route table
// ---------------------------------------------------------------------------------------------

func sharedRoutes(m *HearthManager) []routeEntry {
	return []routeEntry{
		{"GET", []string{"v1", "shared"}, true, m.getShared},
		{"GET", []string{"v1", "shared", "catalog"}, true, m.getSharedCatalog},
		{"POST", []string{"v1", "shared", "attach"}, true, m.postSharedAttach},
		{"POST", []string{"v1", "shared", "detach"}, true, m.postSharedDetach},
		{"POST", []string{"v1", "shared", "install"}, true, m.postSharedInstall},
		{"POST", []string{"v1", "shared", "remove"}, true, m.postSharedRemove},
	}
}

// sharedCtx returns the manager's SharedContext. buildRoutes only mounts these
// routes when the manager has one, so this cannot fail in practice — it stays
// an error rather than a panic all the same.
func sharedCtx(m *HearthManager) (*SharedContext, *ManagerHttpError) {
	if m.shared == nil {
		return nil, newHTTPError(http.StatusNotFound, "not_shared", "This daemon does not manage shared services")
	}
	return m.shared, nil
}

func sharedErr(e error) *ManagerHttpError {
	return newHTTPError(http.StatusInternalServerError, "shared_error", e.Error())
}

func publishInstall(m *HearthManager, id string, installState shared.InstallState, message *string) {
	data := map[string]any{"service": id, "installState": string(installState)}
	if message != nil {
		data["message"] = *message
	}
	m.events.Publish("shared.install", data)
}

// parseInstanceID: `"postgres@16.4"` → `("postgres", "16.4")`. Versions may
// contain `.`/`_`; neither name nor version may be empty or contain another `@`.
func parseInstanceID(value any) (string, string, *ManagerHttpError) {
	raw, _ := value.(string)
	idx := strings.Index(raw, "@")
	if idx < 0 {
		return "", "", newHTTPError(http.StatusBadRequest, "invalid_service", "service must be <name>@<version>")
	}
	name := raw[:idx]
	version := raw[idx+1:]
	if name == "" || version == "" || strings.Contains(version, "@") {
		return "", "", newHTTPError(http.StatusBadRequest, "invalid_service", "service must be <name>@<version>")
	}
	return name, version, nil
}

// parseProjectRoot canonicalizes the body's `projectRoot` when it exists. An
// empty string is a 400 rather than a path of "" — that would hash the daemon's
// own cwd into a project id.
func parseProjectRoot(body map[string]any) (*string, *ManagerHttpError) {
	v, ok := body["projectRoot"]
	if !ok || v == nil {
		return nil, nil
	}
	s, isStr := v.(string)
	if !isStr || s == "" {
		return nil, newHTTPError(http.StatusBadRequest, "invalid_request", "projectRoot must be a non-empty string")
	}
	canonical := s
	if resolved, err := filepath.EvalSymlinks(s); err == nil {
		if abs, err2 := filepath.Abs(resolved); err2 == nil {
			canonical = abs
		} else {
			canonical = resolved
		}
	}
	return &canonical, nil
}

// ---------------------------------------------------------------------------------------------
// Read endpoints
// ---------------------------------------------------------------------------------------------

type sharedAttachmentView struct {
	ProjectID   string          `json:"projectId"`
	ProjectRoot string          `json:"projectRoot"`
	Provisioned bool            `json:"provisioned"`
	Connection  json.RawMessage `json:"connection"`
}

type sharedInstanceView struct {
	ID           string                 `json:"id"`
	Name         string                 `json:"name"`
	Version      string                 `json:"version"`
	Port         uint16                 `json:"port"`
	ExtraPorts   []uint16               `json:"extraPorts"`
	InstallState string                 `json:"installState"`
	InstallError *string                `json:"installError"`
	State        any                    `json:"state"`
	Attachments  []sharedAttachmentView `json:"attachments"`
}

type sharedResponse struct {
	Instances []sharedInstanceView `json:"instances"`
}

type sharedCatalogResponse struct {
	Catalog *shared.SharedCatalogDocument `json:"catalog"`
}

func (m *HearthManager) getShared(w http.ResponseWriter, _ *http.Request, _ map[string]string) {
	ctx, herr := sharedCtx(m)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	states := map[string]map[string]any{}
	for _, s := range m.ServiceStates() {
		states[s.ServiceID] = map[string]any{"actualState": string(s.ActualState), "readiness": string(s.Readiness)}
	}
	instances := []sharedInstanceView{}
	for _, i := range ctx.Registry.List() {
		id := i.ID()
		var stateView any
		if sv, ok := states[id]; ok {
			stateView = sv
		}
		extraPorts := i.ExtraPorts
		if extraPorts == nil {
			extraPorts = []uint16{}
		}
		attachments := []sharedAttachmentView{}
		for pid, a := range i.Attachments {
			attachments = append(attachments, sharedAttachmentView{
				ProjectID:   pid,
				ProjectRoot: a.ProjectRoot,
				Provisioned: a.Provisioned,
				Connection:  a.Connection,
			})
		}
		instances = append(instances, sharedInstanceView{
			ID:           id,
			Name:         i.Name,
			Version:      i.Version,
			Port:         i.Port,
			ExtraPorts:   extraPorts,
			InstallState: string(i.InstallState),
			InstallError: i.InstallError,
			State:        stateView,
			Attachments:  attachments,
		})
	}
	writeJSON(w, http.StatusOK, sharedResponse{Instances: instances})
}

func (m *HearthManager) getSharedCatalog(w http.ResponseWriter, _ *http.Request, _ map[string]string) {
	ctx, herr := sharedCtx(m)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	// Rust's `remote.load(false)` prefers a memo/cache; the Go RemoteCatalog
	// only exposes Document(), which always attempts the fetch first and falls
	// back to the cache — the refresh path, which is the one that matters here.
	doc, err := ctx.Remote.Document()
	if err != nil {
		writeHTTPError(w, sharedErr(err))
		return
	}
	writeJSON(w, http.StatusOK, sharedCatalogResponse{Catalog: doc})
}

// ---------------------------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------------------------

func (m *HearthManager) postSharedAttach(w http.ResponseWriter, r *http.Request, _ map[string]string) {
	result, herr := m.sharedAttach(r)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	writeJSON(w, http.StatusOK, result)
}

func (m *HearthManager) postSharedDetach(w http.ResponseWriter, r *http.Request, _ map[string]string) {
	result, herr := m.sharedDetach(r)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	writeJSON(w, http.StatusOK, result)
}

func (m *HearthManager) postSharedInstall(w http.ResponseWriter, r *http.Request, _ map[string]string) {
	if herr := m.ensureNotClosing(); herr != nil {
		writeHTTPError(w, herr)
		return
	}
	ctx, herr := sharedCtx(m)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	body, herr := strictBody(r, []string{"service"}, []string{"service"})
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	name, version, herr := parseInstanceID(body["service"])
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	id := shared.InstanceID(name, version)
	unlock := ctx.InstanceLock(id)
	defer unlock()
	instance, herr := m.ensureRegisteredAndInstalled(ctx, name, version)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"service":      instance.ID(),
		"port":         instance.Port,
		"installState": string(instance.InstallState),
	})
}

// postSharedRemove stops the instance, unregisters it and deletes its install
// and data directories. The data directory holds every attached project's
// databases, so an instance with attachments is refused (409
// `shared_service_attached`) unless the caller sends `force: true` after its own
// explicit confirmation.
func (m *HearthManager) postSharedRemove(w http.ResponseWriter, r *http.Request, _ map[string]string) {
	if herr := m.ensureNotClosing(); herr != nil {
		writeHTTPError(w, herr)
		return
	}
	ctx, herr := sharedCtx(m)
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	body, herr := strictBody(r, []string{"service", "force"}, []string{"service"})
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	name, version, herr := parseInstanceID(body["service"])
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	force, herr := parseBoolFlag(body, "force")
	if herr != nil {
		writeHTTPError(w, herr)
		return
	}
	id := shared.InstanceID(name, version)
	unlock := ctx.InstanceLock(id)
	defer unlock()
	instance := ctx.Registry.Get(id)
	if instance == nil {
		writeHTTPError(w, newHTTPError(http.StatusNotFound, "unknown_shared_service", id+" is not registered"))
		return
	}
	if !force && len(instance.Attachments) > 0 {
		projects := make([]string, 0, len(instance.Attachments))
		for _, a := range instance.Attachments {
			projects = append(projects, a.ProjectRoot)
		}
		writeHTTPError(w, newHTTPError(http.StatusConflict, "shared_service_attached", fmt.Sprintf(
			"%s is attached to %d project(s) (%s); removing it deletes their data — retry with force: true to remove anyway",
			id, len(projects), strings.Join(projects, ", "))))
		return
	}
	if err := m.supervisor.Stop(id, nil); err != nil {
		writeHTTPError(w, sharedErr(&shared.SharedError{Message: err.Error()}))
		return
	}
	if _, err := ctx.Registry.Remove(id); err != nil {
		writeHTTPError(w, sharedErr(err))
		return
	}
	if err := m.syncCatalog(ctx); err != nil {
		writeHTTPError(w, sharedErr(err))
		return
	}
	installDir := instance.InstallDir(ctx.Root)
	dataDir := instance.DataDir(ctx.Root)
	_ = fileio.RemoveDirectory(installDir)
	_ = fileio.RemoveDirectory(dataDir)
	writeJSON(w, http.StatusOK, map[string]any{"removed": id})
}

// ---------------------------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------------------------

// syncCatalog re-synthesizes smp's catalog from the registry and swaps it in —
// ReloadCatalog handles stopping removed-but-active services with the old
// catalog, so this is the only mutation path for smp's service set.
func (m *HearthManager) syncCatalog(ctx *SharedContext) error {
	cat, err := shared.SynthesizeCatalog(ctx.Root, ctx.Registry.List())
	if err != nil {
		return err
	}
	if _, err := m.ReloadCatalog(*cat); err != nil {
		return &shared.SharedError{Message: err.Error()}
	}
	return nil
}

// registerInstance picks the new instance's ports and inserts its registry row.
// Runs under the machine-wide portAllocation lock: per-instance locks don't
// serialize two *different* new ids, and two of those whose hash slots are close
// could otherwise both pass the bind probe and share a port.
func registerInstance(ctx *SharedContext, id, name, version string, recipe shared.SharedRecipe) *ManagerHttpError {
	ctx.portAllocation.Lock()
	defer ctx.portAllocation.Unlock()
	var ports []uint16
	if len(recipe.Ports) > 0 {
		// Pinned ports: the recipe IS its ports — nothing to probe, just a free
		// check.
		if len(recipe.Ports) > int(shared.MaxSharedPorts) {
			return newHTTPError(http.StatusBadRequest, "too_many_ports", fmt.Sprintf(
				"%s pins %d ports; the maximum is %d", id, len(recipe.Ports), shared.MaxSharedPorts))
		}
		if !shared.BindAll(recipe.Ports) {
			return newHTTPError(http.StatusConflict, "port_in_use", fmt.Sprintf(
				"%s cannot pin %s; one or more are in use", id, formatPorts(recipe.Ports)))
		}
		ports = recipe.Ports
	} else {
		count := recipe.AdditionalPorts
		if count < ^uint16(0) {
			count++
		}
		if count > shared.MaxSharedPorts {
			return newHTTPError(http.StatusBadRequest, "too_many_ports", fmt.Sprintf(
				"%s asks for %d ports; the maximum is %d", id, count, shared.MaxSharedPorts))
		}
		taken := map[uint16]bool{}
		for _, i := range ctx.Registry.List() {
			for _, p := range i.AllPorts() {
				taken[p] = true
			}
		}
		ports = shared.AllocatePorts(id, count, taken)
		if ports == nil {
			return newHTTPError(http.StatusInternalServerError, "no_free_port", "no free port block in the shared range")
		}
	}
	port := ports[0]
	extraPorts := append([]uint16{}, ports[1:]...)
	if err := ctx.Registry.Update(func(d *shared.SharedRegistryData) error {
		d.Instances[id] = shared.SharedInstance{
			Name:         name,
			Version:      version,
			Port:         port,
			ExtraPorts:   extraPorts,
			InstallState: shared.InstallPending,
			Recipe:       recipe,
		}
		return nil
	}); err != nil {
		return sharedErr(err)
	}
	return nil
}

func formatPorts(ports []uint16) string {
	parts := make([]string, len(ports))
	for i, p := range ports {
		parts[i] = strconv.Itoa(int(p))
	}
	return "[" + strings.Join(parts, ", ") + "]"
}

func setInstallState(ctx *SharedContext, id string, installState shared.InstallState, installError *string) *ManagerHttpError {
	if err := ctx.Registry.Update(func(d *shared.SharedRegistryData) error {
		if inst, ok := d.Instances[id]; ok {
			inst.InstallState = installState
			inst.InstallError = installError
			d.Instances[id] = inst
		}
		return nil
	}); err != nil {
		return sharedErr(err)
	}
	return nil
}

// ensureRegisteredAndInstalled registers `name@version` in the registry if
// absent (fetching the remote recipe + allocating a port), ensures it is
// installed, and makes sure the running catalog knows the service. Returns the
// registry row; the caller holds the instance lock.
func (m *HearthManager) ensureRegisteredAndInstalled(ctx *SharedContext, name, version string) (*shared.SharedInstance, *ManagerHttpError) {
	id := shared.InstanceID(name, version)
	instance := ctx.Registry.Get(id)
	if instance == nil {
		document, err := ctx.Remote.Document()
		if err != nil {
			return nil, newHTTPError(http.StatusBadGateway, "catalog_unavailable", err.Error())
		}
		recipe := document.Recipe(name, version)
		if recipe == nil {
			return nil, newHTTPError(http.StatusNotFound, "unknown_shared_service", id+" is not in the shared catalog")
		}
		if herr := registerInstance(ctx, id, name, version, *recipe); herr != nil {
			return nil, herr
		}
		if err := m.syncCatalog(ctx); err != nil {
			return nil, sharedErr(err)
		}
		instance = ctx.Registry.Get(id)
	}

	if isInstalled(ctx, instance) {
		if instance.InstallState != shared.InstallInstalled {
			if herr := setInstallState(ctx, id, shared.InstallInstalled, nil); herr != nil {
				return nil, herr
			}
		}
		return ctx.Registry.Get(id), nil
	}

	if herr := setInstallState(ctx, id, shared.InstallInstalling, nil); herr != nil {
		return nil, herr
	}
	publishInstall(m, id, shared.InstallInstalling, nil)
	progress := func(line string) {
		l := line
		publishInstall(m, id, shared.InstallInstalling, &l)
	}
	if err := ensureInstalled(ctx, instance, progress); err != nil {
		message := err.Error()
		if herr := setInstallState(ctx, id, shared.InstallFailed, &message); herr != nil {
			return nil, herr
		}
		publishInstall(m, id, shared.InstallFailed, &message)
		return nil, newHTTPError(http.StatusInternalServerError, "install_failed", message)
	}
	if herr := setInstallState(ctx, id, shared.InstallInstalled, nil); herr != nil {
		return nil, herr
	}
	publishInstall(m, id, shared.InstallInstalled, nil)
	return ctx.Registry.Get(id), nil
}

// isInstalled: `installs/<name>/<version>` is ready — marker present. The
// marker only exists after a fully extracted payload was renamed into place, so
// its presence implies the payload too.
func isInstalled(ctx *SharedContext, instance *shared.SharedInstance) bool {
	marker := filepath.Join(instance.InstallDir(ctx.Root), ".hearth-installed")
	info, err := os.Stat(marker)
	return err == nil && info.Mode().IsRegular()
}

// ensureInstalled is the idempotent install. onProgress receives
// human-readable lines the caller streams into an event.
func ensureInstalled(ctx *SharedContext, instance *shared.SharedInstance, onProgress func(string)) error {
	if isInstalled(ctx, instance) {
		return nil
	}
	artifact, ok := instance.Recipe.Artifacts[shared.SharedArtifactPlatform]
	if !ok {
		return &shared.SharedError{Message: instance.ID() + ": no artifact for " + shared.SharedArtifactPlatform}
	}
	installer := &shared.TarballInstaller{DownloadsDir: ctx.DownloadsDir(), CatalogURL: ctx.CatalogURL}
	_, err := installer.Install(
		instance.ID(),
		instance.InstallDir(ctx.Root),
		shared.ArtifactSpec{URL: artifact.URL, Script: artifact.Script, ScriptArgs: artifact.ScriptArgs, Sha256: artifact.Sha256},
		onProgress,
	)
	return err
}

func (m *HearthManager) sharedAttach(r *http.Request) (map[string]any, *ManagerHttpError) {
	if herr := m.ensureNotClosing(); herr != nil {
		return nil, herr
	}
	ctx, herr := sharedCtx(m)
	if herr != nil {
		return nil, herr
	}
	body, herr := strictBody(r, []string{"service", "projectRoot", "args"}, []string{"service"})
	if herr != nil {
		return nil, herr
	}
	attachArgs := []string{}
	if v, ok := body["args"]; ok && v != nil {
		items, isArr := v.([]any)
		if !isArr {
			return nil, newHTTPError(http.StatusBadRequest, "invalid_request", "args must be an array of strings")
		}
		for _, item := range items {
			s, isStr := item.(string)
			if !isStr {
				return nil, newHTTPError(http.StatusBadRequest, "invalid_request", "args must be an array of strings")
			}
			attachArgs = append(attachArgs, s)
		}
	}
	name, version, herr := parseInstanceID(body["service"])
	if herr != nil {
		return nil, herr
	}
	id := shared.InstanceID(name, version)
	projectRoot, herr := parseProjectRoot(body)
	if herr != nil {
		return nil, herr
	}

	unlock := ctx.InstanceLock(id)
	defer unlock()
	instance, herr := m.ensureRegisteredAndInstalled(ctx, name, version)
	if herr != nil {
		return nil, herr
	}
	// The synthesized run command uses dataDir as cwd — it must exist before
	// spawn.
	if err := os.MkdirAll(instance.DataDir(ctx.Root), 0o755); err != nil {
		return nil, sharedErr(&shared.SharedError{Message: "cannot create data dir: " + err.Error()})
	}

	// Start the instance. supervisor.Start already awaits readiness, so on Ok
	// the service is Ready; a failure lands as `failed` in state.json and
	// propagates as the attach error.
	alreadyReady := false
	if st := m.ServiceState(id); st != nil && st.ActualState == state.ActualReady {
		alreadyReady = true
	}
	if !alreadyReady {
		if err := m.supervisor.Start(id, nil); err != nil {
			return nil, newHTTPError(http.StatusInternalServerError, "start_failed", err.Error())
		}
	}

	result := map[string]any{"service": id, "port": instance.Port}
	if projectRoot == nil {
		return result, nil
	}

	pid := shared.ProjectID(*projectRoot)
	if existing := ctx.Registry.Get(id); existing != nil {
		if a, ok := existing.Attachments[pid]; ok && a.Provisioned {
			result["attachment"] = map[string]any{"projectId": pid, "projectRoot": *projectRoot, "connection": a.Connection}
			return result, nil
		}
	}

	// Every template is rendered and checked before anything runs: a typo'd
	// `{projectDB}` must fail the attach up front, not run literally — and a bad
	// connection template must not wait until after provisioning, where its
	// failure would skip recording the attachment and re-run provisioning on
	// every attach.
	vars := shared.InstanceVars(instance, ctx.Root)
	shared.ProjectVars(vars, pid)
	vars["projectRoot"] = *projectRoot
	invalidRecipe := func(e error) *ManagerHttpError {
		return newHTTPError(http.StatusInternalServerError, "invalid_recipe", e.Error())
	}
	commands := make([]catalog.CommandSpec, 0, len(instance.Recipe.Provision))
	for i := range instance.Recipe.Provision {
		cmd, err := render.RenderCommandChecked(&instance.Recipe.Provision[i], vars, "provision")
		if err != nil {
			return nil, invalidRecipe(err)
		}
		commands = append(commands, cmd)
	}
	var renderedConnection map[string]any
	if instance.Recipe.Connection != nil {
		conn, err := render.RenderConnection(&render.SharedConnectionLike{
			URL: instance.Recipe.Connection.URL,
			Env: instance.Recipe.Connection.Env,
		}, vars)
		if err != nil {
			return nil, invalidRecipe(err)
		}
		renderedConnection = conn
	}

	// Provision this project's logical resources (e.g. its own database+user).
	// A failure is recorded on the attachment so the next attach retries the
	// commands — recipes must be idempotent.
	var provisionError *string
	for i := range commands {
		command := commands[i]
		// Attach args append to the rendered argv — e.g. the shared nginx
		// recipe takes the attaching project's rendered conf directory as a
		// trailing argument.
		if len(attachArgs) > 0 {
			if command.IsArgv() {
				command.Argv = append(command.Argv, attachArgs...)
			} else {
				msg := "attach args require argv provision commands"
				provisionError = &msg
				break
			}
		}
		code, err := runRecipeCommand(&command, instance.DataDir(ctx.Root))
		if err != nil {
			msg := err.Error()
			provisionError = &msg
			break
		}
		if code != 0 {
			msg := fmt.Sprintf("provision command exited with %d", code)
			provisionError = &msg
			break
		}
	}

	var connection json.RawMessage
	if provisionError == nil && renderedConnection != nil {
		connection, _ = json.Marshal(renderedConnection)
	}
	if err := ctx.Registry.Update(func(d *shared.SharedRegistryData) error {
		if inst, ok := d.Instances[id]; ok {
			if inst.Attachments == nil {
				inst.Attachments = map[string]shared.SharedAttachment{}
			}
			inst.Attachments[pid] = shared.SharedAttachment{
				ProjectRoot: *projectRoot,
				Provisioned: provisionError == nil,
				Connection:  connection,
				Error:       provisionError,
				AttachedAt:  now(),
			}
			d.Instances[id] = inst
		}
		return nil
	}); err != nil {
		return nil, sharedErr(err)
	}
	m.events.Publish("shared.attach", map[string]any{"service": id, "projectId": pid, "provisioned": provisionError == nil})
	if provisionError != nil {
		return nil, newHTTPError(http.StatusInternalServerError, "provision_failed", *provisionError)
	}
	result["attachment"] = map[string]any{"projectId": pid, "projectRoot": *projectRoot, "connection": connection}
	return result, nil
}

func (m *HearthManager) sharedDetach(r *http.Request) (map[string]any, *ManagerHttpError) {
	if herr := m.ensureNotClosing(); herr != nil {
		return nil, herr
	}
	ctx, herr := sharedCtx(m)
	if herr != nil {
		return nil, herr
	}
	body, herr := strictBody(r, []string{"service", "projectRoot", "stopIfUnused"}, []string{"service", "projectRoot"})
	if herr != nil {
		return nil, herr
	}
	name, version, herr := parseInstanceID(body["service"])
	if herr != nil {
		return nil, herr
	}
	id := shared.InstanceID(name, version)
	projectRoot, herr := parseProjectRoot(body)
	if herr != nil {
		return nil, herr
	}
	if projectRoot == nil {
		return nil, newHTTPError(http.StatusBadRequest, "invalid_request", "projectRoot must be a non-empty string")
	}
	// `manager stop` in a project: also stop the instance once no project is
	// attached to it. Never while another project still is.
	stopIfUnused := false
	if v, ok := body["stopIfUnused"]; ok && v != nil {
		b, isBool := v.(bool)
		if !isBool {
			return nil, newHTTPError(http.StatusBadRequest, "invalid_request", "stopIfUnused must be a boolean")
		}
		stopIfUnused = b
	}
	pid := shared.ProjectID(*projectRoot)

	unlock := ctx.InstanceLock(id)
	defer unlock()
	instance := ctx.Registry.Get(id)
	if instance == nil {
		return nil, newHTTPError(http.StatusNotFound, "unknown_shared_service", id+" is not registered")
	}
	if _, ok := instance.Attachments[pid]; !ok {
		// Detaching what was never attached is a no-op — `stop` in a project
		// must be idempotent.
		stopped := false
		if stopIfUnused && len(instance.Attachments) == 0 {
			var herr *ManagerHttpError
			stopped, herr = m.stopUnusedInstance(id)
			if herr != nil {
				return nil, herr
			}
		}
		return map[string]any{"detached": id, "projectId": pid, "stopped": stopped}, nil
	}

	// Best-effort deprovision: a failing teardown must not block the detach
	// itself — the project is leaving either way, and the attachment row is
	// what the probe keys on. A step whose template doesn't render is skipped
	// rather than run with a literal `{var}` in it.
	vars := shared.InstanceVars(instance, ctx.Root)
	shared.ProjectVars(vars, pid)
	vars["projectRoot"] = *projectRoot
	for i := range instance.Recipe.Deprovision {
		if cmd, err := render.RenderCommandChecked(&instance.Recipe.Deprovision[i], vars, "deprovision"); err == nil {
			_, _ = runRecipeCommand(&cmd, instance.DataDir(ctx.Root))
		}
	}
	if err := ctx.Registry.Update(func(d *shared.SharedRegistryData) error {
		if inst, ok := d.Instances[id]; ok {
			delete(inst.Attachments, pid)
			d.Instances[id] = inst
		}
		return nil
	}); err != nil {
		return nil, sharedErr(err)
	}
	m.events.Publish("shared.detach", map[string]any{"service": id, "projectId": pid})
	// Re-read under the same instance lock: an attach for another project
	// cannot slip in between this check and the stop.
	unused := false
	if inst := ctx.Registry.Get(id); inst != nil && len(inst.Attachments) == 0 {
		unused = true
	}
	stopped := false
	if stopIfUnused && unused {
		var herr *ManagerHttpError
		stopped, herr = m.stopUnusedInstance(id)
		if herr != nil {
			return nil, herr
		}
	}
	return map[string]any{"detached": id, "projectId": pid, "stopped": stopped}, nil
}

// stopUnusedInstance stops a shared instance nobody is attached to. false when
// it was not running.
func (m *HearthManager) stopUnusedInstance(id string) (bool, *ManagerHttpError) {
	running := false
	if st := m.ServiceState(id); st != nil {
		running = isActiveState(st.ActualState)
	}
	if !running {
		return false, nil
	}
	if err := m.supervisor.Stop(id, nil); err != nil {
		return false, newHTTPError(http.StatusInternalServerError, "shared_stop_failed", fmt.Sprintf(
			"%s was detached but could not be stopped: %s", id, err.Error()))
	}
	m.events.Publish("shared.stop", map[string]any{"service": id, "reason": "unused"})
	return true, nil
}

// runRecipeCommand runs one rendered recipe command (provision/deprovision)
// with a bounded timeout and its output discarded — provision output is not a
// service log; failures surface via the exit code. A timeout kills the
// command's whole process group, not just the direct child.
func runRecipeCommand(command *catalog.CommandSpec, cwd string) (int, error) {
	argv, _ := supervisor.CommandArgv(command)
	if len(argv) == 0 {
		return 0, &shared.SharedError{Message: "empty recipe command"}
	}
	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Dir = cwd
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := cmd.Start(); err != nil {
		return 0, &shared.SharedError{Message: fmt.Sprintf("failed to spawn %q: %v", argv[0], err)}
	}
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	select {
	case err := <-done:
		if err != nil {
			if exitErr, ok := err.(*exec.ExitError); ok {
				return exitErr.ExitCode(), nil
			}
			return 0, &shared.SharedError{Message: fmt.Sprintf("failed to spawn %q: %v", argv[0], err)}
		}
		return 0, nil
	case <-time.After(recipeCommandTimeout):
		_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
		<-done
		return 0, &shared.SharedError{Message: fmt.Sprintf("recipe command timed out: %q", argv[0])}
	}
}
