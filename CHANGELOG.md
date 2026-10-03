# Changelog

## 0.18.1

- In `hearth tui`, drag a painted service URL to copy it while mouse reporting is on. A click or a drag shorter than two columns copies the whole address; a longer drag copies that span. `m` still releases the mouse for a native selection.
- A bare `hearth` on a terminal opens the TUI. Piped input or output still prints help.

## 0.18.0

The CLI command is `hearth`. It was `hearthd` through 0.17.0.

- Install path: `~/.local/bin/hearth` → `~/.local/share/hearth/bin/hearth-<version>`.
- `hearth update` downloads the GitHub asset `hearth-vX.Y.Z`.
- `hearthd update` from 0.17.0 cannot install this release. That client looks for an asset named `hearthd-vX.Y.Z` and a version line starting with `hearthd `. Install 0.18.0 with `task install`.
- `task install` and a successful `hearth update` remove a leftover `~/.local/bin/hearthd` symlink. They leave the old versioned file in place, so a daemon still mapped to it keeps running until restart.
