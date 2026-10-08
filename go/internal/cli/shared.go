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
	"github.com/ngosangns/hearth/go/internal/shared"
	"github.com/ngosangns/hearth/go/internal/state"
)

// RunShared runs one `hearth shared …` command and returns the process exit
// code. spawnSMP starts a detached `hearth smp` when none is running.
func RunShared(ctx context.Context, root string, argv []string, io *Io, spawnSMP func(root string)) int {
	code, err := runSharedInner(ctx, root, argv, io, spawnSMP)
	if err != nil {
		errOut(io, err.Error())
		return exitCodeOf(err)
	}
	return code
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
	parsed, passthrough := splitPassthrough(args)
	flags, err := ParseCommandFlags(parsed, []Flag{FlagJSON, FlagForce, FlagStopIfUnused})
	if err != nil {
		return 0, err
	}
	if len(flags.Positionals) == 0 {
		return 0, usageErr("usage: hearth shared ensure|list|installed|status|attach|detach|probe|install|start|stop|remove [--json] [--force] [<name>@<version>] [attach-args...]")
	}
	subcommand := flags.Positionals[0]
	if flags.Force && subcommand != "remove" {
		return 0, usageErr("--force is only supported by `shared remove`")
	}
	if flags.StopIfUnused && subcommand != "detach" {
		return 0, usageErr("--stop-if-unused is only supported by `shared detach`")
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
		if flags.JSON {
			payload := map[string]any{"instances": instances}
			out(io, prettyJSON(jsonValue(payload)))
		} else {
			for _, instance := range instances {
				out(io, fmt.Sprintf("%s  port %d  %s  (%d project(s) attached)",
					instance.ID(), instance.Port, string(instance.InstallState), len(instance.Attachments)))
			}
		}
		return 0, nil
	case "status":
		c := liveSMPClient(client.DiscoverSMP(ctx))
		if c == nil {
			errOut(io, "smp is not running")
			return ExitUnavailable, nil
		}
		body, err := client.SMPRequest(ctx, c, "/v1/shared", "GET", nil)
		if err != nil {
			return 0, failErr(ExitUnavailable, err.Error())
		}
		if flags.JSON {
			out(io, prettyJSON(jsonValue(body)))
		} else {
			for _, entry := range asArray(body["instances"]) {
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
		return runOperation(ctx, c, state.OpStart, id, io)
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
		return runOperation(ctx, c, state.OpStop, id, io)
	case "remove":
		id, err := parseSharedID(firstOrNil(rest))
		if err != nil {
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

// runOperation submits one operation to smp, waits for it, prints its outcome,
// and fails when it failed.
func runOperation(ctx context.Context, c *client.Client, action state.ServiceOperationKind, id string, io *Io) (int, error) {
	accepted, err := c.Submit(ctx, action, id, false, client.NewRequestID())
	if err != nil {
		return 0, failErr(ExitFailed, err.Error())
	}
	operation, err := WaitOperation(ctx, c, accepted.ID)
	if err != nil {
		return 0, err
	}
	out(io, fmt.Sprintf("%s %s %s", operation.Status, id, operation.ID))
	if operation.Status == state.OpStatusFailed {
		return 0, failErr(ExitFailed, "service operation failed")
	}
	return 0, nil
}

// envOrNil reads an environment variable, returning nil when it is unset.
func envOrNil(name string) *string {
	value, ok := os.LookupEnv(name)
	if !ok {
		return nil
	}
	return &value
}

// asArray returns value as a slice, or nil.
func asArray(value any) []any {
	array, _ := value.([]any)
	return array
}
