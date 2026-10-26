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
	"strings"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/cli"
	"github.com/ngosangns/hearth/go/internal/configfile"
	"github.com/ngosangns/hearth/go/internal/daemon"
	"github.com/ngosangns/hearth/go/internal/env"
	"github.com/ngosangns/hearth/go/internal/manager"
	"github.com/ngosangns/hearth/go/internal/mcpserver"
	"github.com/ngosangns/hearth/go/internal/paths"
	"github.com/ngosangns/hearth/go/internal/platform"
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

func stdio() *cli.Io {
	return &cli.Io{
		Out: func(s string) { fmt.Println(s) },
		Err: func(s string) { fmt.Fprintln(os.Stderr, s) },
	}
}

func catalogFailed(root string, args []string, err error) int {
	errs := []string{err.Error()}
	if le, ok := err.(*configfile.LoadError); ok {
		errs = le.Errors
	}
	if cli.JSONRequested(args) {
		message := fmt.Sprintf("could not load a service catalog for %s: %s", root, strings.Join(errs, "; "))
		fmt.Println(cli.JSONError(1, message, ""))
		return 1
	}
	reportConfigError(root, errs)
	return 1
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
	if text, ok := cli.HelpFor(append([]string{"daemon"}, rest...)); ok {
		fmt.Println(text)
		return 0
	}
	if len(rest) != 0 {
		return cli.ReportFailure(stdio(), rest, &cli.LocalctlError{ExitCode: cli.ExitUsage, Message: "usage: hearth daemon --root <path>"})
	}
	loaded, err := configfile.LoadCatalog(root)
	if err != nil {
		return catalogFailed(root, rest, err)
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
	if text, ok := cli.HelpFor(append([]string{"smp"}, rest...)); ok {
		fmt.Println(text)
		return 0
	}
	if len(rest) != 0 {
		return cli.ReportFailure(stdio(), rest, &cli.LocalctlError{ExitCode: cli.ExitUsage, Message: "usage: hearth smp"})
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
// discarded, never waited on — the child outlives this process. On Unix the
// first hop is `_spawn1`, which starts `_spawn2` and exits so the long-lived
// daemon is not in the same macOS coalition as a GUI client.
func spawnDetached(args []string, cwd string) {
	exe := platform.Executable()
	hop := append([]string{"_spawn1"}, args...)
	_ = platform.SpawnDetached(exe, hop, cwd)
}

// `_spawn1`: start `_spawn2` with the real daemon argv and exit at once.
func runSpawn1(payload []string) int {
	exe := platform.Executable()
	leaf := append([]string{"_spawn2"}, payload...)
	if err := platform.SpawnDetached(exe, leaf, ""); err != nil {
		return 1
	}
	return 0
}

// `_spawn2`: run the manager in the foreground (this process becomes the daemon).
func runSpawn2(payload []string) int {
	if len(payload) > 0 && payload[0] == "smp" {
		return runSmpSubcommand(payload[1:])
	}
	if len(payload) == 0 || payload[0] != "daemon" {
		return cli.ReportFailure(stdio(), payload, &cli.LocalctlError{
			ExitCode: cli.ExitUsage,
			Message:  "usage: hearth _spawn2 daemon --root <path> | hearth _spawn2 smp",
		})
	}
	root, rest, err := cli.ParseRoot(payload[1:])
	if err != nil {
		if _, ok := err.(*cli.LocalctlError); ok {
			return cli.ReportFailure(stdio(), payload, err)
		}
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	return runDaemonSubcommand(root, rest)
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

// spawnProjectDaemon and spawnSharedDaemon are what the CLI is given. Tests
// replace them so a confirm-gate regression cannot start a daemon.
var spawnProjectDaemon = spawnDaemon
var spawnSharedDaemon = spawnSMP

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
	fmt.Println(cli.HelpText())
}

// stdinIsTerminal reports whether stdin is a real terminal. The exit-6 --yes
// gates and the interactive confirm both follow this: a character device such
// as /dev/null is not a terminal.
func stdinIsTerminal() bool {
	return isTerminal(os.Stdin)
}

// commandIO is one user-facing CLI surface. Confirm is wired only for a real
// terminal. requireYes treats a non-nil Confirm as that same decision, so a
// non-interactive stdin keeps the exit-6 gates closed and gets no prompt.
func commandIO() *cli.Io {
	io := &cli.Io{
		Out: func(s string) { fmt.Println(s) },
		Err: func(s string) { fmt.Fprintln(os.Stderr, s) },
	}
	if stdinIsTerminal() {
		io.Confirm = confirmStdin
	}
	return io
}

func confirmStdin(prompt string) bool {
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

func runCLI(root string, rest []string) int {
	if len(rest) > 0 {
		switch rest[0] {
		case "--version", "-V":
			fmt.Printf("hearth %s\n", version)
			return 0
		}
	}
	// No TUI: a bare `hearth` prints help whether or not it's on a terminal.
	// --help on any command exits 0 before a catalog is loaded.
	if len(rest) == 0 {
		printHelp()
		return 0
	}
	if text, ok := cli.HelpFor(rest); ok {
		fmt.Println(text)
		return 0
	}
	// `shared` manages the machine-global smp daemon — it deliberately does NOT
	// require a project `hearth.yaml` (the project root, when a command needs one
	// for attach/probe identity, is just the cwd).
	if rest[0] == "shared" {
		return cli.RunShared(context.Background(), root, rest[1:], commandIO(), spawnSharedDaemon)
	}
	// Self-update talks to GitHub, not to a project daemon, and must work in a
	// directory that has no hearth.yaml. version, schema, and skill are the
	// same: they describe or install the binary, not a project catalog.
	if rest[0] == "version" || rest[0] == "schema" || rest[0] == "skill" || rest[0] == "workspaces" {
		return cli.Main(context.Background(), &cli.Options{}, root, rest, commandIO())
	}
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
		return catalogFailed(root, rest, err)
	}
	// Only the bare `mcp` (serve over stdio) needs the MCP server; `mcp install`
	// is plain file editing and falls through to cli.Main.
	if rest[0] == "mcp" && len(rest) == 1 {
		return runMcpSubcommand(root, *loaded.Catalog)
	}
	options := &cli.Options{
		Catalog:     *loaded.Catalog,
		SpawnDaemon: spawnProjectDaemon,
	}
	// commandIO wires the port-conflict prompt and the exit-6 gates from the
	// same terminal check. A non-terminal stdin leaves Confirm nil, so a plain
	// start still goes through and nothing is killed without --yes.
	return cli.Main(context.Background(), options, root, rest, commandIO())
}

func main() {
	cli.Version = version
	// Before anything else spawns a helper. A GUI-spawned `hearth` (and the
	// daemon it spawns) inherits launchd's bare PATH, under which `docker`,
	// `tailscale` and `bun` cannot be found — see env.WithKnownToolDirectories.
	os.Setenv("PATH", env.WithKnownToolDirectories(os.Getenv("PATH"), os.Getenv("HOME")))

	argv := os.Args[1:]
	if len(argv) > 0 && argv[0] == "_spawn1" {
		os.Exit(runSpawn1(argv[1:]))
	}
	if len(argv) > 0 && argv[0] == "_spawn2" {
		os.Exit(runSpawn2(argv[1:]))
	}
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
		if _, ok := err.(*cli.LocalctlError); ok {
			os.Exit(cli.ReportFailure(stdio(), argv, err))
		}
		if cli.JSONRequested(argv) {
			fmt.Println(cli.JSONError(1, err.Error(), ""))
			os.Exit(1)
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
