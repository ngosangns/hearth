# Changelog

## Unreleased

- New SwiftUI macOS app in `apps/macos-swiftui`, laid out like synca's: a unit-tested `HearthKit` and a three-column window (workspaces, services grouped by `groupTree`, log). Trust, daemon stop/restart, forget, group and shared stop/restart that affect other workspaces, shared remove, and port reclaim ask through a confirmation dialog. `task app:build` bundles it with the installed `hearth`; `task app:swift-test` runs its tests.

## 0.22.0

- Stopping a service also kills a child that a double fork reparented to launchd while it stayed in the service's process group (`(cmd &)`), including one that ignores SIGTERM. It used to survive: the stop saw the leader die, treated the tree as gone, and never sent SIGKILL.
- Stopping a service also kills a descendant that moved into its own session and was then orphaned (`setsid` plus a fork), as long as the 200 ms tree sampler saw it while it was still a child. A descendant that escapes before the first sample (about one second after spawn, while the identity settles) is still not tracked.
- A service whose main process exits no longer leaves its children running. `sleep 1000 & exit 1` used to record `failed` while `sleep` ran on with nothing tracking it. Children are now sent SIGTERM, then SIGKILL after the grace period, like children in their own process group already were.
- A readiness probe, `preparationCommand`, `stop:` command, compose command, or build that leaves a background child holding its output (`cmd &` without a redirect) has that child killed one second after the command exits. A `command` probe used to leave one such child behind every 1.5 s, and a preparation step that did this hung the start forever. A child that redirects its output (`cmd >/dev/null 2>&1 &`) is left alone. Shared-service pack scripts, extracts, and recipe commands follow the same rule, and their timeout now kills the leftover child instead of only the reaped leader.
- `hearth manager restart`, SIGTERM, and Ctrl+C still leave services running, but no longer leave the daemon's `docker logs --follow` followers behind. Each one used to be reparented to launchd and run until its container wrote again or stopped, one more per restart. The next daemon attaches its own.
- `hearth stop`, restart, and a `stop-services` shutdown now stop an `orphaned` service whose process still has the recorded pid and start time. A shell that runs setup and then `exec`s into its server (`sh -c 'setup; exec server'`) is marked `orphaned` once its command line changes, and used to fail with "no longer owned by this manager" and keep running. It now gets the same SIGTERM, then SIGKILL, to its whole process tree as an owned service. A pid whose start time differs (reused by another program) is still refused, and is not signalled.
- A daemon that exits (any shutdown) now kills a readiness probe or preparation command that is still running. It used to be left running under launchd. A daemon killed with SIGKILL still cannot clean up its in-flight probe.
- `hearth manager restart` and `manager ensure` no longer report "hearth manager is unavailable" while the new daemon is still starting. A daemon holds the lock with no port while it loads state and re-adopts services, which took about 7 s with 25 services, and `ensure` gave up after a fixed 5 s. It now waits for that daemon as long as its pid is alive (up to 120 s), does not spawn a second one meanwhile, and fails at once with "exited before it started listening" if it dies.
- A `hearth daemon` (or `hearth smp`) that loses the lock to a daemon already serving the same root now exits with status 3 and prints which pid and port hold it. It used to exit 0, which read as a successful start.
- `hearth status` prints the daemon's real state (`preparing`, `starting`, `running-unready`) instead of folding them into `running`. `status --json` keeps `state` as before and adds `actualState` with the real state.
- `hearth status` no longer prints a pid for a stopped, queued, preparing, failed, finished, or externally-owned row. That pid was the last recorded process, which was gone or could belong to something else.
- `hearth logs <name>` with a name that is not a service prints `unknown service: <name>` (exit 2) instead of the usage line.
- A start that fails because another process holds the service's port now says so: `hearth start` prints `<service>: Port N is held by pid P (command)` and a `--kill-unowned` hint instead of only "service operation failed". Other failed operations print their own error too. A service with a build, `preparation`, or `preparationCommand` checks the port before that step, so a held port fails in seconds instead of after the build.
- `hearth manager stop` (and the MCP `stop_daemon` tool) now also stops the project's `ownership: external` services that are up and declare a `stop:` command, such as a docker compose unit, and detaches the project from its `shared:` services. A shared instance is stopped too when no other project is still attached to it; one another project uses keeps running. `manager restart` and SIGTERM still leave every service running. `hearth shared detach <id> --stop-if-unused` and the `stopIfUnused` field on `POST /v1/shared/detach` are the new way to ask for this; a `manager stop` against an older smp falls back to a plain detach.

## 0.21.0

- Bulk, group, and multi-service `start` no longer serialize on a shared `__manager__` lock. Each operation locks only the services it touches (acquired in sorted order), so disjoint starts run in parallel and overlapping ones wait only on the shared services. `queued-start` still appears when a service is waiting on its own lock. Manager shutdown still uses `__manager__`.
- Scrolling the service list, the log, and the mouse wheel in `hearth tui` no longer waits on a log or snapshot request. Those fetches run in the background, at most one of each at a time, and a selected log refetches at most ten times a second. The screen redraws immediately after a key or the wheel, and at most once per frame otherwise.
- Readiness probes run every 1.5s for as long as the process is up, including after `ready`. A failing probe becomes `running-unready` and does not kill or fail the service. The probe stops when the service is stopped or restarted, the process exits, or the daemon shuts down. `readiness: process` still has no probe. `readiness: exit` still waits for the process to exit, polling at the same interval.
- `hearth start --wait`, a bulk start, and MCP `manage` settle when the process is up (`ready` or `running-unready`), not when the probe first passes. `readinessTimeoutMs` is still accepted and still bounds a single command probe and a one-shot shared attach. It no longer fails a long-lived service.
- A `readiness: exit` service that finished (`succeeded`) shows its URLs in `hearth tui`, including ones that default to `requiresRunning: true`. `hearth urls` no longer marks them `(not running)`; its `--json` `running` field stays `false`.

## 0.20.0

- Restarting a service or a group no longer stops at "cannot be stopped" when the row has no process and no `stop` command. The service is started. A listener that is the service itself is replaced first. Another program on the port is left alone. `hearth stop <group>` and `hearth restart <group>` run without `--wait`. In `hearth tui`, a group stop or restart waits for every member and the notice names a failure instead of only showing the verb.
- Daemon startup re-checks a service stuck on `external` / "Port N is held". A free port no longer keeps that error: desired running becomes failed ("Managed process is no longer alive") and is not started. A holder that is the service itself (the exec program in that directory, or the install directory in a Java command line) is adopted. Another program on the port is left alone.
- Restarting a shared service no longer stays `running-unready` after `hearth shared attach` has exited. A non-zero exit fails immediately, and exit 0 is probed for at most five seconds. A process that rewrites its title but keeps the same absolute executable (Redis) stays owned, and a start adopts it instead of spawning a second copy that fails with `bind: Address already in use`. `hearth` and shared interpreters are not matched this way.
- `hearth manager restart` kills every other hearth daemon for that same project (and every other `hearth smp` when restarting the shared daemon) before starting a new one. Services stay up. A service restart kills other host processes running that same command from the service's directory, including a child that moved into its own process group.
- Restarting the daemon from `hearth tui` no longer stays on "restarting daemon…" after the old daemon has exited. The exited daemon was a zombie of the TUI, and `kill(pid, 0)` still reported it alive for the whole stop timeout.

## 0.18.3

- In `hearth tui`, the word `shared` on a project shared service is blue. The state word keeps its own colour.
- Stop, restart, or remove of a shared instance, and stop or restart of a project `shared:` service, asks for a second press when another workspace is attached and names those workspaces. Stopping or restarting the instance takes it down for every attachment. Stopping or restarting it from a project only detaches that workspace. If the attachment list cannot be read, the second press is an explicit override.

## 0.18.2

- A one-time command (`readiness: { kind: exit }`) stays `running` until the process exits. It does not become `ready`, and `readinessTimeoutMs` does not stop it. Exit 0 is still `succeeded`.
- The TUI no longer shows "Manager restarted; state resynchronized." after a manager restart.

## 0.18.1

- In `hearth tui`, drag a painted service URL to copy it while mouse reporting is on. A click or a drag shorter than two columns copies the whole address; a longer drag copies that span. `m` still releases the mouse for a native selection.
- A bare `hearth` on a terminal opens the TUI. Piped input or output still prints help.

## 0.18.0

The CLI command is `hearth`. It was `hearthd` through 0.17.0.

- Install path: `~/.local/bin/hearth` → `~/.local/share/hearth/bin/hearth-<version>`.
- `hearth update` downloads the GitHub asset `hearth-vX.Y.Z`.
- `hearthd update` from 0.17.0 cannot install this release. That client looks for an asset named `hearthd-vX.Y.Z` and a version line starting with `hearthd `. Install 0.18.0 with `task install`.
- `task install` and a successful `hearth update` remove a leftover `~/.local/bin/hearthd` symlink. They leave the old versioned file in place, so a daemon still mapped to it keeps running until restart.
