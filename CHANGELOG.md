# Changelog

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
