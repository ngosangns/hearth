// The `hearth shared …` CLI surface — the machine-global smp daemon
// (docs/shared-services.md). These commands deliberately do not require a
// project `hearth.yaml`: `attach`/`detach`/`probe` derive the project identity
// from `--root`/cwd so the generated `shared:` service commands work in any
// directory a project daemon runs them from.
//
// Ported from rust/crates/hearth-cli/src/shared.rs. The smp discovery/ensure
// and the attach/detach/probe/remove/install request shapes live in
// internal/client (DiscoverSMP, EnsureSMP, SharedAttach, …); this file is the
// command dispatch on top of them.
package cli

import (
	"context"
	"fmt"
	"os"
	"sort"
	"strings"

	"github.com/ngosangns/hearth/go/internal/client"
	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/omap"
	"github.com/ngosangns/hearth/go/internal/shared"
	"github.com/ngosangns/hearth/go/internal/state"
)

// RunShared runs one `hearth shared …` command and returns the process exit
// code. spawnSMP starts a detached `hearth smp` when none is running.
func RunShared(ctx context.Context, root string, argv []string, io *Io, spawnSMP func(root string)) int {
	code, err := runSharedInner(ctx, root, argv, io, spawnSMP)
	if err != nil {
		if !hasFlag(argv, "--json") && isSharedConnection(argv) {
			code, _ = finishNew(io, false, err)
			return code
		}
		return emitFailure(io, hasFlag(argv, "--json"), err)
	}
	return code
}

// loadSharedInstances is GET /v1/shared. Tests replace it so a read-only
// command never discovers a real smp.
var loadSharedInstances = defaultLoadSharedInstances

// SetSharedInstanceLoader replaces the smp status loader used by
// `shared status` and `shared connection`. The returned function restores it.
func SetSharedInstanceLoader(fn func(context.Context) (map[string]any, error)) func() {
	prev := loadSharedInstances
	if fn == nil {
		loadSharedInstances = defaultLoadSharedInstances
	} else {
		loadSharedInstances = fn
	}
	return func() { loadSharedInstances = prev }
}

func defaultLoadSharedInstances(ctx context.Context) (map[string]any, error) {
	c := liveSMPClient(client.DiscoverSMP(ctx))
	if c == nil {
		return nil, failErrHint(ExitUnavailable, "smp is not running", "run: hearth shared ensure")
	}
	body, err := client.SMPRequest(ctx, c, "/v1/shared", "GET", nil)
	if err != nil {
		return nil, failErr(ExitUnavailable, err.Error())
	}
	return body, nil
}

func isSharedConnection(argv []string) bool {
	for _, arg := range argv {
		if arg == "--" {
			return false
		}
		if arg == "connection" {
			return true
		}
	}
	return false
}

func sharedConnectionHelp(args []string) bool {
	sawConnection := false
	for _, arg := range args {
		if arg == "--" {
			return false
		}
		if !sawConnection {
			if arg == "connection" {
				sawConnection = true
			}
			continue
		}
		if arg == "--help" || arg == "-h" || arg == "help" {
			return true
		}
	}
	return false
}

// splitPassthrough splits args into the part flags are parsed from and verbatim
// trailing arguments: everything after a `--`, and — for `attach` — everything
// after the instance id, since those are the recipe's provision arguments and
// may themselves start with `--`.
func splitPassthrough(args []string) ([]string, []string) {
	positionals := 0
	attach := false
	for index, argument := range args {
		if argument == "--" {
			return args[:index], args[index+1:]
		}
		if strings.HasPrefix(argument, "--") {
			continue
		}
		positionals++
		if positionals == 1 {
			attach = argument == "attach"
		} else if attach {
			return args[:index+1], args[index+1:]
		}
	}
	return args, nil
}

// parseSharedID validates a `<name>@<version>` instance id.
func parseSharedID(raw *string) (string, error) {
	if raw == nil {
		return "", usageErr("expected <name>@<version>")
	}
	id := *raw
	index := strings.Index(id, "@")
	if index < 0 {
		return "", usageErr(fmt.Sprintf("expected <name>@<version>, got %s", id))
	}
	name := id[:index]
	version := id[index+1:]
	if name == "" || version == "" || strings.Contains(version, "@") {
		return "", usageErr(fmt.Sprintf("expected <name>@<version>, got %s", id))
	}
	return id, nil
}

// liveSMPClient is `discover` that never spawns — for read-only commands
// (`status`, `probe`) that must not bring a daemon up as a side effect.
func liveSMPClient(discovery client.Discovery) *client.Client {
	if discovery.Kind == client.DiscoveryLive {
		return discovery.Client
	}
	return nil
}

func runSharedInner(ctx context.Context, root string, args []string, io *Io, spawnSMP func(root string)) (int, error) {
	if text, ok := HelpFor(append([]string{"shared"}, args...)); ok {
		out(io, text)
		return 0, nil
	}
	parsed, passthrough := splitPassthrough(args)
	flags, err := ParseCommandFlags(parsed, []Flag{FlagJSON, FlagForce, FlagStopIfUnused, FlagReveal, FlagMask, FlagYes})
	if err != nil {
		return 0, err
	}
	if len(flags.Positionals) == 0 {
		return 0, usageErr(sharedUsage)
	}
	subcommand := flags.Positionals[0]
	if flags.Force && subcommand != "remove" {
		return 0, usageErr("--force is only supported by `shared remove`")
	}
	if flags.StopIfUnused && subcommand != "detach" {
		return 0, usageErr("--stop-if-unused is only supported by `shared detach`")
	}
	if flags.Yes && subcommand != "remove" && subcommand != "detach" {
		return 0, usageErr("--yes is only supported by `shared remove` and `shared detach`")
	}
	if flags.Mask && subcommand != "status" && subcommand != "installed" {
		return 0, usageErr("--mask is only supported by `shared status` and `shared installed`")
	}
	if flags.Reveal && subcommand != "status" && subcommand != "installed" && subcommand != "connection" {
		return 0, usageErr("--reveal is only supported by `shared status`, `shared installed`, and `shared connection`")
	}
	// --mask is an explicit no-op: status and installed mask unless --reveal.
	// Together the two flags are a usage error.
	if flags.Mask && flags.Reveal {
		return 0, usageErr("--mask and --reveal cannot be used together")
	}
	rest := append([]string{}, flags.Positionals[1:]...)
	// `attach <id> -- ...`: the split for attach happens AT the id, so a `--`
	// separator written after it would otherwise be forwarded verbatim into the
	// recipe's provision argv.
	if subcommand == "attach" && len(passthrough) > 0 && passthrough[0] == "--" {
		passthrough = passthrough[1:]
	}
	rest = append(rest, passthrough...)

	switch subcommand {
	// The `manager ensure --json` contract for smp — find-or-start the shared
	// daemon and print the connection a client needs to talk to it directly.
	case "ensure":
		c, err := client.EnsureSMP(ctx, spawnSMP)
		if err != nil {
			return 0, err
		}
		out(io, printValue(c.EnsurePayload(), flags.JSON))
		return 0, nil
	// The remote registry — readable without smp running.
	case "list":
		remote := shared.NewRemoteCatalog(shared.SharedRoot(), envOrNil("HEARTH_SHARED_CATALOG_URL"))
		doc, err := remote.Document()
		if err != nil {
			return 0, failErr(ExitUnavailable, err.Error())
		}
		if flags.JSON {
			out(io, prettyJSON(jsonValue(doc)))
		} else {
			for name, family := range doc.Services {
				versions := make([]string, 0, len(family.Versions))
				for version := range family.Versions {
					versions = append(versions, version)
				}
				sort.Strings(versions)
				out(io, fmt.Sprintf("%s  %s", name, strings.Join(versions, ", ")))
			}
		}
		return 0, nil
	// The local registry — what this machine has installed/running.
	case "installed":
		registry, err := shared.LoadRegistry(fileio.New(false), shared.SharedRoot())
		if err != nil {
			return 0, failErr(ExitUnavailable, err.Error())
		}
		instances := registry.List()
		sort.Slice(instances, func(i, j int) bool {
			if instances[i].Name != instances[j].Name {
				return instances[i].Name < instances[j].Name
			}
			return instances[i].Version < instances[j].Version
		})
		if flags.JSON {
			emitSharedJSON(io, map[string]any{"instances": instances}, !flags.Reveal)
		} else {
			for _, instance := range instances {
				out(io, fmt.Sprintf("%s  port %d  %s  (%d project(s) attached)",
					instance.ID(), instance.Port, string(instance.InstallState), len(instance.Attachments)))
			}
		}
		return 0, nil
	case "status":
		body, err := loadSharedInstances(ctx)
		if err != nil {
			return 0, err
		}
		sortSharedStatusInstances(body)
		// Text is id, state, and port — no secret values. JSON masks secrets
		// unless --reveal. --mask is accepted and changes nothing.
		if flags.JSON {
			emitSharedJSON(io, body, !flags.Reveal)
		} else {
			for _, entry := range omap.AsArray(body["instances"]) {
				instance, ok := entry.(map[string]any)
				if !ok {
					continue
				}
				actualState := "stopped"
				if stateObj, ok := instance["state"].(map[string]any); ok {
					if s, ok := stateObj["actualState"].(string); ok {
						actualState = s
					}
				}
				id, _ := instance["id"].(string)
				installState, _ := instance["installState"].(string)
				out(io, fmt.Sprintf("%s  %s  port %v  %s", id, actualState, instance["port"], installState))
			}
		}
		return 0, nil
	case "connection":
		if len(rest) != 1 {
			return 0, usageErr(connectionUsage)
		}
		body, err := loadSharedInstances(ctx)
		if err != nil {
			return 0, err
		}
		doc, err := selectConnection(body, rest[0], shared.ProjectID(root))
		if err != nil {
			return 0, err
		}
		writeConnection(io, doc, flags.JSON, flags.Reveal)
		return 0, nil
	case "attach":
		id, err := parseSharedID(firstOrNil(rest))
		if err != nil {
			return 0, err
		}
		// Everything after the id is forwarded verbatim to the recipe's
		// provision argv — flags for this command must come before the id.
		attachArgs := append([]string{}, rest[1:]...)
		c, err := client.EnsureSMP(ctx, spawnSMP)
		if err != nil {
			return 0, err
		}
		result, err := client.SharedAttach(ctx, c, id, root, attachArgs)
		if err != nil {
			return 0, failErr(ExitFailed, err.Error())
		}
		if flags.JSON {
			out(io, prettyJSON(jsonValue(result)))
		} else {
			service, _ := result["service"].(string)
			if service == "" {
				service = id
			}
			out(io, "attached "+service)
			if attachment, ok := result["attachment"].(map[string]any); ok {
				if conn, ok := attachment["connection"].(map[string]any); ok {
					for k, v := range conn {
						text, ok := v.(string)
						if !ok {
							text = fmt.Sprintf("%v", v)
						}
						out(io, fmt.Sprintf("  %s: %s", k, text))
					}
				}
			}
		}
		return 0, nil
	case "detach":
		id, err := parseSharedID(firstOrNil(rest))
		if err != nil {
			return 0, err
		}
		if flags.StopIfUnused {
			if err := requireYes(io, flags.Yes, "stopping a shared instance when unused needs confirmation"); err != nil {
				return 0, err
			}
		}
		c := liveSMPClient(client.DiscoverSMP(ctx))
		if c == nil {
			// smp down means the probe already reports not-ready — detach is a
			// no-op.
			return 0, nil
		}
		result, err := client.SharedDetach(ctx, c, id, root, flags.StopIfUnused)
		if err != nil {
			return 0, failErr(ExitFailed, err.Error())
		}
		if stopped, _ := result["stopped"].(bool); stopped {
			out(io, fmt.Sprintf("detached %s; stopped it (no project is attached)", id))
		} else {
			out(io, "detached "+id)
		}
		return 0, nil
	// The readiness probe the generated `shared:` service polls: exit 0 iff the
	// instance is ready AND this project is attached. Silent — its output would
	// land in probe noise.
	case "probe":
		id, err := parseSharedID(firstOrNil(rest))
		if err != nil {
			return 0, err
		}
		c := liveSMPClient(client.DiscoverSMP(ctx))
		if c == nil {
			return 1, nil
		}
		if client.SharedProbe(ctx, c, id, root) {
			return 0, nil
		}
		return 1, nil
	case "install":
		id, err := parseSharedID(firstOrNil(rest))
		if err != nil {
			return 0, err
		}
		c, err := client.EnsureSMP(ctx, spawnSMP)
		if err != nil {
			return 0, err
		}
		result, err := client.SharedInstall(ctx, c, id)
		if err != nil {
			return 0, failErr(ExitFailed, err.Error())
		}
		service, _ := result["service"].(string)
		if service == "" {
			service = id
		}
		out(io, fmt.Sprintf("installed %s (port %v)", service, result["port"]))
		return 0, nil
	case "start":
		id, err := parseSharedID(firstOrNil(rest))
		if err != nil {
			return 0, err
		}
		c, err := client.EnsureSMP(ctx, spawnSMP)
		if err != nil {
			return 0, err
		}
		// install first (the service only enters smp's catalog once
		// registered), then drive a normal service start through the operations
		// API.
		if _, err := client.SharedInstall(ctx, c, id); err != nil {
			return 0, failErr(ExitFailed, err.Error())
		}
		return runOperation(ctx, c, state.OpStart, id, io, flags.JSON)
	case "stop":
		id, err := parseSharedID(firstOrNil(rest))
		if err != nil {
			return 0, err
		}
		c := liveSMPClient(client.DiscoverSMP(ctx))
		if c == nil {
			return 0, failErr(ExitUnavailable, "smp is not running")
		}
		// A failed stop must fail the command: the instance may still be running.
		return runOperation(ctx, c, state.OpStop, id, io, flags.JSON)
	case "remove":
		id, err := parseSharedID(firstOrNil(rest))
		if err != nil {
			return 0, err
		}
		if err := requireYes(io, flags.Yes, "removing a shared instance needs confirmation"); err != nil {
			return 0, err
		}
		c := liveSMPClient(client.DiscoverSMP(ctx))
		if c == nil {
			return 0, failErr(ExitUnavailable, "smp is not running")
		}
		// The server refuses (409 shared_service_attached) an instance that
		// still has project attachments unless the caller explicitly confirms
		// the data wipe with force — its error message already says to retry
		// with force.
		if _, err := client.SharedRemove(ctx, c, id, flags.Force); err != nil {
			return 0, failErr(ExitFailed, err.Error())
		}
		out(io, "removed "+id)
		return 0, nil
	default:
		return 0, usageErr("unknown shared subcommand: " + subcommand)
	}
}

// sharedInstanceMatches is the skill script's lookup: the instance id, the
// service name, or a prefix of name@version.
func sharedInstanceMatches(id, svc string) bool {
	if id == svc {
		return true
	}
	name := id
	if index := strings.Index(id, "@"); index >= 0 {
		name = id[:index]
	}
	return name == svc || strings.HasPrefix(svc, name+"@")
}

func selectConnection(body map[string]any, svc, projectID string) (*omap.OMap, error) {
	var chosen map[string]any
	chosenID := ""
	for _, entry := range omap.AsArray(body["instances"]) {
		instance, ok := entry.(map[string]any)
		if !ok {
			continue
		}
		id, _ := instance["id"].(string)
		if sharedInstanceMatches(id, svc) {
			chosen = instance
			chosenID = id
			break
		}
	}
	if chosen == nil {
		return nil, failErr(ExitUnavailable, svc+" is not a registered shared instance")
	}
	var attachment map[string]any
	for _, entry := range omap.AsArray(chosen["attachments"]) {
		item, ok := entry.(map[string]any)
		if !ok {
			continue
		}
		pid, _ := item["projectId"].(string)
		if pid == "" {
			pid, _ = item["project_id"].(string)
		}
		if pid == projectID {
			attachment = item
			break
		}
	}
	if attachment == nil {
		return nil, failErrHint(ExitUnavailable, "this project has not attached "+chosenID+" — start the service first", "start the service first")
	}
	provisioned, _ := attachment["provisioned"].(bool)
	if !provisioned {
		return nil, failErr(ExitUnavailable, chosenID+" is attached but not yet provisioned")
	}
	doc := omap.New()
	doc.Set("service", chosenID)
	doc.Set("projectId", projectID)
	doc.Set("connection", connectionValue(attachment["connection"]))
	return doc, nil
}

func connectionValue(raw any) any {
	object, ok := raw.(map[string]any)
	if !ok {
		return jsonValue(raw)
	}
	out := omap.New()
	if url, ok := object["url"]; ok {
		out.Set("url", url)
	}
	if env, ok := object["env"]; ok {
		out.Set("env", jsonValue(env))
	}
	for key, value := range object {
		if key == "url" || key == "env" {
			continue
		}
		out.Set(key, jsonValue(value))
	}
	return out
}

func writeConnection(io *Io, doc *omap.OMap, jsonMode, reveal bool) {
	value := jsonValue(doc)
	if !reveal {
		value = maskOrdered(value)
	}
	if jsonMode {
		out(io, compactJSON(value))
		return
	}
	conn := omap.AsObject(value)
	var connection *omap.OMap
	if conn != nil {
		if raw, ok := conn.Get("connection"); ok {
			connection = omap.AsObject(raw)
		}
	}
	url := ""
	env := map[string]string{}
	if connection != nil {
		if raw, ok := connection.Get("url"); ok {
			if text, ok := raw.(string); ok {
				url = text
			}
		}
		if raw, ok := connection.Get("env"); ok {
			if object := omap.AsObject(raw); object != nil {
				object.Each(func(key string, entry any) {
					if text, ok := entry.(string); ok {
						env[key] = text
					}
				})
			}
		}
	}
	if url == "" && len(env) == 0 {
		out(io, prettyJSON(value))
		return
	}
	if url != "" {
		out(io, url)
	}
	keys := make([]string, 0, len(env))
	for key := range env {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	for _, key := range keys {
		out(io, key+"="+env[key])
	}
}

// runOperation submits one operation to smp, waits for it, prints its outcome,
// and fails when it failed.
func runOperation(ctx context.Context, c *client.Client, action state.ServiceOperationKind, id string, io *Io, jsonMode bool) (int, error) {
	accepted, err := c.Submit(ctx, action, id, false, client.NewRequestID())
	if err != nil {
		return 0, failErr(ExitFailed, err.Error())
	}
	operation, err := WaitOperation(ctx, c, accepted.ID)
	if err != nil {
		return 0, err
	}
	if operation.Status == state.OpStatusFailed {
		if !jsonMode {
			out(io, fmt.Sprintf("%s %s %s", operation.Status, id, operation.ID))
		}
		return 0, failErr(ExitFailed, "service operation failed")
	}
	out(io, fmt.Sprintf("%s %s %s", operation.Status, id, operation.ID))
	return 0, nil
}

// sortSharedStatusInstances orders instances by name, then version. String
// order, not semver. A row with no name falls back to splitting id on the
// first '@'.
func sortSharedStatusInstances(body map[string]any) {
	raw := omap.AsArray(body["instances"])
	if len(raw) < 2 {
		return
	}
	sort.SliceStable(raw, func(i, j int) bool {
		leftName, leftVersion := sharedInstanceSortKey(raw[i])
		rightName, rightVersion := sharedInstanceSortKey(raw[j])
		if leftName != rightName {
			return leftName < rightName
		}
		return leftVersion < rightVersion
	})
	body["instances"] = raw
}

func sharedInstanceSortKey(entry any) (string, string) {
	instance, ok := entry.(map[string]any)
	if !ok {
		return "", ""
	}
	name, _ := instance["name"].(string)
	version, _ := instance["version"].(string)
	if name != "" || version != "" {
		return name, version
	}
	id, _ := instance["id"].(string)
	if index := strings.Index(id, "@"); index >= 0 {
		return id[:index], id[index+1:]
	}
	return id, ""
}

// envOrNil reads an environment variable, returning nil when it is unset.
func envOrNil(name string) *string {
	value, ok := os.LookupEnv(name)
	if !ok {
		return nil
	}
	return &value
}
