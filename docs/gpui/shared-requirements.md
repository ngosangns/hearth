# Hearth shared services: requirements

Status: draft, 2026-10-22. Source of truth for the hearth side: `hearth/docs/shared-services.md`.

## 1. Purpose

Many workspaces need the same infrastructure: MongoDB, Redis, MinIO, Kafka, nginx. Without shared
services, each workspace runs its own copy. That costs memory and ports, and each copy needs its own
install.

Hearth solves this with one machine-wide pool:

- A workspace declares what it needs in `hearth.yaml` under `shared:` (for example `redis: "8.2.10"`).
- Hearth installs that exact `name@version` once into `~/.hearth/shared/` (no Docker, no Homebrew).
- One global daemon, `smp` (`hearth smp`), runs one instance per `name@version`.
- When a workspace starts its shared entry, Hearth *attaches* the workspace to the instance. Attach
  provisions a private resource for that workspace (database `h_<projectId>`, bucket
  `h-<projectId>`, topic, or nginx conf) and records the connection URL and env values.
- Stopping the workspace entry *detaches* it. The instance keeps running for other workspaces.
  `hearth manager stop` stops the instance when no workspace is attached any more.
  `remove` is the only delete.

Vocabulary used in the UI (one meaning each):

| Term | Meaning |
|---|---|
| Shared service | The feature as a whole. |
| Instance | One installed `name@version` in `~/.hearth/shared`, run by `smp`. |
| Recipe | A `name@version` the catalog can install. Not installed yet, or already installed. |
| Shared daemon | `smp`, the one process that runs all instances. |
| Attached workspace | A workspace that uses an instance, with its own database/bucket/topic. |
| Connection | The URL and env values a workspace uses to reach its part of the instance. |

## 2. Users and use cases

The user is a developer with several local workspaces.

1. "What shared infrastructure runs on this machine, and is it up?"
2. "Which workspaces use this Redis? Can I stop it safely?"
3. "What is the connection URL / env for workspace X on this instance?" (copy it)
4. "The shared daemon is down. Start it."
5. "An install failed. Why?"
6. "I want Postgres/Kafka for a new workspace. What can I add, and what do I put in `hearth.yaml`?"
7. "Clean up an instance no workspace uses any more."
8. "Read the instance log when it misbehaves."

## 3. Functional requirements

- F1. Show the shared daemon state (running / not running). When it is not running, offer
  "Start shared daemon" (`hearth shared ensure --json`). Instance states then read "Unknown", not "—".
- F2. List instances with: state, ports (primary and extra), install state, install error,
  and the number of attached workspaces.
- F3. Group instances: "In use" (one or more attached workspaces) and "Not in use".
- F4. Instance detail (selected instance): attached workspaces with path and attach date, and per
  workspace the connection URL and env values. Every value has a copy button. Secret-looking env
  values (`*SECRET*`, `*PASSWORD*`, `*TOKEN*`) are masked; copy still copies the real value.
- F5. "Open workspace" on an attached workspace that is in Hearth's workspace list switches to it.
- F6. Instance actions: start, stop, restart, remove, with confirmations. The stop/remove
  confirmation names the attached workspaces. Show the log of the selected instance.
- F7. Recipes ("Available to install"): grouped by service name, versions listed, installed ones
  marked. Actions: install, and "Copy hearth.yaml entry" (`shared:\n  <name>: "<version>"`).
- F8. Action feedback: the row shows the pending state and a spinner. Failures show as a toast.
  Nothing is inserted above the lists.

## 4. UX requirements

- U1. A short explanation at the top of the Shared pane (one or two sentences, fixed height).
- U2. Empty states that say what to do: no instances ("Add a `shared:` entry to a workspace's
  hearth.yaml, then start it. Or install a recipe below.").
- U3. Every button has a tooltip that says what happens, including side effects
  ("Stop redis@8.2.10. 2 workspaces lose it until it starts again.").
- U4. No layout jumps: pending state and errors never insert or remove rows above the lists.
  Fixed slots for spinners.
- U5. Plain sentence-case labels. Use the vocabulary in section 1.
- U6. Paths show with `~` (display_path). Long values truncate with a tooltip showing the full value.

## 5. Non-goals (this round)

- Detach a single workspace from the GUI (the workspace's own stop does it).
- Edit `hearth.yaml` from the Shared pane.
- Change the catalog URL from the GUI.
- Clean up orphaned per-project resources inside an instance (moved or renamed project).
- Upgrade an instance to a new version (versions run side by side by design).
- Any change to the hearth CLI or its data files.

## 6. Open questions

1. Should "Remove" on an instance that is in use be allowed at all from the GUI (today it passes
   `--force`)? Or only after the user stops those workspaces?
2. Should "Stop" on an in-use instance stop it (today: yes, `hearth shared stop`), or detach first?
3. Should the Workspaces board show, for each `shared:` entry, a link to the instance in the Shared pane?
4. Should Hearth start the shared daemon automatically when the Shared pane opens? (Today: no, and F1
   keeps it an explicit action.)
5. Is a "Not in use for N days" hint useful for clean-up?

## 7. Audit of the current Shared pane (before this round)

- A1. No explanation of what Shared is. The pane is two flat lists: "Instances" and "Recipes".
- A2. When `smp` is down, `status` fails silently. States show "—" and nothing says the daemon is
  down or how to start it.
- A3. Attachments show only as "N attached". The user cannot see which workspaces, or their
  connection URL/env.
- A4. Extra ports (MinIO console, Kafka controller, nginx 443) are not shown.
- A5. Install errors (`installError`) are not shown.
- A6. Recipes are a flat `name@version` list with only "Install". Nothing tells the user that the
  normal path is a `shared:` entry in `hearth.yaml`.
- A7. Actions insert an "Installing…/Stopping…" strip above the Instances list, which pushes the
  lists down (fixed in the same round: in-row spinner).
- A8. Tooltips say only "Stop", "Remove". They do not say what happens to attached workspaces.
- A9. Instance rows have no pending state while an action runs.

## 8. Implemented in this round

Shipped in `apps/hearth-gpui` (the sidebar is unchanged):

- F1. Shared daemon row: "Shared daemon running" / "Shared daemon not running", with "Start shared daemon" (`hearth shared ensure --json`) only while it is down, and Refresh. Instance state reads "Unknown" when the daemon is down and the live state is empty.
- F2. Instance rows show state, the primary port and extra ports with labels, the install state when it is not `installed`, the install error, and the workspace count.
- F3. Instances are grouped into "In use" and "Not in use", each with a count.
- F4. The dock shows "Used by" above the log: workspace name and path, attach date, connection URL, and env lines. Each value has a copy button. Env keys containing SECRET, PASSWORD, or TOKEN are shown as `••••••`; copy still copies the real value.
- F5. "Open workspace" on an attached workspace that is in Hearth's workspace list switches to the Workspaces pane and selects it.
- F6. Start, stop, restart, and remove stay on the existing confirmations. Stop and remove name the attached workspace paths. The selected instance log stays under the details block.
- F7. "Available to install" groups recipes by service name. Each version has Install or Reinstall, and "Copy hearth.yaml entry" copies `shared:\n  <name>: "<version>"`.
- F8. Pending install, start, stop, restart, and remove use the in-row spinner. Failures stay toasts. The intro, daemon row, and section heads stay in place.
- U1. The intro is the two sentences from the spec, always at the top.
- U2. No instances: "Add a `shared:` entry to a workspace's hearth.yaml, then start it. Or install a recipe below."
- U3. Every button has a tooltip. Stop, restart, and remove name the instance and how many workspaces lose it.
- U4. Spinner slots are fixed width. Install errors occupy their own line on the instance row. Actions do not insert a strip above the lists.
- U5. Labels are sentence case and use the vocabulary in section 1.
- U6. Paths use `display_path` (`~`). Long ports, errors, URLs, and env lines truncate, with the full value in a tooltip.

Still deferred, as in section 5: detach one workspace from this pane, edit `hearth.yaml` here, change the catalog URL, clean up orphaned per-project resources, upgrade an instance to a new version, and any change to the hearth CLI.

Open questions in section 6 are unchanged. Remove on an instance that is in use still passes `--force` after the confirmation lists those workspaces. Stop still runs `hearth shared stop` and does not detach first. Opening the Shared pane does not start the shared daemon.
