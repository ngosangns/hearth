// `hearth`: the daemon, the CLI, and the MCP server in one binary.
// `hearth daemon` and `hearth smp` run a manager in the foreground (and are what
// `ensure` spawns detached); bare `mcp` is intercepted here because internal/cli
// cannot depend on internal/mcpserver; every other subcommand delegates to
// cli.Main. Ported from rust/bin/hearth/src/main.rs — the TUI is gone, so a bare
// `hearth` always prints help and `tui` is an unknown command.
package main

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"syscall"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/cli"
	"github.com/ngosangns/hearth/go/internal/configfile"
	"github.com/ngosangns/hearth/go/internal/daemon"
	"github.com/ngosangns/hearth/go/internal/env"
	"github.com/ngosangns/hearth/go/internal/manager"
	"github.com/ngosangns/hearth/go/internal/mcpserver"
	"github.com/ngosangns/hearth/go/internal/paths"
	"github.com/ngosangns/hearth/go/internal/shared"
	"github.com/ngosangns/hearth/go/internal/supervisor"
	"github.com/ngosangns/hearth/go/internal/updater"
)

// version is stamped at build time: -ldflags "-X main.version=$(cat VERSION)".
// The repo-root VERSION file is the canonical version — `task install`, the
// release workflows, and `hearth update` (which compares it against GitHub
// release tags) all read from it.
var version = "0.0.0-dev"

func reportConfigError(root string, errors []string) {
	fmt.Fprintf(os.Stderr, "hearth: could not load a service catalog for %s\n", root)
	for _, e := range errors {
		fmt.Fprintf(os.Stderr, "  - %s\n", e)
	}
}

// Runs one manager in the foreground until it shuts down. A bootstrap failure
// exits non-zero: the daemon is spawned by `Ensure()`, and exiting 0 after
// failing to start would make a broken daemon indistinguishable from a healthy
// one.
func runManager(root, runtimeDirectory string, cat catalog.ServiceCatalog, sharedCtx *manager.SharedContext) int {
	baseEnvironment := env.ResolveBaseEnvironment(env.BaseEnvironmentOptions{Root: root})
	sup := supervisor.DefaultSupervisorOptions(root, runtimeDirectory, baseEnvironment)
	outcome := daemon.RunDaemon(manager.HearthManagerOptions{
		RuntimeDirectory: &runtimeDirectory,
		Root:             &root,
		Catalog:          cat,
		Supervisor:       &sup,
		Shared:           sharedCtx,
	}, manager.LeaveServices)
	switch outcome.Kind {
	case daemon.DaemonOutcomeStopped:
		return 0
	case daemon.DaemonOutcomeAlreadyRunning:
		fmt.Fprintf(os.Stderr,
			"hearth: a daemon for %s is already running (pid %d, port %d); not starting another\n",
			root, outcome.Pid, outcome.Port)
		return 3
	default:
		return 1
	}
}

func runDaemonSubcommand(root string, rest []string) int {
	if len(rest) != 0 {
		fmt.Fprintln(os.Stderr, "usage: hearth daemon --root <path>")
		return 2
	}
	loaded, err := configfile.LoadCatalog(root)
	if err != nil {
		if le, ok := err.(*configfile.LoadError); ok {
			reportConfigError(root, le.Errors)
		} else {
			reportConfigError(root, []string{err.Error()})
		}
		return 1
	}
	runtimeDirectory := paths.ResolveRuntimeDirectory(root, catalogRuntimeDir(loaded.Catalog))
	return runManager(root, runtimeDirectory, *loaded.Catalog, nil)
}

func catalogRuntimeDir(cat *catalog.ServiceCatalog) string {
	if cat != nil && cat.RuntimeDirectory != nil {
		return *cat.RuntimeDirectory
	}
	return ""
}

// The machine-global shared-services manager, rooted at `~/.hearth/shared`,
// whose catalog is synthesized from `registry.json` rather than a `hearth.yaml`.
// Everything else (lock, token, state.json, HTTP+SSE surface, `ensure`/
// `discover`) is identical to a project daemon.
func runSmpSubcommand(rest []string) int {
	if len(rest) != 0 {
		fmt.Fprintln(os.Stderr, "usage: hearth smp")
		return 2
	}
	root := shared.SharedRoot()
	// Dev/test escape hatch: point smp at a local registry file instead of the
	// pinned GitHub URL.
	var catalogURL *string
	if v := os.Getenv("HEARTH_SHARED_CATALOG_URL"); v != "" {
		catalogURL = &v
	}
	ctx, err := manager.NewSharedContext(root, catalogURL)
	if err != nil {
		fmt.Fprintf(os.Stderr, "hearth smp: cannot open %s: %v\n", root, err)
		return 1
	}
	instances := ctx.Registry.List()
	cat, err := shared.SynthesizeCatalog(root, instances)
	if err != nil {
		fmt.Fprintf(os.Stderr, "hearth smp: cannot synthesize catalog: %v\n", err)
		return 1
	}
	return runManager(root, ctx.RuntimeDirectory(), *cat, ctx)
}

// Re-invokes this binary with args, detached: its own process group, stdio
// discarded, never waited on — the child outlives this process.
func spawnDetached(args []string, cwd string) {
	exe, err := os.Executable()
	if err != nil {
		exe = "hearth"
	}
	command := exec.Command(exe, args...)
	command.Stdin = nil
	command.Stdout = nil
	command.Stderr = nil
	if cwd != "" {
		command.Dir = cwd
	}
	command.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	// Setpgid does not reparent. Not waiting on the child leaves it a zombie of
	// this process when the daemon exits, and `kill(pid, 0)` is still true for
	// that zombie — `manager restart` would then wait out the whole stop
	// timeout on "restarting daemon…". Reap it in the background.
	if err := command.Start(); err == nil {
		go func() { _ = command.Wait() }()
	}
}

// `hearth daemon --root <root>`, detached.
func spawnDaemon(root string) {
	spawnDetached([]string{"daemon", "--root", root}, root)
}

// `hearth smp`, detached. smp has one fixed root, so the requested one is
// ignored.
func spawnSMP(_root string) {
	spawnDetached([]string{"smp"}, "")
}

// Runs an MCP server over stdio for root's catalog. RequireConfirm stays at its
// safe default (true); ToolPrefix is fixed at `local_services_`, the name
// consumers' skill docs use.
func runMcpSubcommand(root string, cat catalog.ServiceCatalog) int {
	known := make([]string, 0, len(cat.Services))
	for _, s := range cat.Services {
		known = append(known, s.ID)
	}
	apiClient := mcpserver.NewManagerApiClient(root, mcpserver.ClientOptions{
		Catalog:     cat,
		SpawnDaemon: spawnDaemon,
	})
	opts := mcpserver.DefaultOptions()
	opts.ToolPrefix = "local_services_"
	opts.KnownServiceIDs = known
	server := mcpserver.New(apiClient, opts)
	if err := server.Serve(context.Background(), os.Stdin, os.Stdout); err != nil {
		fmt.Fprintf(os.Stderr, "hearth mcp: %v\n", err)
		return 1
	}
	return 0
}

// `--help`/`--version` must work from ANY directory. `shared` and `update` also
// run without a project catalog. Answering "how do I use this?" with "there is
// no config file here" is a bad first experience for someone who just installed
// the binary.
func printHelp() {
	fmt.Println(`hearth — local dev services daemon, CLI, and MCP server

usage: hearth [--root <path>] <command> [options]

  status [target] [--json]              current state of one service, a group, or all
  start|stop|restart <target> [--wait]  lifecycle actions
  logs <service> [--tail N] [--follow]  read a service's log
  urls [target] [--json]                where each service can be reached (live URLs)
  operation get|watch <id> [--json]     inspect one operation
  doctor [--json]                       environment and catalog diagnostics
  cleanup                               remove stale runtime state
  manager ensure|status|stop|restart|reload
                                        daemon lifecycle (restart keeps services running)
  daemon --root <path>                  run the daemon in the foreground (spawned internally)
  smp                                   run the machine-global shared-services daemon
  shared list|installed|status          inspect the shared service registry and smp
  shared attach|detach|probe <id>       attach this project to a shared service (used by hearth.yaml ` + "`shared:`" + `)
  shared install|start|stop|remove <id> manage a shared service instance
  update [--check] [--json] [--force]   install the latest stable GitHub release
  mcp                                   serve the MCP tool surface over stdio
  mcp install [--name N] [--key K] <config-file>...
                                        register this binary in an MCP client config
  skill install --dest <skill-dir>      install SKILL.md + scripts (MCP retired for agents)

shared and update do not need a hearth.yaml in the current directory.
The macOS app is the workspace once it is installed.
mcp install records this process path. A binary inside Hearth.app makes those configs point into the bundle.
Every other command resolves a catalog from --root (default: cwd):
hearth.yaml, .yml, or .json.`)
}

func stdinIsTerminal() bool {
	info, err := os.Stdin.Stat()
	return err == nil && info.Mode()&os.ModeCharDevice != 0
}

func runCLI(root string, rest []string) int {
	if len(rest) > 0 {
		switch rest[0] {
		case "--help", "-h", "help":
			printHelp()
			return 0
		case "--version", "-V":
			fmt.Printf("hearth %s\n", version)
			return 0
		}
	}
	// No TUI: a bare `hearth` prints help whether or not it's on a terminal.
	if len(rest) == 0 {
		printHelp()
		return 0
	}
	// `shared` manages the machine-global smp daemon — it deliberately does NOT
	// require a project `hearth.yaml` (the project root, when a command needs one
	// for attach/probe identity, is just the cwd).
	if rest[0] == "shared" {
		return cli.RunShared(context.Background(), root, rest[1:], &cli.Io{
			Out: func(s string) { fmt.Println(s) },
			Err: func(s string) { fmt.Fprintln(os.Stderr, s) },
		}, spawnSMP)
	}
	// Self-update talks to GitHub, not to a project daemon, and must work in a
	// directory that has no hearth.yaml.
	if rest[0] == "update" {
		err := updater.Run(context.Background(), updater.Options{
			Args:    rest[1:],
			Version: version,
			Stdout:  os.Stdout,
			Stderr:  os.Stderr,
		})
		if err == nil {
			return 0
		}
		if _, ok := err.(*updater.ExitError); ok {
			// Message already written to Stderr by Run.
			return updater.ExitCode(err)
		}
		fmt.Fprintln(os.Stderr, err)
		return updater.ExitCode(err)
	}
	loaded, err := configfile.LoadCatalog(root)
	if err != nil {
		if le, ok := err.(*configfile.LoadError); ok {
			reportConfigError(root, le.Errors)
		} else {
			reportConfigError(root, []string{err.Error()})
		}
		return 1
	}
	// Only the bare `mcp` (serve over stdio) needs the MCP server; `mcp install`
	// is plain file editing and falls through to cli.Main.
	if rest[0] == "mcp" && len(rest) == 1 {
		return runMcpSubcommand(root, *loaded.Catalog)
	}
	options := &cli.Options{
		Catalog:     *loaded.Catalog,
		SpawnDaemon: spawnDaemon,
	}
	io := &cli.Io{
		Out: func(s string) { fmt.Println(s) },
		Err: func(s string) { fmt.Fprintln(os.Stderr, s) },
	}
	// The port-conflict "kill the holder?" prompt is wired only on an interactive
	// stdin. Scripts, agents and piped calls get no prompt at all (Confirm nil),
	// so they never kill an unowned process and a plain start still goes through.
	if stdinIsTerminal() {
		io.Confirm = func(prompt string) bool {
			fmt.Fprint(os.Stderr, prompt)
			var line string
			if _, err := fmt.Fscanln(os.Stdin, &line); err != nil {
				return false
			}
			switch strings.ToLower(strings.TrimSpace(line)) {
			case "y", "yes":
				return true
			}
			return false
		}
	}
	return cli.Main(context.Background(), options, root, rest, io)
}

func main() {
	// Before anything else spawns a helper. A GUI-spawned `hearth` (and the
	// daemon it spawns) inherits launchd's bare PATH, under which `docker`,
	// `tailscale` and `bun` cannot be found — see env.WithKnownToolDirectories.
	os.Setenv("PATH", env.WithKnownToolDirectories(os.Getenv("PATH"), os.Getenv("HOME")))

	argv := os.Args[1:]
	// `smp` has its own fixed root, so it is dispatched before --root is parsed.
	if len(argv) > 0 && argv[0] == "smp" {
		os.Exit(runSmpSubcommand(argv[1:]))
	}
	// `hearth daemon --root <path>` is the spawned form; `--root <path> daemon`
	// works too.
	var daemonMode bool
	if len(argv) > 0 && argv[0] == "daemon" {
		argv = argv[1:]
		daemonMode = true
	}
	root, rest, err := cli.ParseRoot(argv)
	if err != nil {
		if le, ok := err.(*cli.LocalctlError); ok {
			fmt.Fprintln(os.Stderr, le.Message)
			os.Exit(le.ExitCode)
		}
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	var code int
	switch {
	case daemonMode:
		code = runDaemonSubcommand(root, rest)
	case len(rest) > 0 && rest[0] == "daemon":
		code = runDaemonSubcommand(root, rest[1:])
	default:
		code = runCLI(root, rest)
	}
	// A Go exit runs no destructors — any helper whose cleanup mattered owns a
	// context or a signal path (daemon.go wires SIGINT/SIGTERM itself), so a
	// plain os.Exit here is correct.
	os.Exit(code)
}
