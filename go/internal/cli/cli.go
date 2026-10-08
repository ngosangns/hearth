// Package cli is the `hearth` command-line surface: a thin HTTP client for one
// daemon's loopback API, plain-text or JSON output, hand-rolled flag parsing.
// The typed per-endpoint client lives in internal/client.
//
// Ported from rust/crates/hearth-cli/src/lib.rs (the whole CLI: discovery
// handling, flag parsing, `status`/`urls`/`logs`/`cleanup`/`manager`/`start`/
// `stop`/`restart`/`operation`/`doctor`/`mcp install`/`skill install`, and the
// `main` dispatch) plus the `hearth shared …` subcommand set from
// rust/crates/hearth-cli/src/shared.rs (see shared.go).
//
// The binary calls ParseRoot once, then Main (or RunShared for the `shared`
// subcommand). Io carries the output/error sinks and the optional interactive
// confirmation prompt; a nil Confirm means non-interactive, which never
// prompts and never kills an unowned process.
package cli

import (
	"bytes"
	"context"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/client"
	"github.com/ngosangns/hearth/go/internal/doctor"
	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/manager"
	"github.com/ngosangns/hearth/go/internal/omap"
	"github.com/ngosangns/hearth/go/internal/paths"
	"github.com/ngosangns/hearth/go/internal/platform"
	"github.com/ngosangns/hearth/go/internal/state"
	"github.com/ngosangns/hearth/go/internal/supervisor"
)

// Exit codes, matching the Rust CLI's `LocalctlError.exit_code`.
const (
	ExitUsage        = 2
	ExitUnavailable  = 3
	ExitProtocol     = 4
	ExitFailed       = 5
	ExitUnauthorized = 7
)

// managerStopTimeout is how long `manager stop` waits for the daemon to finish
// stopping every service and exit.
const managerStopTimeout = 300 * time.Second

// Io is the CLI's output surface. Out/Err receive one line each; Confirm is the
// interactive confirmation prompt — it receives the question text and returns
// the user's yes/no. A nil Confirm means non-interactive (scripts, tests),
// which makes prompt-gated behavior (killing an unowned port-holder) simply
// never fire.
type Io struct {
	Out     func(string)
	Err     func(string)
	Confirm func(string) bool
}

// Options configures one CLI invocation. Catalog is the project's loaded
// catalog; SpawnDaemon starts a detached daemon for a root when none is found.
type Options struct {
	Catalog     catalog.ServiceCatalog
	SpawnDaemon func(root string)
}

// LocalctlError is the CLI's typed failure: an exit code plus a message,
// mirroring the Rust `LocalctlError`.
type LocalctlError struct {
	ExitCode int
	Message  string
}

func (e *LocalctlError) Error() string { return e.Message }

func usageErr(message string) error {
	return &LocalctlError{ExitCode: ExitUsage, Message: message}
}

func failErr(exitCode int, message string) error {
	return &LocalctlError{ExitCode: exitCode, Message: message}
}

func unavailableErr(err error) error {
	return &LocalctlError{ExitCode: ExitUnavailable, Message: err.Error()}
}

// exitCodeOf extracts the process exit code a typed error carries, defaulting
// to EXIT_FAILED for anything else.
func exitCodeOf(err error) int {
	var le *LocalctlError
	if errors.As(err, &le) {
		return le.ExitCode
	}
	var ce *client.Error
	if errors.As(err, &ce) {
		return ce.ExitCode
	}
	return ExitFailed
}

func out(io *Io, s string) {
	if io != nil && io.Out != nil {
		io.Out(s)
	}
}

func errOut(io *Io, s string) {
	if io != nil && io.Err != nil {
		io.Err(s)
	}
}

// ---------------------------------------------------------------------------------------------
// Flag parsing
// ---------------------------------------------------------------------------------------------

// Flag is one recognized flag name.
type Flag int

const (
	FlagJSON Flag = iota
	FlagWait
	FlagFollow
	FlagTail
	FlagName
	FlagDest
	FlagKey
	FlagKillUnowned
	FlagForce
	FlagStopIfUnused
)

// Flags is the parsed result of ParseCommandFlags.
type Flags struct {
	Positionals  []string
	JSON         bool
	Wait         bool
	Follow       bool
	Tail         *uint64
	Name         *string
	Dest         *string
	Key          *string
	KillUnowned  bool
	Force        bool
	StopIfUnused bool
}

func containsFlag(allowed []Flag, flag Flag) bool {
	for _, f := range allowed {
		if f == flag {
			return true
		}
	}
	return false
}

// ParseCommandFlags parses `--flag` arguments against the allowed set. A bare
// argument is a positional; an unknown, unsupported or duplicated flag is a
// usage error.
func ParseCommandFlags(arguments []string, allowed []Flag) (*Flags, error) {
	result := &Flags{}
	seen := map[string]bool{}
	index := 0
	for index < len(arguments) {
		argument := arguments[index]
		if !strings.HasPrefix(argument, "--") {
			result.Positionals = append(result.Positionals, argument)
			index++
			continue
		}
		name := argument[2:]
		var flag Flag
		var canonical string
		switch name {
		case "json":
			flag, canonical = FlagJSON, "json"
		case "wait":
			flag, canonical = FlagWait, "wait"
		case "follow":
			flag, canonical = FlagFollow, "follow"
		case "tail":
			flag, canonical = FlagTail, "tail"
		case "name":
			flag, canonical = FlagName, "name"
		case "dest":
			flag, canonical = FlagDest, "dest"
		case "key":
			flag, canonical = FlagKey, "key"
		case "kill-unowned":
			flag, canonical = FlagKillUnowned, "kill-unowned"
		case "force":
			flag, canonical = FlagForce, "force"
		case "stop-if-unused":
			flag, canonical = FlagStopIfUnused, "stop-if-unused"
		default:
			return nil, usageErr("unknown flag: " + argument)
		}
		if !containsFlag(allowed, flag) {
			return nil, usageErr("unsupported flag: --" + canonical)
		}
		if seen[canonical] {
			return nil, usageErr("duplicate flag: --" + canonical)
		}
		seen[canonical] = true
		switch flag {
		case FlagJSON:
			result.JSON = true
		case FlagWait:
			result.Wait = true
		case FlagFollow:
			result.Follow = true
		case FlagTail:
			index++
			var value *int64
			if index < len(arguments) {
				if v, err := strconv.ParseInt(arguments[index], 10, 64); err == nil {
					value = &v
				}
			}
			if value == nil || *value < 1 {
				return nil, usageErr("--tail must be a positive integer")
			}
			u := uint64(*value)
			result.Tail = &u
		case FlagName:
			index++
			if index < len(arguments) && arguments[index] != "" {
				v := arguments[index]
				result.Name = &v
			} else {
				return nil, usageErr("--name requires a value")
			}
		case FlagDest:
			index++
			if index < len(arguments) && arguments[index] != "" {
				v := arguments[index]
				result.Dest = &v
			} else {
				return nil, usageErr("--dest requires a value")
			}
		case FlagKey:
			index++
			if index < len(arguments) && arguments[index] != "" {
				v := arguments[index]
				result.Key = &v
			} else {
				return nil, usageErr("--key requires a value")
			}
		case FlagKillUnowned:
			result.KillUnowned = true
		case FlagForce:
			result.Force = true
		case FlagStopIfUnused:
			result.StopIfUnused = true
		}
		index++
	}
	return result, nil
}

// ---------------------------------------------------------------------------------------------
// Discovery / connection helpers
// ---------------------------------------------------------------------------------------------

// Ensure discovers a live daemon for root, spawning one when none is running.
func Ensure(ctx context.Context, root string, options *Options) (*client.Client, error) {
	return client.Ensure(ctx, root, &client.Options{
		Catalog:     &options.Catalog,
		SpawnDaemon: options.SpawnDaemon,
	})
}

// RequireClient returns a live, authorized daemon for root — never spawns one.
func RequireClient(ctx context.Context, root string, options *Options) (*client.Client, error) {
	return client.RequireClientFor(ctx, root, &options.Catalog)
}

// requestWithProtocol sends one request carrying the daemon's own protocol
// version (not this binary's), so an older daemon accepts it.
func requestWithProtocol(ctx context.Context, c *client.Client, path, method string, body any, protocol uint32) (map[string]any, error) {
	timeout := client.DefaultRequestTimeout
	return c.RequestWithTimeout(ctx, path, method, body, &protocol, &timeout)
}

// ---------------------------------------------------------------------------------------------
// Targets / operation ids
// ---------------------------------------------------------------------------------------------

// Targets expands a target into service ids: nil selects every service, a
// service id selects itself, a group selects its members, anything else is a
// usage error.
func Targets(cat *catalog.ServiceCatalog, target *string) ([]string, error) {
	if target == nil {
		ids := make([]string, 0, len(cat.Services))
		for _, s := range cat.Services {
			ids = append(ids, s.ID)
		}
		return ids, nil
	}
	t := *target
	for _, s := range cat.Services {
		if s.ID == t {
			return []string{t}, nil
		}
	}
	if group, ok := cat.Groups[t]; ok {
		return append([]string{}, group...), nil
	}
	return nil, usageErr("unknown service or group: " + t)
}

// RunnableTargets is Targets with the disabled/verified rules applied: a
// disabled direct target is a usage error, a disabled group member is silently
// skipped, and a service whose run profile is unresolved fails.
func RunnableTargets(cat *catalog.ServiceCatalog, target *string) ([]string, error) {
	selected, err := Targets(cat, target)
	if err != nil {
		return nil, err
	}
	if target != nil {
		for _, s := range cat.Services {
			if s.ID == *target && s.Disabled {
				return nil, usageErr("service " + *target + " is disabled")
			}
		}
	}
	filtered := make([]string, 0, len(selected))
	for _, id := range selected {
		disabled := false
		for _, s := range cat.Services {
			if s.ID == id {
				disabled = s.Disabled
				break
			}
		}
		if !disabled {
			filtered = append(filtered, id)
		}
	}
	if len(filtered) == 0 {
		return nil, usageErr("nothing to run: every selected service is disabled")
	}
	for _, id := range filtered {
		verified := false
		for _, s := range cat.Services {
			if s.ID == id {
				verified = s.Profiles.Run.IsVerified()
				break
			}
		}
		if !verified {
			return nil, failErr(ExitFailed, "unsupported service: "+id)
		}
	}
	return filtered, nil
}

// WaitOperation polls an operation until it succeeds or fails, with no deadline.
func WaitOperation(ctx context.Context, c *client.Client, id string) (*state.Operation, error) {
	return c.Wait(ctx, id, nil)
}

// ---------------------------------------------------------------------------------------------
// status / cleanup / logs
// ---------------------------------------------------------------------------------------------

func serviceRows(ctx context.Context, c *client.Client) ([]state.ServiceLifecycleState, error) {
	return c.Services(ctx)
}

// actualStateName is the daemon's own state name (`running-unready`,
// `preparing`, …). A missing row is `stopped`.
func actualStateName(s *state.ServiceLifecycleState) string {
	if s == nil {
		return "stopped"
	}
	return string(s.ActualState)
}

// statusPid is the pid `hearth status` shows: only while the recorded process
// can still be running. A stopped, queued, preparing, failed, or finished row
// keeps the last identity it had, and printing it named a process that was gone
// (or, after pid reuse, someone else's).
func statusPid(s *state.ServiceLifecycleState) *int64 {
	if s == nil {
		return nil
	}
	live := false
	switch s.ActualState {
	case state.ActualStarting, state.ActualRunning, state.ActualRunningUnready,
		state.ActualReady, state.ActualStopping, state.ActualOrphaned:
		live = true
	}
	if !live || s.Identity == nil || s.Identity.IsDocker() || s.Identity.Pid == nil {
		return nil
	}
	pid := *s.Identity.Pid
	return &pid
}

// textState is the coarse state vocabulary of `status --json`'s `state` field
// and of `urls`, kept for scripts written against it. `hearth status` prints
// actualStateName.
func textState(s *state.ServiceLifecycleState) string {
	if s == nil {
		return "stopped"
	}
	switch s.ActualState {
	case state.ActualReady:
		return "ready"
	case state.ActualSucceeded:
		return "succeeded"
	case state.ActualQueuedStart:
		return "queued-start"
	case state.ActualRunning, state.ActualRunningUnready, state.ActualStarting, state.ActualPreparing:
		return "running"
	case state.ActualStopping:
		return "stopping"
	case state.ActualFailed:
		return "failed"
	case state.ActualOrphaned:
		return "orphaned"
	case state.ActualExternallyOwned:
		return "externally-owned"
	default:
		return "stopped"
	}
}

// Cleanup removes stale runtime state for root: a lock no live daemon owns, and
// quarantined stale-lock directories whose marker proves the same ownership.
func Cleanup(ctx context.Context, root string, options *Options) error {
	guarded := true
	if options.Catalog.PrivateFileGuard != nil {
		guarded = *options.Catalog.PrivateFileGuard
	}
	ioFiles := fileio.New(guarded)
	runtimeDirectory := paths.ResolveRuntimeDirectory(root, derefString(options.Catalog.RuntimeDirectory))
	discovered := client.Discover(ctx, root, &options.Catalog)
	if discovered.Kind == client.DiscoveryLive || !ioFiles.IsPrivateDirectory(runtimeDirectory) {
		return nil
	}
	ownershipKey := manager.ReadLockOwnershipKey(ioFiles, runtimeDirectory)
	if ownershipKey == nil {
		return nil
	}
	candidates := []string{"manager.lock"}
	if entries, err := os.ReadDir(runtimeDirectory); err == nil {
		for _, entry := range entries {
			name := entry.Name()
			if strings.HasPrefix(name, "manager.lock.stale-") {
				candidates = append(candidates, name)
			}
		}
	}
	for _, entry := range candidates {
		path := filepath.Join(runtimeDirectory, entry)
		artifacts := manager.ReadOwnedLockArtifacts(ioFiles, path)
		if artifacts == nil {
			continue
		}
		if !manager.VerifyLockOwnershipProof(ownershipKey, &artifacts.Metadata, artifacts.Token, &artifacts.Proof) {
			continue
		}
		if entry == "manager.lock" {
			// A live pid — including a stale or protocol-incompatible daemon —
			// still owns this lock. Deleting it makes LockOwnershipWatch exit
			// the process that is actually running.
			if platform.IsPIDAlive(artifacts.Metadata.Pid) {
				continue
			}
			_ = fileio.RemoveDirectory(path)
			continue
		}
		markerRaw, err := ioFiles.ReadFile(filepath.Join(path, state.StaleLockMarkerName))
		if err != nil || markerRaw == nil {
			continue
		}
		var marker state.StaleLockMarker
		if json.Unmarshal([]byte(*markerRaw), &marker) != nil {
			continue
		}
		if manager.IsStaleLockMarker(&marker, *ownershipKey, &artifacts.Metadata, artifacts.Token, &artifacts.Proof) {
			_ = fileio.RemoveDirectory(path)
		}
	}
	return nil
}

// Logs streams a service's log. `--tail` trims the first snapshot only; later
// follow chunks are the new lines since the cursor, so trimming each one would
// drop everything but the last N lines of every poll.
func Logs(ctx context.Context, c *client.Client, serviceID string, tail uint64, follow, jsonOutput bool, options *Options, write, writeErr func(string)) error {
	var cursor, generation *uint64
	reconnects := 0
	first := true
	limit := uint64(16_384)
	for {
		slice, err := c.Log(ctx, serviceID, cursor, generation, &limit)
		if err == nil {
			lines := rustLines(slice.Data)
			nonEmpty := make([]string, 0, len(lines))
			for _, line := range lines {
				if line != "" {
					nonEmpty = append(nonEmpty, line)
				}
			}
			tailLines := nonEmpty
			if first && uint64(len(nonEmpty)) > tail {
				tailLines = nonEmpty[len(nonEmpty)-int(tail):]
			}
			first = false
			joined := ""
			if len(nonEmpty) > 0 {
				joined = strings.Join(tailLines, "\n") + "\n"
			}
			if jsonOutput {
				printed := omap.New()
				printed.Set("serviceId", serviceID)
				printed.Set("generation", slice.Generation)
				printed.Set("cursor", slice.Cursor)
				printed.Set("nextCursor", slice.NextCursor)
				printed.Set("data", joined)
				printed.Set("reset", slice.Reset)
				printed.Set("truncated", slice.Truncated)
				write(compactJSON(printed))
			} else {
				if slice.Reset {
					writeErr(fmt.Sprintf("log reset %s generation %d", serviceID, slice.Generation))
				}
				if joined != "" {
					write(joined)
				}
			}
			cursor = &slice.NextCursor
			generation = &slice.Generation
			reconnects = 0
			if !follow {
				return nil
			}
			if err := sleepCtx(ctx, 500*time.Millisecond); err != nil {
				return err
			}
			continue
		}
		reconnects++
		if !follow || reconnects > 3 {
			return failErr(ExitUnavailable, "log follow lost manager connection")
		}
		if err := sleepCtx(ctx, 200*time.Millisecond); err != nil {
			return err
		}
		c, err = RequireClient(ctx, c.Root, options)
		if err != nil {
			return err
		}
	}
}

// rustLines splits s the way Rust's `str::lines` does: on `\n`, stripping a
// preceding `\r`, with no trailing empty element for a final newline.
func rustLines(s string) []string {
	var out []string
	for len(s) > 0 {
		idx := strings.IndexByte(s, '\n')
		var line string
		if idx < 0 {
			line = s
			s = ""
		} else {
			line = s[:idx]
			s = s[idx+1:]
		}
		out = append(out, strings.TrimSuffix(line, "\r"))
	}
	return out
}

func sleepCtx(ctx context.Context, d time.Duration) error {
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-timer.C:
		return nil
	}
}

// ---------------------------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------------------------

// ParseRoot splits a leading `--root <path>` off argv; the root defaults to the
// current directory. The binary calls this once and hands the result to Main,
// so every subcommand agrees on the root.
func ParseRoot(argv []string) (string, []string, error) {
	if len(argv) > 0 && argv[0] == "--root" {
		if len(argv) < 2 {
			return "", nil, usageErr("--root requires a path")
		}
		return argv[1], argv[2:], nil
	}
	cwd, err := os.Getwd()
	if err != nil {
		return "", nil, failErr(ExitFailed, "cannot resolve the current directory: "+err.Error())
	}
	return cwd, argv, nil
}

// Main runs one CLI command for the project at root (already split off by
// ParseRoot) and returns the process exit code.
func Main(ctx context.Context, options *Options, root string, argv []string, io *Io) int {
	code, err := mainInner(ctx, options, root, argv, io)
	if err != nil {
		errOut(io, err.Error())
		return exitCodeOf(err)
	}
	return code
}

func mainInner(ctx context.Context, options *Options, root string, argv []string, io *Io) (int, error) {
	if len(argv) == 0 {
		return 0, usageErr("usage: hearth <command>")
	}
	command := argv[0]
	rest := argv[1:]

	switch command {
	case "doctor":
		return doctorCommand(options, rest, io)
	case "cleanup":
		flags, err := ParseCommandFlags(rest, nil)
		if err != nil {
			return 0, err
		}
		if len(flags.Positionals) > 0 {
			return 0, usageErr("cleanup takes no positional arguments")
		}
		if err := Cleanup(ctx, root, options); err != nil {
			return 0, err
		}
		return 0, nil
	case "manager":
		return managerCommand(ctx, root, options, rest, io)
	case "status":
		return statusCommand(ctx, root, options, rest, io)
	case "urls":
		return urlsCommand(ctx, root, options, rest, io)
	case "start", "stop", "restart":
		return startStopRestartCommand(ctx, root, options, command, rest, io)
	case "operation":
		return operationCommand(ctx, root, options, rest, io)
	case "logs":
		return logsCommand(ctx, root, options, rest, io)
	case "mcp":
		return mcpCommand(root, rest, io)
	case "skill":
		return skillCommand(root, rest, io)
	default:
		return 0, usageErr("unknown command: " + command)
	}
}

func doctorCommand(options *Options, rest []string, io *Io) (int, error) {
	flags, err := ParseCommandFlags(rest, []Flag{FlagJSON})
	if err != nil {
		return 0, err
	}
	if len(flags.Positionals) > 0 {
		return 0, usageErr("doctor takes no positional arguments")
	}
	checks := doctor.DefaultDoctorChecks()
	report := doctor.RunDoctor(&options.Catalog, &checks, doctor.DefaultDoctorAdapter{})
	if flags.JSON {
		out(io, printValue(reportToJSON(&report), true))
	} else {
		for _, check := range report.Checks {
			status := "missing"
			if check.OK {
				status = "ok"
			}
			out(io, fmt.Sprintf("%s %s %s", status, check.Name, check.Detail))
		}
		for _, warning := range report.UnresolvedProfiles {
			out(io, "unresolved "+warning)
		}
	}
	if !report.OK {
		return 0, failErr(ExitFailed, "doctor checks failed")
	}
	return 0, nil
}

func reportToJSON(report *doctor.DoctorReport) any {
	checks := make([]any, 0, len(report.Checks))
	for _, check := range report.Checks {
		entry := omap.New()
		entry.Set("name", check.Name)
		entry.Set("ok", check.OK)
		entry.Set("detail", check.Detail)
		checks = append(checks, entry)
	}
	profiles := make([]any, 0, len(report.UnresolvedProfiles))
	for _, profile := range report.UnresolvedProfiles {
		profiles = append(profiles, profile)
	}
	result := omap.New()
	result.Set("ok", report.OK)
	result.Set("checks", checks)
	result.Set("unresolvedProfiles", profiles)
	return result
}

func statusCommand(ctx context.Context, root string, options *Options, rest []string, io *Io) (int, error) {
	flags, err := ParseCommandFlags(rest, []Flag{FlagJSON})
	if err != nil {
		return 0, err
	}
	if len(flags.Positionals) > 1 {
		return 0, usageErr("usage: hearth status [target] [--json]")
	}
	c, err := RequireClient(ctx, root, options)
	if err != nil {
		return 0, err
	}
	rows, err := serviceRows(ctx, c)
	if err != nil {
		return 0, err
	}
	selected, err := Targets(&options.Catalog, firstOrNil(flags.Positionals))
	if err != nil {
		return 0, err
	}
	result := make([]any, 0, len(selected))
	for _, serviceID := range selected {
		row := findRow(rows, serviceID)
		// `pid` is omitted, never null, when there isn't one — consumers test
		// for the key's presence. `state` keeps its coarse vocabulary
		// (`running` covers preparing/starting/running-unready); `actualState`
		// is the daemon's.
		entry := omap.New()
		entry.Set("serviceId", serviceID)
		entry.Set("state", textState(row))
		entry.Set("actualState", actualStateName(row))
		if pid := statusPid(row); pid != nil {
			entry.Set("pid", *pid)
		}
		result = append(result, entry)
	}
	if flags.JSON {
		payload := omap.New()
		payload.Set("services", result)
		out(io, printValue(payload, true))
	} else {
		for _, entry := range result {
			obj := entry.(*omap.OMap)
			pidSuffix := ""
			if pid, ok := obj.Get("pid"); ok {
				pidSuffix = fmt.Sprintf(" pid %d", pid)
			}
			actual, _ := obj.Get("actualState")
			serviceID, _ := obj.Get("serviceId")
			out(io, fmt.Sprintf("%s %s%s", actual, serviceID, pidSuffix))
		}
	}
	return 0, nil
}

func operationCommand(ctx context.Context, root string, options *Options, rest []string, io *Io) (int, error) {
	flags, err := ParseCommandFlags(rest, []Flag{FlagJSON})
	if err != nil {
		return 0, err
	}
	if len(flags.Positionals) != 2 || (flags.Positionals[0] != "get" && flags.Positionals[0] != "watch") {
		return 0, usageErr("usage: hearth operation get|watch <operationId> [--json]")
	}
	c, err := RequireClient(ctx, root, options)
	if err != nil {
		return 0, err
	}
	var operation *state.Operation
	if flags.Positionals[0] == "watch" {
		operation, err = WaitOperation(ctx, c, flags.Positionals[1])
	} else {
		operation, err = c.Operation(ctx, flags.Positionals[1])
	}
	if err != nil {
		return 0, err
	}
	out(io, printValue(operation, flags.JSON))
	return 0, nil
}

func logsCommand(ctx context.Context, root string, options *Options, rest []string, io *Io) (int, error) {
	flags, err := ParseCommandFlags(rest, []Flag{FlagTail, FlagFollow, FlagJSON})
	if err != nil {
		return 0, err
	}
	if len(flags.Positionals) != 1 {
		return 0, usageErr("usage: hearth logs <service> [--tail N] [--follow] [--json]")
	}
	name := flags.Positionals[0]
	found := false
	for _, s := range options.Catalog.Services {
		if s.ID == name {
			found = true
			break
		}
	}
	if !found {
		return 0, usageErr("unknown service: " + name)
	}
	c, err := RequireClient(ctx, root, options)
	if err != nil {
		return 0, err
	}
	tail := uint64(200)
	if flags.Tail != nil {
		tail = *flags.Tail
	}
	err = Logs(ctx, c, name, tail, flags.Follow, flags.JSON, options,
		func(s string) { out(io, s) },
		func(s string) { errOut(io, s) })
	if err != nil {
		return 0, err
	}
	return 0, nil
}

func managerCommand(ctx context.Context, root string, options *Options, rest []string, io *Io) (int, error) {
	flags, err := ParseCommandFlags(rest, []Flag{FlagJSON})
	if err != nil {
		return 0, err
	}
	subcommand := firstOrNil(flags.Positionals)
	valid := subcommand != nil && (*subcommand == "ensure" || *subcommand == "status" ||
		*subcommand == "stop" || *subcommand == "restart" || *subcommand == "reload")
	if len(flags.Positionals) != 1 || !valid {
		return 0, usageErr("usage: hearth manager ensure|status|stop|restart|reload [--json]")
	}
	switch *subcommand {
	case "reload":
		c, err := RequireClient(ctx, root, options)
		if err != nil {
			return 0, err
		}
		body := map[string]any{
			"requestId": client.NewRequestID(),
			"catalog":   &options.Catalog,
		}
		result, err := requestWithProtocol(ctx, c, "/v1/manager/reload", "POST", body, c.Metadata.ProtocolVersion)
		if err != nil {
			return 0, unavailableErr(err)
		}
		out(io, printValue(result, flags.JSON))
		return 0, nil
	case "ensure":
		c, err := Ensure(ctx, root, options)
		if err != nil {
			return 0, err
		}
		out(io, printValue(c.EnsurePayload(), flags.JSON))
		return 0, nil
	case "restart":
		payload, err := RestartManager(ctx, root, options)
		if err != nil {
			return 0, err
		}
		out(io, printValue(payload, flags.JSON))
		return 0, nil
	case "status":
		discovered := client.Discover(ctx, root, &options.Catalog)
		if discovered.Kind == client.DiscoveryIncompatible {
			return 0, failErr(ExitProtocol, "hearth manager protocol is incompatible")
		}
		c, err := RequireClient(ctx, root, options)
		if err != nil {
			return 0, err
		}
		result, err := c.ManagerInfo(ctx)
		if err != nil {
			return 0, err
		}
		out(io, printValue(result, flags.JSON))
		return 0, nil
	default: // "stop"
		operation, err := StopManager(ctx, root, options)
		if err != nil {
			return 0, err
		}
		out(io, printValue(operation, flags.JSON))
		return 0, nil
	}
}

// RestartManager restarts the daemon for root: shuts the running one down
// without stopping its services (leave-services — daemon-owned processes are
// detached and outlive the daemon, and the next one re-adopts them from their
// persisted identities), kills any other daemon process for this same root,
// then ensures a fresh daemon. Returns the same payload `manager ensure --json`
// prints.
func RestartManager(ctx context.Context, root string, options *Options) (any, error) {
	var recordedPid *int64
	var shutdownErr error
	discovered := client.Discover(ctx, root, &options.Catalog)
	switch discovered.Kind {
	case client.DiscoveryLive, client.DiscoveryStale, client.DiscoveryIncompatible:
		pid := discovered.Client.Metadata.Pid
		recordedPid = &pid
		if err := shutDownForRestart(ctx, discovered.Client); err != nil {
			shutdownErr = err
		}
	}
	// The lock records one pid. Another `hearth daemon --root <this>` (or
	// `hearth smp` when root is the shared root) can still be alive — a
	// previous binary that never exited, or a daemon that ignored the shutdown
	// request. Kill those pids before starting a new one.
	if err := supervisor.ReapDuplicateDaemons(root); err != nil {
		return nil, failErr(ExitFailed, err.Error())
	}
	if recordedPid != nil {
		stillRunning, err := supervisor.PIDIsLive(*recordedPid)
		if err != nil {
			return nil, failErr(ExitFailed, err.Error())
		}
		if *recordedPid != int64(os.Getpid()) && stillRunning {
			if shutdownErr != nil {
				return nil, shutdownErr
			}
			return nil, failErr(ExitFailed, fmt.Sprintf("hearth manager (pid %d) is still running", *recordedPid))
		}
	}
	c, err := Ensure(ctx, root, options)
	if err != nil {
		return nil, err
	}
	return c.EnsurePayload(), nil
}

// shutDownForRestart shuts one running daemon down without stopping its
// services, and waits for the process to exit.
func shutDownForRestart(ctx context.Context, c *client.Client) error {
	pid := c.Metadata.Pid
	body := map[string]any{
		"requestId": client.NewRequestID(),
		"mode":      "leave-services",
	}
	_, err := requestWithProtocol(ctx, c, "/v1/manager/shutdown", "POST", body, c.Metadata.ProtocolVersion)
	if err != nil {
		if strings.HasPrefix(err.Error(), "invalid_shutdown_mode") {
			// A daemon from before `leave-services` existed rejects the mode
			// outright. Its own SIGTERM path is the same shutdown, so signal it
			// instead: a daemon that predates this command is precisely the one
			// `restart` exists to replace.
			if !platform.TerminatePID(pid) {
				return unavailableErr(err)
			}
		} else if !platform.IsPIDAlive(pid) {
			// A recorded daemon whose pid is already gone is nothing to
			// restart — the lock artifacts simply outlived it, and `ensure`
			// below starts a fresh one.
			return nil
		} else {
			return unavailableErr(err)
		}
	}
	return waitForDaemonExit(ctx, pid, managerStopTimeout, platform.IsPIDAlive)
}

// StopManager is the shared `manager stop` body: a `stop-services` shutdown,
// then wait for the daemon process to actually exit. `Incompatible` is
// deliberately included in discovery: shutting the old daemon down is THE
// documented recovery path after a PROTOCOL_VERSION bump.
func StopManager(ctx context.Context, root string, options *Options) (any, error) {
	discovered := client.Discover(ctx, root, &options.Catalog)
	var c *client.Client
	switch discovered.Kind {
	case client.DiscoveryLive, client.DiscoveryStale, client.DiscoveryIncompatible:
		c = discovered.Client
	default:
		var err error
		c, err = RequireClient(ctx, root, options)
		if err != nil {
			return nil, err
		}
	}
	body := map[string]any{
		"requestId": client.NewRequestID(),
		"mode":      "stop-services",
	}
	result, err := requestWithProtocol(ctx, c, "/v1/manager/shutdown", "POST", body, c.Metadata.ProtocolVersion)
	if err != nil {
		return nil, unavailableErr(err)
	}
	operation := result["operation"]
	if err := waitForDaemonExit(ctx, c.Metadata.Pid, managerStopTimeout, platform.IsPIDAlive); err != nil {
		return nil, err
	}
	return operation, nil
}

// waitForDaemonExit waits until pid is no longer alive, or fails after timeout.
// An in-process daemon (tests, embedders) shares our pid and can never be seen
// to exit, so it is not waited on.
func waitForDaemonExit(ctx context.Context, pid int64, timeout time.Duration, alive func(int64) bool) error {
	if pid == int64(os.Getpid()) {
		return nil
	}
	deadline := time.Now().Add(timeout)
	for alive(pid) {
		if !time.Now().Before(deadline) {
			return failErr(ExitFailed, fmt.Sprintf("hearth manager (pid %d) did not exit within %ds", pid, int(timeout.Seconds())))
		}
		if err := sleepCtx(ctx, 200*time.Millisecond); err != nil {
			return err
		}
	}
	return nil
}

// urlsCommand lists every registered URL of the selected services,
// placeholders resolved by the daemon. A URL that only works while its service
// runs is marked `(not running)` when the service is not up, so a dead link is
// recognisable before anyone clicks it. A finished one-shot (`succeeded`) is
// not flagged. URLs whose placeholder has no value on this machine go to stderr
// with the reason rather than vanishing.
func urlsCommand(ctx context.Context, root string, options *Options, rest []string, io *Io) (int, error) {
	flags, err := ParseCommandFlags(rest, []Flag{FlagJSON})
	if err != nil {
		return 0, err
	}
	if len(flags.Positionals) > 1 {
		return 0, usageErr("usage: hearth urls [target] [--json]")
	}
	selected, err := Targets(&options.Catalog, firstOrNil(flags.Positionals))
	if err != nil {
		return 0, err
	}
	c, err := RequireClient(ctx, root, options)
	if err != nil {
		return 0, err
	}
	body, err := c.URLs(ctx)
	if err != nil {
		return 0, unavailableErr(err)
	}
	rows, err := serviceRows(ctx, c)
	if err != nil {
		return 0, err
	}
	isRunning := func(serviceID string) bool {
		switch textState(findRow(rows, serviceID)) {
		case "ready", "running":
			return true
		}
		return false
	}
	isFinished := func(serviceID string) bool {
		return textState(findRow(rows, serviceID)) == "succeeded"
	}
	inSelection := func(serviceID string) bool {
		for _, s := range selected {
			if s == serviceID {
				return true
			}
		}
		return false
	}

	urls := make([]any, 0, len(body.URLs))
	for _, entry := range body.URLs {
		if !inSelection(entry.ServiceID) {
			continue
		}
		obj := omap.New()
		obj.Set("serviceId", entry.ServiceID)
		if entry.Label != nil {
			obj.Set("label", *entry.Label)
		}
		obj.Set("url", entry.URL)
		obj.Set("requiresRunning", entry.RequiresRunning)
		obj.Set("running", isRunning(entry.ServiceID))
		urls = append(urls, obj)
	}
	unresolved := make([]any, 0, len(body.Unresolved))
	for _, entry := range body.Unresolved {
		if !inSelection(entry.ServiceID) {
			continue
		}
		obj := omap.New()
		obj.Set("serviceId", entry.ServiceID)
		obj.Set("url", entry.URL)
		obj.Set("placeholder", entry.Placeholder)
		unresolved = append(unresolved, obj)
	}

	if flags.JSON {
		payload := omap.New()
		payload.Set("urls", urls)
		payload.Set("unresolved", unresolved)
		out(io, printValue(payload, true))
		return 0, nil
	}
	for _, entry := range urls {
		obj := entry.(*omap.OMap)
		serviceID, _ := obj.Get("serviceId")
		label, hasLabel := obj.Get("label")
		labelText := "-"
		if hasLabel {
			labelText, _ = label.(string)
		}
		url, _ := obj.Get("url")
		requiresRunning, _ := obj.Get("requiresRunning")
		running, _ := obj.Get("running")
		stale := requiresRunning != false && running == false && !isFinished(serviceID.(string))
		suffix := ""
		if stale {
			suffix = "  (not running)"
		}
		out(io, fmt.Sprintf("%s  %s  %s%s", serviceID, labelText, url, suffix))
	}
	for _, entry := range unresolved {
		obj := entry.(*omap.OMap)
		serviceID, _ := obj.Get("serviceId")
		url, _ := obj.Get("url")
		placeholder, _ := obj.Get("placeholder")
		errOut(io, fmt.Sprintf("unresolved %s %s: no value for {%s} on this machine", serviceID, url, placeholder))
	}
	return 0, nil
}

// promptKillUnowned asks whether to kill the port-holder when the row is
// `externally-owned` and the caller wired an interactive confirm. Returns
// Some(true) on yes (submit with killUnowned), Some(false) on a declined
// prompt, nil when prompting doesn't apply (not externally owned, --json, no
// confirm callback, or the flag already set it).
func promptKillUnowned(serviceID string, row *state.ServiceLifecycleState, flags *Flags, io *Io) *bool {
	if flags.JSON || flags.KillUnowned {
		return nil
	}
	if row == nil || row.ActualState != state.ActualExternallyOwned {
		return nil
	}
	reason := "its port is held by a process this manager does not own"
	if row.Error != nil {
		reason = *row.Error
	}
	if io == nil || io.Confirm == nil {
		return nil
	}
	answer := io.Confirm(fmt.Sprintf("%s: %s. Kill it and start? [y/N] ", serviceID, reason))
	return &answer
}

func startStopRestartCommand(ctx context.Context, root string, options *Options, command string, rest []string, io *Io) (int, error) {
	flags, err := ParseCommandFlags(rest, []Flag{FlagWait, FlagJSON, FlagKillUnowned})
	if err != nil {
		return 0, err
	}
	if flags.KillUnowned && command != "start" {
		return 0, usageErr("--kill-unowned only applies to `hearth start`")
	}
	if len(flags.Positionals) != 1 {
		flagHint := ""
		if command == "start" {
			flagHint = " [--kill-unowned]"
		}
		return 0, usageErr(fmt.Sprintf("usage: hearth %s <service|group> [--wait] [--json]%s", command, flagHint))
	}
	target := flags.Positionals[0]
	selected, err := RunnableTargets(&options.Catalog, &target)
	if err != nil {
		return 0, err
	}
	isSingleService := false
	for _, s := range options.Catalog.Services {
		if s.ID == target {
			isSingleService = true
			break
		}
	}
	c, err := Ensure(ctx, root, options)
	if err != nil {
		return 0, err
	}

	if command == "start" && !isSingleService {
		// An explicit `--kill-unowned` is the confirmation for every member of
		// the group; there is no per-service prompt on a bulk start.
		accepted, err := c.BulkStart(ctx, selected, flags.KillUnowned, client.NewRequestID())
		if err != nil {
			return 0, err
		}
		var operation *state.Operation
		if flags.Wait {
			operation, err = WaitOperation(ctx, c, accepted.ID)
		} else {
			operation = accepted
		}
		if err != nil {
			return 0, err
		}
		if flags.JSON {
			payload := omap.New()
			payload.Set("operation", operation)
			out(io, printValue(payload, true))
		} else {
			out(io, fmt.Sprintf("%s bulk-start %s", operation.Status, operation.ID))
		}
		if operation.Status == state.OpStatusFailed {
			return 0, operationFailure(ctx, c, []*state.Operation{operation}, target)
		}
		return 0, nil
	}

	var operations []*state.Operation
	var stateRows *[]state.ServiceLifecycleState
	action := state.OpStart
	switch command {
	case "stop":
		action = state.OpStop
	case "restart":
		action = state.OpRestart
	}
	for _, serviceID := range selected {
		killUnowned := flags.KillUnowned
		declined := false
		// Pre-submit prompt: a prior attempt already persisted
		// `externally-owned`, so the conflict is known before this start even
		// runs — works without --wait. A declined prompt still submits a plain
		// start: `externally-owned` is only re-evaluated by a start, and the
		// daemon fails it on its own if the port is still held.
		if action == state.OpStart {
			if stateRows == nil {
				if rows, err := serviceRows(ctx, c); err == nil {
					stateRows = &rows
				}
			}
			var row *state.ServiceLifecycleState
			if stateRows != nil {
				row = findRow(*stateRows, serviceID)
			}
			if answer := promptKillUnowned(serviceID, row, flags, io); answer != nil {
				if *answer {
					killUnowned = true
				} else {
					declined = true
				}
			}
		}
		var completed *state.Operation
		for {
			accepted, err := c.Submit(ctx, action, serviceID, killUnowned, client.NewRequestID())
			if err != nil {
				return 0, err
			}
			// A group has no bulk stop/restart. Waiting for each member keeps
			// stop-on-first-failure order. `--wait` is still what a single
			// service uses to block until that one operation finishes.
			if flags.Wait || !isSingleService {
				completed, err = WaitOperation(ctx, c, accepted.ID)
			} else {
				completed = accepted
			}
			if err != nil {
				return 0, err
			}
			// Post-failure prompt: a FRESH conflict only surfaces once the
			// operation completes, which needs --wait. Re-read the row — the
			// daemon just persisted `externally-owned` with the holder's
			// pid/command in `error`. Never re-asks a question already declined.
			if action != state.OpStart || !flags.Wait || killUnowned || declined ||
				completed.Status != state.OpStatusFailed {
				break
			}
			freshRows, _ := serviceRows(ctx, c)
			answer := promptKillUnowned(serviceID, findRow(freshRows, serviceID), flags, io)
			if answer != nil && *answer {
				killUnowned = true
				continue
			}
			if answer != nil && !*answer {
				out(io, "declined "+serviceID)
			}
			break
		}
		failed := completed.Status == state.OpStatusFailed
		operations = append(operations, completed)
		if failed {
			break
		}
	}
	if flags.JSON {
		payload := omap.New()
		payload.Set("operations", operations)
		out(io, printValue(payload, true))
	} else {
		for _, operation := range operations {
			serviceID := ""
			if operation.ServiceID != nil {
				serviceID = *operation.ServiceID
			}
			out(io, fmt.Sprintf("%s %s %s", operation.Status, serviceID, operation.ID))
		}
	}
	var failed []*state.Operation
	for _, operation := range operations {
		if operation.Status == state.OpStatusFailed {
			failed = append(failed, operation)
		}
	}
	if len(failed) > 0 {
		return 0, operationFailure(ctx, c, failed, target)
	}
	return 0, nil
}

// operationFailure explains why a start/stop/restart failed, instead of a bare
// "service operation failed": each failed operation's own error and, for a
// service whose port another process holds, that holder and the `--kill-unowned`
// way out.
func operationFailure(ctx context.Context, c *client.Client, failed []*state.Operation, target string) error {
	rows, _ := serviceRows(ctx, c)
	var lines []string
	portHeld := false
	for _, operation := range failed {
		var ids []string
		if operation.ServiceID != nil {
			ids = []string{*operation.ServiceID}
		} else if operation.TargetServiceIDs != nil {
			ids = *operation.TargetServiceIDs
		}
		explained := false
		for _, id := range ids {
			row := findRow(rows, id)
			if row == nil {
				continue
			}
			if row.ActualState == state.ActualExternallyOwned {
				portHeld = true
				explained = true
				reason := "its port is held by a process this manager does not own"
				if row.Error != nil {
					reason = *row.Error
				}
				lines = append(lines, fmt.Sprintf("%s: %s", id, reason))
			}
		}
		if !explained && operation.Error != nil {
			label := target
			if operation.ServiceID != nil {
				label = *operation.ServiceID
			}
			message := operation.Error.Message
			if strings.HasPrefix(message, label+":") {
				lines = append(lines, message)
			} else {
				lines = append(lines, fmt.Sprintf("%s: %s", label, message))
			}
		}
	}
	if portHeld {
		lines = append(lines, fmt.Sprintf("hint: `hearth start %s --kill-unowned` stops the process holding the port and starts the service", target))
	}
	message := "service operation failed"
	if len(lines) > 0 {
		message = strings.Join(lines, "\n")
	}
	return &LocalctlError{ExitCode: ExitFailed, Message: message}
}

// ---------------------------------------------------------------------------------------------
// mcp install / skill install
//
// `mcp install` registers the *running* `hearth` binary's absolute path
// directly in an MCP host's JSON config — no wrapper script — merging only
// `{command, args}`.
// ---------------------------------------------------------------------------------------------

func mcpCommand(root string, rest []string, io *Io) (int, error) {
	if len(rest) == 0 {
		return 0, usageErr("usage: hearth mcp install [--name <name>] [--json] <config-file>...")
	}
	switch rest[0] {
	case "install":
		return mcpInstallCommand(root, rest[1:], io)
	default:
		return 0, usageErr("unknown mcp subcommand: " + rest[0])
	}
}

// mcpInstallCommand merges `mcpServers.<name>` into each given JSON file,
// setting only `command`/`args` and preserving every other field already on
// that entry and every other entry in the file. Creates the file (as `{}`) if
// it doesn't exist yet.
func mcpInstallCommand(root string, rest []string, io *Io) (int, error) {
	flags, err := ParseCommandFlags(rest, []Flag{FlagName, FlagKey, FlagJSON})
	if err != nil {
		return 0, err
	}
	if len(flags.Positionals) == 0 {
		return 0, usageErr("usage: hearth mcp install [--name <name>] [--key <topLevelKey>] [--json] <config-file>...")
	}
	name := "hearth"
	if flags.Name != nil {
		name = *flags.Name
	}
	key := "mcpServers"
	if flags.Key != nil {
		key = *flags.Key
	}
	exe, err := os.Executable()
	if err != nil {
		return 0, failErr(ExitFailed, "could not resolve the running hearth binary's own path: "+err.Error())
	}
	resolvedRoot := canonicalize(root)
	command := exe
	args := []string{"--root", resolvedRoot, "mcp"}
	for _, configFile := range flags.Positionals {
		if err := installMcpEntry(configFile, key, name, command, args); err != nil {
			return 0, err
		}
	}
	if flags.JSON {
		files := make([]any, 0, len(flags.Positionals))
		for _, f := range flags.Positionals {
			files = append(files, f)
		}
		argList := make([]any, 0, len(args))
		for _, a := range args {
			argList = append(argList, a)
		}
		payload := omap.New()
		payload.Set("key", key)
		payload.Set("server", name)
		payload.Set("command", command)
		payload.Set("args", argList)
		payload.Set("files", files)
		out(io, printValue(payload, true))
	} else {
		for _, configFile := range flags.Positionals {
			out(io, fmt.Sprintf("installed mcp server %q into %s", name, configFile))
		}
	}
	return 0, nil
}

// installMcpEntry merges `<key>.<name>` (default key: `mcpServers`, the shape
// every standard MCP host config shares) into path, touching only
// `command`/`args` on that entry. Insertion order is preserved so a
// one-entry change never turns into a huge, unreviewable diff.
func installMcpEntry(path, key, name, command string, args []string) error {
	var document any
	if fileExists(path) {
		text, err := os.ReadFile(path)
		if err != nil {
			return failErr(ExitFailed, fmt.Sprintf("could not read %s: %v", path, err))
		}
		parsed, err := omap.ParseJSON(string(text))
		if err != nil {
			return failErr(ExitFailed, fmt.Sprintf("could not parse %s as JSON: %v", path, err))
		}
		document = parsed
	} else {
		document = omap.New()
	}
	rootObject, ok := document.(*omap.OMap)
	if !ok {
		return failErr(ExitFailed, fmt.Sprintf("%s's top level is not a JSON object", path))
	}
	serversValue, ok := rootObject.Get(key)
	if !ok {
		serversValue = omap.New()
		rootObject.Set(key, serversValue)
	}
	serversObject, ok := serversValue.(*omap.OMap)
	if !ok {
		return failErr(ExitFailed, fmt.Sprintf("%s's %q is not a JSON object", path, key))
	}
	entryValue, ok := serversObject.Get(name)
	if !ok {
		entryValue = omap.New()
		serversObject.Set(name, entryValue)
	}
	entryObject, ok := entryValue.(*omap.OMap)
	if !ok {
		return failErr(ExitFailed, fmt.Sprintf("%s's %q is not a JSON object", path, key+"."+name))
	}
	entryObject.Set("command", command)
	argList := make([]any, 0, len(args))
	for _, a := range args {
		argList = append(argList, a)
	}
	entryObject.Set("args", argList)
	parent := filepath.Dir(path)
	if parent != "" && parent != "." {
		if err := os.MkdirAll(parent, 0o755); err != nil {
			return failErr(ExitFailed, fmt.Sprintf("could not create %s: %v", parent, err))
		}
	}
	serialized := prettyJSON(document) + "\n"
	if err := os.WriteFile(path, []byte(serialized), 0o644); err != nil {
		return failErr(ExitFailed, fmt.Sprintf("could not write %s: %v", path, err))
	}
	return nil
}

// ---------------------------------------------------------------------------------------------
// skill install
// ---------------------------------------------------------------------------------------------

//go:embed skill/SKILL.md
var skillMarkdown string

//go:embed skill/scripts/hearth.sh
var skillScriptHearth string

//go:embed skill/scripts/status.sh
var skillScriptStatus string

//go:embed skill/scripts/logs.sh
var skillScriptLogs string

//go:embed skill/scripts/urls.sh
var skillScriptURLs string

//go:embed skill/scripts/doctor.sh
var skillScriptDoctor string

//go:embed skill/scripts/manage.sh
var skillScriptManage string

//go:embed skill/scripts/trace.sh
var skillScriptTrace string

//go:embed skill/scripts/events.sh
var skillScriptEvents string

//go:embed skill/scripts/restart-daemon.sh
var skillScriptRestartDaemon string

//go:embed skill/scripts/stop-daemon.sh
var skillScriptStopDaemon string

//go:embed skill/scripts/shared-list.sh
var skillScriptSharedList string

//go:embed skill/scripts/shared-status.sh
var skillScriptSharedStatus string

//go:embed skill/scripts/shared-connection.sh
var skillScriptSharedConnection string

// skillScripts is the ordered (name, body) list the skill pack installs.
var skillScripts = []struct {
	name string
	body string
}{
	{"hearth.sh", skillScriptHearth},
	{"status.sh", skillScriptStatus},
	{"logs.sh", skillScriptLogs},
	{"urls.sh", skillScriptURLs},
	{"doctor.sh", skillScriptDoctor},
	{"manage.sh", skillScriptManage},
	{"trace.sh", skillScriptTrace},
	{"events.sh", skillScriptEvents},
	{"restart-daemon.sh", skillScriptRestartDaemon},
	{"stop-daemon.sh", skillScriptStopDaemon},
	{"shared-list.sh", skillScriptSharedList},
	{"shared-status.sh", skillScriptSharedStatus},
	{"shared-connection.sh", skillScriptSharedConnection},
}

func skillCommand(root string, rest []string, io *Io) (int, error) {
	if len(rest) == 0 {
		return 0, usageErr("usage: hearth skill install --dest <skill-dir>")
	}
	switch rest[0] {
	case "install":
		return skillInstallCommand(root, rest[1:], io)
	default:
		return 0, usageErr("unknown skill subcommand: " + rest[0])
	}
}

func skillInstallCommand(root string, rest []string, io *Io) (int, error) {
	flags, err := ParseCommandFlags(rest, []Flag{FlagDest})
	if err != nil {
		return 0, err
	}
	if len(flags.Positionals) > 0 {
		return 0, usageErr("skill install takes no positional arguments")
	}
	if flags.Dest == nil {
		return 0, usageErr("usage: hearth skill install --dest <skill-dir>")
	}
	dest := *flags.Dest
	// `--dest` is the skill directory (e.g. ~/.agents/skills/hearth). A trailing
	// SKILL.md path is accepted and normalized to its parent so older one-file
	// installs still work.
	resolved := dest
	if !filepath.IsAbs(dest) {
		resolved = filepath.Join(root, dest)
	}
	if filepath.Base(resolved) == "SKILL.md" {
		resolved = filepath.Dir(resolved)
	}
	scriptsDir := filepath.Join(resolved, "scripts")
	if err := os.MkdirAll(scriptsDir, 0o755); err != nil {
		return 0, failErr(ExitFailed, fmt.Sprintf("could not create %s: %v", scriptsDir, err))
	}
	skillMD := filepath.Join(resolved, "SKILL.md")
	if err := os.WriteFile(skillMD, []byte(skillMarkdown), 0o644); err != nil {
		return 0, failErr(ExitFailed, fmt.Sprintf("could not write %s: %v", skillMD, err))
	}
	for _, script := range skillScripts {
		path := filepath.Join(scriptsDir, script.name)
		if err := os.WriteFile(path, []byte(script.body), 0o755); err != nil {
			return 0, failErr(ExitFailed, fmt.Sprintf("could not write %s: %v", path, err))
		}
		if err := os.Chmod(path, 0o755); err != nil {
			return 0, failErr(ExitFailed, fmt.Sprintf("could not chmod %s: %v", path, err))
		}
	}
	out(io, "installed hearth skill at "+resolved)
	return 0, nil
}

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

func firstOrNil(items []string) *string {
	if len(items) == 0 {
		return nil
	}
	return &items[0]
}

func derefString(s *string) string {
	if s == nil {
		return ""
	}
	return *s
}

func findRow(rows []state.ServiceLifecycleState, serviceID string) *state.ServiceLifecycleState {
	for i := range rows {
		if rows[i].ServiceID == serviceID {
			return &rows[i]
		}
	}
	return nil
}

func fileExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// canonicalize resolves root the way Rust's `std::fs::canonicalize` does,
// falling back to the original path when it cannot.
func canonicalize(root string) string {
	abs, err := filepath.Abs(root)
	if err != nil {
		return root
	}
	resolved, err := filepath.EvalSymlinks(abs)
	if err != nil {
		return abs
	}
	return resolved
}

// ---------------------------------------------------------------------------------------------
// JSON output
// ---------------------------------------------------------------------------------------------

// printValue renders a value for the CLI: compact JSON for --json, the raw
// string when the value is one, pretty JSON otherwise.
func printValue(value any, jsonOutput bool) string {
	value = jsonValue(value)
	if jsonOutput {
		return compactJSON(value)
	}
	if s, ok := value.(string); ok {
		return s
	}
	return prettyJSON(value)
}

// jsonValue normalizes a value into the ordered representation (omap.OMap /
// []any / scalars) the serializers understand. Structs and maps are marshaled
// and re-parsed so their key order survives.
func jsonValue(v any) any {
	switch t := v.(type) {
	case *omap.OMap:
		result := omap.New()
		t.Each(func(k string, e any) { result.Set(k, jsonValue(e)) })
		return result
	case []any:
		result := make([]any, len(t))
		for i, e := range t {
			result[i] = jsonValue(e)
		}
		return result
	case string, bool, int64, float64, nil:
		return v
	default:
		encoded, err := json.Marshal(v)
		if err != nil {
			return nil
		}
		parsed, err := omap.ParseJSON(string(encoded))
		if err != nil {
			return nil
		}
		return parsed
	}
}

func compactJSON(v any) string {
	var b strings.Builder
	writeCompact(&b, v)
	return b.String()
}

func writeCompact(b *strings.Builder, v any) {
	switch t := v.(type) {
	case *omap.OMap:
		b.WriteByte('{')
		keys := t.Keys()
		for i, k := range keys {
			if i > 0 {
				b.WriteByte(',')
			}
			b.WriteString(encodeString(k))
			b.WriteByte(':')
			value, _ := t.Get(k)
			writeCompact(b, value)
		}
		b.WriteByte('}')
	case []any:
		b.WriteByte('[')
		for i, e := range t {
			if i > 0 {
				b.WriteByte(',')
			}
			writeCompact(b, e)
		}
		b.WriteByte(']')
	default:
		b.WriteString(compactScalar(v))
	}
}

func prettyJSON(v any) string {
	var b strings.Builder
	writePretty(&b, v, 0)
	return b.String()
}

func writePretty(b *strings.Builder, v any, indent int) {
	switch t := v.(type) {
	case *omap.OMap:
		if t.Len() == 0 {
			b.WriteString("{}")
			return
		}
		b.WriteString("{\n")
		keys := t.Keys()
		for i, k := range keys {
			b.WriteString(strings.Repeat(" ", indent+2))
			b.WriteString(encodeString(k))
			b.WriteString(": ")
			value, _ := t.Get(k)
			writePretty(b, value, indent+2)
			if i < len(keys)-1 {
				b.WriteByte(',')
			}
			b.WriteByte('\n')
		}
		b.WriteString(strings.Repeat(" ", indent))
		b.WriteByte('}')
	case []any:
		if len(t) == 0 {
			b.WriteString("[]")
			return
		}
		b.WriteString("[\n")
		for i, e := range t {
			b.WriteString(strings.Repeat(" ", indent+2))
			writePretty(b, e, indent+2)
			if i < len(t)-1 {
				b.WriteByte(',')
			}
			b.WriteByte('\n')
		}
		b.WriteString(strings.Repeat(" ", indent))
		b.WriteByte(']')
	default:
		b.WriteString(compactScalar(v))
	}
}

func compactScalar(v any) string {
	switch t := v.(type) {
	case nil:
		return "null"
	case string:
		return encodeString(t)
	case bool:
		if t {
			return "true"
		}
		return "false"
	case int64:
		return strconv.FormatInt(t, 10)
	case float64:
		return strconv.FormatFloat(t, 'g', -1, 64)
	default:
		encoded, err := json.Marshal(v)
		if err != nil {
			return "null"
		}
		return string(encoded)
	}
}

// encodeString JSON-encodes a string the way serde_json does — no HTML
// escaping of `<`, `>` or `&`.
func encodeString(s string) string {
	var buf bytes.Buffer
	encoder := json.NewEncoder(&buf)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(s); err != nil {
		return `""`
	}
	return strings.TrimSuffix(buf.String(), "\n")
}
