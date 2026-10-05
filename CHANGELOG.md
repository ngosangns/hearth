# Changelog

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
