//! Hearth's window. The Go CLI stays the engine: this entity reads status, urls,
//! logs, the workspace file, and the shared registry, and sends lifecycle
//! commands only after the matching confirmation.

#[cfg(test)]
mod bench;
mod catalog;
mod controls;
mod logbuf;
mod logview;
#[cfg(feature = "devtools")]
mod perf;
mod store;

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use catalog::{
    Catalog, CatalogStamps, ServiceGroup, expand, offline_ids, service_groups_hiding,
    should_reload_catalog, touches_shared, visible_groups,
};
use controls::{
    ACTIONS_COL, TEXT_ACTION_HEIGHT, Weight, empty_state, group_cluster, group_header, row_action,
    state_badge, tag_badge, text_action,
};
use ely_gpui_component::data_display::{Badge, Tone as BadgeTone};
use ely_gpui_component::feedback::{ToastViewport, Toaster};
use ely_gpui_component::forms::Switch;
use ely_gpui_component::layout::{ScrollArea, Scrollbar};
use ely_gpui_component::menus::{ContextMenu, Menu, MenuItem};
use ely_gpui_component::motion::Spinner;
use ely_gpui_component::navigation::{Command, CommandPalette};
use ely_gpui_component::overlays::ConfirmDialog;
use ely_gpui_component::primitives::{Icon, Severity, Tooltip};
use ely_gpui_component::settings::{SettingsRow, SettingsSection};
use ely_gpui_component::shell::{UpdateBanner, UpdateState};
use ely_gpui_component::theme::IconSize;
use gpui::{
    Axis, Bounds, ClipboardItem, Context, Entity, FocusHandle, IntoElement, KeyDownEvent,
    MouseButton, Pixels, Render, Role, ScrollHandle, ScrollStrategy, ScrollWheelEvent,
    SharedString, Subscription, UniformListScrollHandle, canvas, div, point, prelude::*, px,
    uniform_list,
};
use logbuf::{LogFetch, LogPage};
use ns_ui::{
    IconTone, SIDEBAR_ROW_H, SIDEBAR_SECTION_H, SidebarKey, Tone, awareness, button, chrome_busy,
    icon_button, quiet_button, quiet_button_md, settings_rail, side_column, sidebar_actions,
    sidebar_button, sidebar_empty, sidebar_keys, sidebar_reveal, sidebar_row,
    sidebar_scroll_tracked, sidebar_section, sidebar_step, skeleton_list, spinner, tab_strip,
    theme, working_row,
};
use serde::Deserialize;
use serde_json::Value;
use store::Workspace;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Workspaces,
    Shared,
}

enum Pending {
    Trust,
    StopDaemon,
    RestartDaemon,
    Forget,
    Reclaim(String),
    StopServices(Vec<String>),
    RestartServices(Vec<String>),
    InstallShared(String),
    StartShared(String),
    StopShared(String),
    RestartShared(String),
    RemoveShared { id: String, force: bool },
    InstallUpdate,
}

/// One confirmation. It carries the action that runs when the confirm button is chosen.
struct DialogRequest {
    destructive: bool,
    title: String,
    message: String,
    confirm: String,
    pending: Pending,
}

struct Report {
    ok: bool,
    text: String,
}

/// One lifecycle command still waiting for the status read that follows it.
struct InFlight {
    id: u64,
    label: String,
    verb: &'static str,
    /// Service id, group name, "all services", or "daemon".
    target: String,
    /// Rows that show the pending state. Empty when the command does not change those rows yet.
    ids: Vec<String>,
    daemon: bool,
    /// Folder the command was sent to. Shared commands record it too.
    root: Option<String>,
    /// Reload generation that drops this mark. Empty while the command is still running.
    clear_after: Option<u64>,
    /// Shared registry, not the workspace board.
    shared: bool,
}

#[derive(Clone)]
struct Service {
    id: String,
    state: String,
}

#[derive(Clone)]
struct UrlRow {
    service: String,
    label: String,
    url: String,
    visible: bool,
}

/// One workspace attached to an instance, with the connection it was given.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Attachment {
    project_id: String,
    root: String,
    attached_at: String,
    url: Option<String>,
    env: Vec<(String, String)>,
    error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Instance {
    id: String,
    port: Option<u64>,
    extra_ports: Vec<u64>,
    /// Labels for `extra_ports`, from the recipe snapshot or the catalog.
    extra_labels: Vec<String>,
    install: String,
    install_error: Option<String>,
    actual: String,
    /// Attached workspaces, sorted by root.
    attachments: Vec<Attachment>,
}

/// `hearth shared list` grouped into one row per service name.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RecipeGroup {
    name: String,
    versions: Vec<String>,
}

#[derive(Clone)]
struct Board {
    services: Vec<Service>,
    urls: Vec<UrlRow>,
    catalog: Catalog,
    daemon_up: bool,
    port: Option<u64>,
    daemon_note: String,
    note: String,
    stamp: Option<u64>,
}

/// Derived board data the workspace render path reads every frame: service
/// sections, per-service URL indexes, and the status aggregates. Rebuilt only
/// when the board inputs (services, urls, catalog, hide disabled) move.
#[derive(Default)]
struct BoardCache {
    sections: Vec<ServiceGroup>,
    /// Indexes into `urls` per service id, in board order.
    urls_by_service: HashMap<String, Vec<usize>>,
    /// Catalog-disabled ids, for O(1) row checks.
    disabled: HashSet<String>,
    counts: Counts,
    /// Ids of the failed services the board lists, `hide_disabled` applied.
    failed: Vec<String>,
}

impl BoardCache {
    fn refresh(
        services: &[Service],
        urls: &[UrlRow],
        catalog: &Catalog,
        hide_disabled: bool,
    ) -> Self {
        let ids: Vec<String> = services.iter().map(|service| service.id.clone()).collect();
        let groups = if hide_disabled {
            visible_groups(&catalog.groups, &catalog.disabled)
        } else {
            catalog.groups.clone()
        };
        let sections = service_groups_hiding(
            &ids,
            &groups,
            if hide_disabled {
                &catalog.disabled
            } else {
                &[]
            },
        );
        let mut urls_by_service: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, url) in urls.iter().enumerate() {
            urls_by_service
                .entry(url.service.clone())
                .or_default()
                .push(index);
        }
        let disabled: HashSet<String> = catalog.disabled.iter().cloned().collect();
        let counts = count_services(services, &catalog.disabled, hide_disabled);
        let failed = services
            .iter()
            .filter(|service| matches!(service.state.as_str(), "failed" | "crashed"))
            .filter(|service| !(hide_disabled && disabled.contains(&service.id)))
            .map(|service| service.id.clone())
            .collect();
        BoardCache {
            sections,
            urls_by_service,
            disabled,
            counts,
            failed,
        }
    }
}

/// Instance strings that depend only on the registry row, preformatted once
/// per shared load instead of per frame.
#[derive(Default)]
struct InstanceRowText {
    stop_tip: String,
    start_tip: String,
    restart_tip: String,
    remove_tip: String,
    stop_message: String,
    remove_message: String,
    restart_message: String,
    facts: String,
    error: Option<String>,
}

/// One installable recipe version with everything its row shows.
#[derive(Default)]
struct RecipeVersionRow {
    version: String,
    /// Index into `recipes`.
    index: usize,
    /// `name@version`.
    id: String,
    installed: bool,
    copy_entry: String,
    copy_tip: String,
    install_tip: String,
}

/// One service name in the shared catalog and its version rows.
struct RecipeGroupCache {
    name: String,
    versions: Vec<RecipeVersionRow>,
}

/// Derived shared-pane data, rebuilt only when the shared registry moves.
#[derive(Default)]
struct SharedCache {
    in_use: Vec<usize>,
    idle: Vec<usize>,
    recipe_groups: Vec<RecipeGroupCache>,
    rows: Vec<InstanceRowText>,
}

impl SharedCache {
    fn refresh(recipes: &[(String, String)], instances: &[Instance]) -> Self {
        let mut in_use = Vec::new();
        let mut idle = Vec::new();
        for (index, instance) in instances.iter().enumerate() {
            if instance.attachments.is_empty() {
                idle.push(index);
            } else {
                in_use.push(index);
            }
        }
        let installed: HashSet<String> = instances.iter().map(|item| item.id.clone()).collect();
        let mut groups: Vec<RecipeGroupCache> = Vec::new();
        for group in group_recipes(recipes) {
            let mut rows = Vec::with_capacity(group.versions.len());
            for version in &group.versions {
                let Some(index) = recipe_index(recipes, &group.name, version) else {
                    continue;
                };
                let id = format!("{}@{version}", group.name);
                let installed_row = installed.contains(&id);
                rows.push(RecipeVersionRow {
                    version: version.clone(),
                    index,
                    installed: installed_row,
                    copy_entry: hearth_yaml_entry(&group.name, version),
                    copy_tip: format!("Copy a hearth.yaml entry for {id}."),
                    install_tip: if installed_row {
                        format!(
                            "Reinstall {id}. This replaces the installed copy. This can start the shared daemon."
                        )
                    } else {
                        format!("Install {id}. This can start the shared daemon, then install the recipe.")
                    },
                    id,
                });
            }
            groups.push(RecipeGroupCache {
                name: group.name,
                versions: rows,
            });
        }
        let rows = instances
            .iter()
            .map(|instance| {
                let count = instance.attachments.len();
                let ports = port_line(instance.port, &instance.extra_ports, &instance.extra_labels);
                let mut facts = workspace_count(count);
                if !ports.is_empty() {
                    facts = format!("{ports} · {facts}");
                }
                if show_install(&instance.install) {
                    facts = format!("{facts} · {}", instance.install);
                }
                InstanceRowText {
                    stop_tip: stop_tip(&instance.id, count),
                    start_tip: start_tip(&instance.id),
                    restart_tip: restart_tip(&instance.id, count),
                    remove_tip: remove_tip(&instance.id, count),
                    stop_message: stop_confirm(instance),
                    remove_message: remove_confirm(instance),
                    restart_message: format!(
                        "The instance stops, then starts. {}",
                        loss_sentence(count, " until it is back.")
                    ),
                    facts,
                    error: instance.install_error.clone(),
                }
            })
            .collect();
        SharedCache {
            in_use,
            idle,
            recipe_groups: groups,
            rows,
        }
    }
}

enum LogKind {
    Tail,
    Earlier { origin: u64, end: u64 },
    Refresh { origin: u64, end: u64 },
}

struct HearthWindow {
    file: std::path::PathBuf,
    workspaces: Vec<Workspace>,
    selected: Option<usize>,
    services: Vec<Service>,
    selected_service: Option<usize>,
    urls: Vec<UrlRow>,
    catalog: Catalog,
    daemon_up: bool,
    port: Option<u64>,
    daemon_note: String,
    pane: Pane,
    recipes: Vec<(String, String)>,
    instances: Vec<Instance>,
    /// `hearth shared status` succeeded, so the shared daemon is running.
    smp_up: bool,
    shared_pick: Option<usize>,
    recipe_pick: Option<usize>,
    draft: String,
    /// The loaded log, shared with the list closure instead of cloned per frame.
    log_lines: Rc<Vec<String>>,
    log_paints: Rc<Vec<Vec<logbuf::LogPaint>>>,
    log_origin: u64,
    log_end: u64,
    log_has_more: bool,
    log_loading: bool,
    log_service: String,
    /// The runtime directory the open log came from. A workspace service and a
    /// shared instance keep their logs under different roots.
    log_dir: PathBuf,
    /// The open log belongs to a shared instance, not a workspace service.
    log_shared: bool,
    log_follow: bool,
    log_epoch: u64,
    log_scroll: UniformListScrollHandle,
    log_sel: Option<logbuf::LogSelection>,
    log_selecting: bool,
    log_focus: FocusHandle,
    log_watch: u64,
    log_refreshing: bool,
    status_dirty: bool,
    /// One `hearth events --follow` child per listed workspace root, so every
    /// row stays live without being selected.
    watchers: HashMap<String, DaemonWatch>,
    /// Watch generation per root. A dispatcher task exits once its root's id
    /// moved, so a removed-and-re-added folder never runs two dispatchers.
    watch_ids: HashMap<String, u64>,
    next_watch: u64,
    /// Last-read board per root. Sidebar dots read it; selecting a workspace
    /// paints it before the authoritative read returns.
    boards: HashMap<String, Board>,
    /// Roots with a background `load_board` in flight.
    board_loading: HashSet<String>,
    /// Invalidation that arrived while a root's read was in flight.
    board_dirty: HashSet<String>,
    log_char_w: f32,
    view_h: f32,
    catalog_stamps: CatalogStamps,
    pending: Option<Pending>,
    /// Confirmations waiting behind the one on screen.
    dialogs: VecDeque<DialogRequest>,
    /// The confirmation on screen.
    dialog: Option<DialogRequest>,
    /// The action of the last confirmation closed. Confirm runs it after the
    /// dialog's own close, which has already cleared `dialog`.
    answered: Option<Pending>,
    toaster: Entity<Toaster>,
    palette_open: bool,
    update_dismissed: bool,
    _keys: Subscription,
    note: String,
    update: Option<ns_ui::UpdateReport>,
    busy: bool,
    /// The selected workspace changed and its board has not arrived. The old
    /// services, daemon badge, and actions stay off screen until then.
    board_pending: bool,
    /// Start, stop, and restart commands that have not yet been replaced by a
    /// fresh status read. Several can run at once, on different targets.
    inflight: Vec<InFlight>,
    next_job: u64,
    generation: u64,
    /// `load_shared` generation. A workspace refresh must not drop a shared read.
    shared_generation: u64,
    /// Workspace row whose remove control is forced on (the dev `hover` command).
    /// A real pointer uses the row's own hover, so this stays `None` in normal use.
    hovered_workspace: Option<usize>,
    /// Arrow keys move the sidebar selection while this handle is focused.
    list_focus: FocusHandle,
    list_scroll: ScrollHandle,
    /// Hide catalog-disabled service rows, and a group whose services are all disabled.
    hide_disabled: bool,
    settings_open: bool,
    /// Bounds of the log pane, recorded during prepaint. The mouse callbacks no
    /// longer receive a hitbox, so the pane reads the last prepaint instead of
    /// the event.
    log_view_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Service groups the user folded away. Held for the session only.
    collapsed: HashSet<String>,
    /// The loaded log cut into display rows `log_cols` cells wide.
    log_rows: Rc<logview::Rows>,
    log_levels: Rc<Vec<logview::LineLevel>>,
    log_cols: usize,
    /// Derived board data for the workspace render path. Rebuilt only when the
    /// board inputs (services, urls, catalog, hide disabled) move.
    board_cache: BoardCache,
    board_cache_dirty: bool,
    /// Derived shared-pane data. Rebuilt only when the shared registry moves.
    shared_cache: SharedCache,
    shared_cache_dirty: bool,
}

impl HearthWindow {
    fn new(cx: &mut Context<Self>) -> Self {
        let file = store::default_path();
        let workspaces = store::load(&file).unwrap_or_default();
        let selected = if workspaces.is_empty() { None } else { Some(0) };
        let mut window = Self {
            file,
            workspaces,
            selected,
            services: Vec::new(),
            selected_service: None,
            urls: Vec::new(),
            catalog: Catalog::default(),
            daemon_up: false,
            port: None,
            daemon_note: String::new(),
            pane: Pane::Workspaces,
            recipes: Vec::new(),
            instances: Vec::new(),
            smp_up: false,
            shared_pick: None,
            recipe_pick: None,
            draft: String::new(),
            log_lines: Rc::new(Vec::new()),
            log_paints: Rc::new(Vec::new()),
            log_origin: 0,
            log_end: 0,
            log_has_more: false,
            log_loading: false,
            log_service: String::new(),
            log_dir: PathBuf::new(),
            log_shared: false,
            log_follow: true,
            log_epoch: 0,
            log_scroll: UniformListScrollHandle::new(),
            log_sel: None,
            log_selecting: false,
            log_focus: cx.focus_handle(),
            log_watch: 0,
            log_refreshing: false,
            status_dirty: false,
            watchers: HashMap::new(),
            watch_ids: HashMap::new(),
            next_watch: 1,
            boards: HashMap::new(),
            board_loading: HashSet::new(),
            board_dirty: HashSet::new(),
            log_char_w: 0.,
            view_h: 752.,
            catalog_stamps: CatalogStamps::default(),
            pending: None,
            dialogs: VecDeque::new(),
            dialog: None,
            answered: None,
            toaster: cx.new(|_| Toaster::default()),
            palette_open: false,
            update_dismissed: false,
            _keys: cx.observe_keystrokes(|this, event, _, cx| {
                let keys = &event.keystroke;
                if keys.modifiers == gpui::Modifiers::command() && keys.key == "k" {
                    this.palette_open = !this.palette_open;
                    cx.notify();
                }
            }),
            note: "Select a workspace.".into(),
            update: None,
            busy: false,
            board_pending: selected.is_some(),
            inflight: Vec::new(),
            next_job: 1,
            generation: 0,
            shared_generation: 0,
            hovered_workspace: None,
            list_focus: cx.focus_handle().tab_stop(true),
            list_scroll: ScrollHandle::new(),
            hide_disabled: read_hide_disabled(),
            settings_open: false,
            log_view_bounds: Rc::new(Cell::new(Bounds::default())),
            collapsed: HashSet::new(),
            log_rows: Rc::new(logview::Rows::default()),
            log_levels: Rc::new(Vec::new()),
            log_cols: 100,
            board_cache: BoardCache::default(),
            board_cache_dirty: true,
            shared_cache: SharedCache::default(),
            shared_cache_dirty: true,
        };
        // Reload after this constructor returns. A board that finishes while
        // the window is still being inserted cannot apply, and `busy` stays set.
        if window.selected.is_some() {
            let view = cx.entity().downgrade();
            cx.defer(move |cx| {
                view.update(cx, |this, cx| this.reload(cx)).ok();
            });
        }
        // The event stream is the daemon's invalidation signal. Service rows
        // remain authoritative through `status --json`; logs still use their
        // own file cursor watcher. Every listed workspace gets a watcher, not
        // just the selected one.
        window.sync_watches(cx);
        window
    }

    fn root(&self) -> Option<String> {
        self.selected
            .and_then(|index| self.workspaces.get(index))
            .map(|row| row.path.clone())
    }

    fn current(&self) -> Option<&Workspace> {
        self.selected.and_then(|index| self.workspaces.get(index))
    }

    /// Rebuild the derived caches whose inputs moved. A plain repaint finds
    /// both flags clear and does no work here.
    fn refresh_caches(&mut self) {
        if self.board_cache_dirty {
            self.board_cache = BoardCache::refresh(
                &self.services,
                &self.urls,
                &self.catalog,
                self.hide_disabled,
            );
            self.board_cache_dirty = false;
        }
        if self.shared_cache_dirty {
            self.shared_cache = SharedCache::refresh(&self.recipes, &self.instances);
            self.shared_cache_dirty = false;
        }
    }

    /// Status-bar text for the workspace on screen: its name and the counts of
    /// the rows listed, so the numbers match what the board shows.
    fn scope_summary(&self) -> String {
        let name = self
            .root()
            .map(|path| ns_ui::path_name(&path))
            .unwrap_or_default();
        scope_text(&name, &self.board_cache.counts)
    }

    fn message(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.push_notice(Severity::Info, text.into(), cx);
    }

    fn warning(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.push_notice(Severity::Warning, text.into(), cx);
    }

    /// A passing result is a toast, not a sheet that blocks the window.
    fn push_notice(&mut self, severity: Severity, text: String, cx: &mut Context<Self>) {
        ns_ui::notify(&self.toaster, severity, &text, cx);
    }

    /// Bring the next waiting confirmation on screen. One at a time: a
    /// second dialog would stack its scrim over the first.
    fn next_dialog(&mut self) {
        if self.dialog.is_none() {
            self.dialog = self.dialogs.pop_front();
            if self.dialog.is_some() {
                self.answered = None;
            }
        }
    }

    fn dismiss_dialog(&mut self, cx: &mut Context<Self>) {
        if let Some(request) = self.dialog.take() {
            self.answered = Some(request.pending);
        }
        cx.notify();
    }

    fn accept_dialog(&mut self, cx: &mut Context<Self>) {
        self.pending = self.answered.take();
        self.confirm(cx);
        cx.notify();
    }

    /// The confirmation as an Ely dialog, or nothing while none is open.
    fn dialog_overlay(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        self.next_dialog();
        let request = self.dialog.as_ref()?;
        let entity = cx.entity();
        let closer = entity.clone();
        let mut dialog = ConfirmDialog::new(
            "hearth-confirm",
            request.title.clone(),
            request.message.clone(),
            move |_, cx| {
                closer.update(cx, |this, cx| this.dismiss_dialog(cx));
            },
        )
        .confirm(request.confirm.clone())
        .on_confirm(move |_, cx| {
            entity.update(cx, |this, cx| this.accept_dialog(cx));
        });
        if request.destructive {
            dialog = dialog.destructive();
        }
        Some(dialog.into_any_element())
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_after(None, cx);
    }

    /// `finished` is the lifecycle command this read follows. That mark stays up
    /// until this read, or a newer one, paints. A read started for another reason
    /// passes `None` and leaves commands that are still running marked.
    fn reload_after(&mut self, finished: Option<u64>, cx: &mut Context<Self>) {
        self.workspaces = store::load(&self.file).unwrap_or_else(|error| {
            self.warning(error, cx);
            self.workspaces.clone()
        });
        if self
            .selected
            .is_some_and(|index| index >= self.workspaces.len())
        {
            self.selected = if self.workspaces.is_empty() {
                None
            } else {
                Some(0)
            };
        }
        self.sync_watches(cx);
        if self.pane == Pane::Shared {
            self.load_shared(cx);
        }
        let Some(path) = self.root() else {
            self.clear_board();
            self.board_pending = false;
            self.inflight.retain(|job| job.shared);
            self.busy = false;
            self.note = "Select a workspace.".into();
            cx.notify();
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        if let Some(id) = finished
            && let Some(job) = self
                .inflight
                .iter_mut()
                .find(|job| job.id == id && !job.shared)
        {
            job.clear_after = Some(generation);
        }
        let previous_stamp = self.catalog_stamps.previous(&path);
        let root_key = path.clone();
        self.busy = true;
        // Keep "Starting shop…" on screen until this read replaces those rows.
        // Another group's mark stays put.
        if !board_has_job(&self.inflight, false, Some(path.as_str())) {
            self.note = format!("Reading {path}");
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            let board = cx
                .background_spawn(async move {
                    let mut board = load_board(&path);
                    if board.daemon_up && should_reload_catalog(previous_stamp, board.stamp) {
                        let reloaded = hearth(Some(&path), &["manager", "reload"]);
                        board = load_board(&path);
                        if !reloaded.ok && board.note.is_empty() {
                            board.note = reloaded.text();
                        }
                    }
                    board
                })
                .await;
            this.update(cx, |this, cx| {
                // A newer switch owns the pending flag. This result is stale.
                if this.generation != generation {
                    return;
                }
                this.catalog_stamps.record(&root_key, board.stamp);
                let note = board.note.clone();
                this.boards.insert(root_key.clone(), board.clone());
                this.apply_board(board);
                // Drop marks whose command already finished. A command that is
                // still running keeps its row.
                clear_applied(&mut this.inflight, generation, false);
                if this
                    .selected_service
                    .is_some_and(|index| index >= this.services.len())
                {
                    this.selected_service = None;
                    this.clear_log();
                } else if this.selected_service.is_some() {
                    this.refresh_log(cx);
                }
                if this.status_dirty && this.pane == Pane::Workspaces {
                    this.status_dirty = false;
                    this.reload(cx);
                }
                if !note.trim().is_empty() {
                    this.warning(note.clone(), cx);
                }
                self_note(this, note);
                this.busy = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_shared(&mut self, cx: &mut Context<Self>) {
        self.load_shared_after(None, cx);
    }

    fn load_shared_after(&mut self, finished: Option<u64>, cx: &mut Context<Self>) {
        self.shared_generation = self.shared_generation.wrapping_add(1);
        let generation = self.shared_generation;
        if let Some(id) = finished
            && let Some(job) = self
                .inflight
                .iter_mut()
                .find(|job| job.id == id && job.shared)
        {
            job.clear_after = Some(generation);
        }
        self.busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let loaded = cx.background_spawn(async { load_shared_board() }).await;
            this.update(cx, |this, cx| {
                // A newer shared read owns the board. This result is stale.
                if this.shared_generation != generation {
                    return;
                }
                // The registry order can change. Keep the selection on the same
                // instance and recipe, not on a shifting index.
                let picked_instance = this
                    .shared_pick
                    .and_then(|index| this.instances.get(index))
                    .map(|item| item.id.clone());
                let picked_recipe = this
                    .recipe_pick
                    .and_then(|index| this.recipes.get(index))
                    .cloned();
                this.recipes = loaded.recipes;
                this.instances = loaded.instances;
                this.shared_cache_dirty = true;
                this.smp_up = loaded.smp_up;
                this.shared_pick = picked_instance
                    .and_then(|id| this.instances.iter().position(|item| item.id == id));
                this.recipe_pick = picked_recipe.and_then(|(name, version)| {
                    this.recipes
                        .iter()
                        .position(|item| item.0 == name && item.1 == version)
                });
                clear_applied(&mut this.inflight, generation, true);
                this.busy = false;
                this.note = format!(
                    "{} recipes · {} instances",
                    this.recipes.len(),
                    this.instances.len()
                );
                if this.log_shared {
                    let still_there = this
                        .shared_pick
                        .and_then(|index| this.instances.get(index))
                        .is_some_and(|instance| instance.id == this.log_service);
                    if still_there {
                        this.refresh_log(cx);
                    } else {
                        this.clear_log();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Switching panes swaps the dock: the workspace pane shows the selected
    /// service's log, the shared pane the selected instance's. The two logs come
    /// from different runtime directories, so the open one is dropped.
    fn set_pane(&mut self, pane: Pane, cx: &mut Context<Self>) {
        if self.pane == pane {
            return;
        }
        self.pane = pane;
        // The status bar describes the pane on screen, not a stale message.
        self.note.clear();
        self.clear_log();
        match pane {
            Pane::Workspaces => {
                self.sync_watches(cx);
                // Events that arrived while the shared pane was open were
                // recorded, not applied. Catch up once.
                if self.status_dirty {
                    self.status_dirty = false;
                    self.reload(cx);
                }
                if self.selected_service.is_some() {
                    self.open_log(cx);
                }
            }
            Pane::Shared => {
                self.load_shared(cx);
                if self.shared_pick.is_some() {
                    self.open_shared_log(cx);
                }
            }
        }
        cx.notify();
    }

    fn select_workspace(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.dialog_open() {
            return;
        }
        let previous = self.root();
        self.pane = Pane::Workspaces;
        self.selected = Some(index);
        self.selected_service = None;
        self.clear_log();
        // The same row is a refresh. A different folder must not keep the old
        // board, but its cached one paints immediately; the reload below
        // replaces it with authoritative state.
        if workspace_root_changed(previous.as_deref(), self.root().as_deref()) {
            self.status_dirty = false;
            match self.root().and_then(|root| self.boards.get(&root)).cloned() {
                Some(board) => self.apply_board(board),
                None => {
                    self.clear_board();
                    self.board_pending = true;
                }
            }
        }
        self.reload(cx);
    }

    /// Hide the previous workspace. `catalog_stamps` keeps one stamp per root,
    /// so switching folders never compares one folder's `hearth.yaml` mtime
    /// with another's and only a real edit to this folder reloads its daemon.
    fn clear_board(&mut self) {
        self.services.clear();
        self.urls.clear();
        self.catalog = Catalog::default();
        self.board_cache_dirty = true;
        self.daemon_up = false;
        self.port = None;
        self.daemon_note.clear();
    }

    /// Paint one read board onto the live fields. Used by the reload
    /// completion and by selecting a workspace whose board is cached.
    fn apply_board(&mut self, board: Board) {
        self.services = board.services;
        self.urls = board.urls;
        self.catalog = board.catalog;
        self.board_cache_dirty = true;
        self.daemon_up = board.daemon_up;
        self.port = board.port;
        self.daemon_note = board.daemon_note;
        self.board_pending = false;
    }

    fn service_id(&self) -> Option<String> {
        self.selected_service
            .and_then(|index| self.services.get(index))
            .map(|service| service.id.clone())
    }

    fn check_update(&mut self, cx: &mut Context<Self>) {
        self.busy = true;
        self.note = "Checking for updates…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let report = cx
                .background_spawn(async { hearth(None, &["update", "--check", "--json"]) })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                let offer = parse_update(&report.text());
                this.note = offer.text.clone();
                this.update = Some(offer);
                this.update_dismissed = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn install_update(&mut self, cx: &mut Context<Self>) {
        self.busy = true;
        self.note = "Installing update…".into();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let report = cx
                .background_spawn(async { hearth(None, &["update", "--json"]) })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                let offer = parse_update(&report.text());
                this.note = offer.text.clone();
                this.update = Some(offer);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn ask_for(
        &mut self,
        title: &str,
        message: &str,
        confirm: &str,
        _confirm_icon: &'static str,
        destructive: bool,
        pending: Pending,
        cx: &mut Context<Self>,
    ) {
        self.dialogs.push_back(DialogRequest {
            destructive,
            title: title.into(),
            message: message.trim().to_string(),
            confirm: confirm.into(),
            pending,
        });
        cx.notify();
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match pending {
            Pending::Trust => self.trust_and_start(cx),
            Pending::StopDaemon => self.run_daemon("stop", cx),
            Pending::RestartDaemon => self.run_daemon("restart", cx),
            Pending::Forget => self.forget(cx),
            Pending::Reclaim(id) => self.run_services("start", vec![id], true, cx),
            Pending::StopServices(ids) => self.run_services("stop", ids, false, cx),
            Pending::RestartServices(ids) => self.run_services("restart", ids, false, cx),
            Pending::InstallShared(id) => {
                let mut flight = inflight("start", &id, Vec::new(), false);
                flight.shared = true;
                flight.label = format!("Installing {id}…");
                self.run_shared_owned(
                    flight,
                    vec!["shared".into(), "install".into(), id, "--json".into()],
                    cx,
                );
            }
            Pending::StartShared(id) => {
                let mut flight = inflight("start", &id, Vec::new(), false);
                flight.shared = true;
                self.run_shared_owned(
                    flight,
                    vec!["shared".into(), "start".into(), id, "--json".into()],
                    cx,
                );
            }
            Pending::StopShared(id) => {
                let mut flight = inflight("stop", &id, Vec::new(), false);
                flight.shared = true;
                self.run_shared_owned(
                    flight,
                    vec!["shared".into(), "stop".into(), id, "--json".into()],
                    cx,
                );
            }
            Pending::InstallUpdate => self.install_update(cx),
            Pending::RestartShared(id) => {
                let id2 = id.clone();
                let mut flight = inflight("restart", &id, Vec::new(), false);
                flight.shared = true;
                self.run_job(
                    Some(flight),
                    move || {
                        let stop = hearth(None, &["shared", "stop", &id, "--json"]);
                        if !stop.ok {
                            return Report {
                                ok: false,
                                text: stop.text(),
                            };
                        }
                        let start = hearth(None, &["shared", "start", &id2, "--json"]);
                        Report {
                            ok: start.ok,
                            text: if start.ok {
                                String::new()
                            } else {
                                start.text()
                            },
                        }
                    },
                    cx,
                );
            }
            Pending::RemoveShared { id, force } => {
                let mut flight = inflight("stop", &id, Vec::new(), false);
                flight.shared = true;
                flight.label = format!("Removing {id}…");
                self.run_job(
                    Some(flight),
                    move || {
                        let args = shared_remove_args(&id, force);
                        let result = ns_ui::run_tool("hearth", &args);
                        Report {
                            ok: result.ok,
                            text: if result.ok {
                                String::new()
                            } else {
                                result.text()
                            },
                        }
                    },
                    cx,
                );
            }
        }
    }

    fn dialog_open(&self) -> bool {
        self.dialog.is_some() || !self.dialogs.is_empty()
    }

    /// The board on screen is the previous workspace, or a confirmation is up.
    fn workspace_locked(&self) -> bool {
        self.dialog_open() || self.board_pending
    }

    /// Daemon, trust, and "all" touch every service. A per-row command does not.
    fn workspace_bulk_locked(&self) -> bool {
        if self.workspace_locked() || self.busy {
            return true;
        }
        let root = self.root();
        board_has_job(&self.inflight, false, root.as_deref())
    }

    fn service_locked(&self, id: &str) -> bool {
        if self.workspace_locked() {
            return true;
        }
        let root = self.root();
        action_blocked(&self.inflight, &[id], false, root.as_deref())
    }

    fn group_locked(&self, name: &str, members: &[String]) -> bool {
        if self.workspace_locked() {
            return true;
        }
        let root = self.root();
        let mut targets = Vec::with_capacity(1 + members.len());
        targets.push(name);
        for member in members {
            targets.push(member.as_str());
        }
        action_blocked(&self.inflight, &targets, false, root.as_deref())
    }

    fn refresh_locked(&self) -> bool {
        self.dialog_open() || self.board_pending || self.busy
    }

    fn shared_target_locked(&self, id: &str) -> bool {
        self.dialog_open() || action_blocked(&self.inflight, &[id], true, None)
    }

    fn shared_board_locked(&self) -> bool {
        self.dialog_open() || board_has_job(&self.inflight, true, None)
    }

    fn request_trust(&mut self, cx: &mut Context<Self>) {
        if self.workspace_bulk_locked() {
            return;
        }
        let Some(row) = self.current() else {
            return;
        };
        if !Path::new(&row.path).is_dir() {
            self.warning("Folder does not exist.", cx);
            return;
        }
        if row.trusted {
            self.run_daemon("ensure", cx);
            return;
        }
        let name = ns_ui::path_name(&row.path);
        let path = ns_ui::display_path(&row.path);
        self.ask_for(
            &format!("Trust {name}?"),
            &format!("{path} will start a daemon that runs the commands in its hearth.yaml. Only trust a folder you wrote or reviewed."),
            "Trust and Start",
            "play",
            false,
            Pending::Trust,
            cx,
        );
    }

    fn trust_and_start(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.current().cloned() else {
            return;
        };
        if let Err(error) = store::trust(&self.file, &mut self.workspaces, &row.id) {
            self.warning(error, cx);
            return;
        }
        self.run_daemon("ensure", cx);
    }

    fn request_stop_daemon(&mut self, cx: &mut Context<Self>) {
        if self.workspace_bulk_locked() {
            return;
        }
        let Some(row) = self.current() else {
            return;
        };
        let name = ns_ui::path_name(&row.path);
        self.ask_for(
            &format!("Stop the daemon for {name}?"),
            "Its services stop. Start runs the daemon again.",
            "Stop Daemon",
            "stop",
            true,
            Pending::StopDaemon,
            cx,
        );
    }

    fn request_restart_daemon(&mut self, cx: &mut Context<Self>) {
        if self.workspace_bulk_locked() {
            return;
        }
        let Some(row) = self.current() else {
            return;
        };
        let name = ns_ui::path_name(&row.path);
        self.ask_for(
            &format!("Restart the daemon for {name}?"),
            "Its services keep running.",
            "Restart Daemon",
            "restart",
            false,
            Pending::RestartDaemon,
            cx,
        );
    }

    fn request_forget(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.current() else {
            return;
        };
        let name = ns_ui::path_name(&row.path);
        self.ask_for(
            &format!("Forget {name}?"),
            "The folder leaves this list. Its services keep running.",
            "Forget",
            "forget",
            true,
            Pending::Forget,
            cx,
        );
    }

    fn forget(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.current().cloned() else {
            return;
        };
        if let Err(error) = store::remove(&self.file, &mut self.workspaces, &row.id) {
            self.warning(error, cx);
            return;
        }
        let name = ns_ui::path_name(&row.path);
        // `row` is the folder that was on screen. After remove, the same index
        // can already point at the next folder, so compare against `row.path`.
        let previous = row.path.clone();
        self.selected = if self.workspaces.is_empty() {
            None
        } else {
            Some(0)
        };
        if workspace_root_changed(Some(previous.as_str()), self.root().as_deref()) {
            self.clear_board();
            self.selected_service = None;
            self.clear_log();
            self.board_pending = self.root().is_some();
        }
        self.message(format!("Forgot {name}. Its services keep running."), cx);
        self.reload(cx);
    }

    fn run_daemon(&mut self, verb: &'static str, cx: &mut Context<Self>) {
        let Some(root) = self.root() else {
            return;
        };
        // Stop takes the services down with the daemon. Restart leaves them running.
        let ids = if verb == "stop" {
            stopping_service_ids(&self.services, &self.catalog.disabled)
        } else {
            Vec::new()
        };
        let flight = inflight(verb, "daemon", ids, true);
        self.run_job(
            Some(flight),
            move || {
                let args = manager_args(verb);
                let result = hearth(Some(&root), &args);
                Report {
                    ok: result.ok,
                    text: if result.ok {
                        String::new()
                    } else {
                        result.text()
                    },
                }
            },
            cx,
        );
    }

    fn request_service(&mut self, verb: &'static str, id: String, cx: &mut Context<Self>) {
        if self.service_locked(&id) {
            return;
        }
        if verb == "start" {
            if self
                .services
                .iter()
                .any(|service| service.id == id && service.state == "externally-owned")
            {
                self.ask_for(
                    &format!("Reclaim the port for {id}?"),
                    "Another process holds this service's port. Reclaiming signals that process (SIGTERM, then SIGKILL) and starts it.",
                    "Kill and Start",
                    "play",
                    true,
                    Pending::Reclaim(id),
                    cx,
                );
            } else {
                self.run_services("start", vec![id], false, cx);
            }
            return;
        }
        self.confirm_services(verb, vec![id.clone()], id, cx);
    }

    fn request_all(&mut self, verb: &'static str, cx: &mut Context<Self>) {
        if self.workspace_bulk_locked() {
            return;
        }
        let ids: Vec<String> = if verb == "start" {
            self.services
                .iter()
                .filter(|service| !self.is_disabled(&service.id))
                .map(|service| service.id.clone())
                .collect()
        } else {
            self.services
                .iter()
                .filter(|service| {
                    !self.is_disabled(&service.id)
                        && service.state != "stopped"
                        && service.state != "succeeded"
                })
                .map(|service| service.id.clone())
                .collect()
        };
        if ids.is_empty() {
            self.message(
                if verb == "stop" {
                    "Nothing to stop."
                } else {
                    "Nothing to start."
                },
                cx,
            );
            return;
        }
        if verb == "start" {
            self.run_services("start", ids, false, cx);
        } else {
            self.confirm_services(verb, ids, "all services".into(), cx);
        }
    }

    fn request_group(&mut self, verb: &'static str, name: String, cx: &mut Context<Self>) {
        let members = expand(&name, &self.catalog.groups);
        if self.group_locked(&name, &members) {
            return;
        }
        let ids = vec![name.clone()];
        if verb == "start" {
            self.run_services("start", ids, false, cx);
        } else {
            self.confirm_services(verb, ids, name, cx);
        }
    }

    fn confirm_services(
        &mut self,
        verb: &'static str,
        ids: Vec<String>,
        label: String,
        cx: &mut Context<Self>,
    ) {
        let expanded: Vec<String> = ids
            .iter()
            .flat_map(|id| {
                if self.catalog.groups.iter().any(|(name, _)| name == id) {
                    expand(id, &self.catalog.groups)
                } else {
                    vec![id.clone()]
                }
            })
            .collect();
        if touches_shared(&expanded, &self.catalog.shared) {
            let title = format!(
                "{} {label}?",
                if verb == "stop" { "Stop" } else { "Restart" }
            );
            let message =
                "This reaches a shared service. Other workspaces attached to it are affected.";
            let pending = if verb == "stop" {
                Pending::StopServices(ids)
            } else {
                Pending::RestartServices(ids)
            };
            self.ask_for(
                &title,
                message,
                if verb == "stop" { "Stop" } else { "Restart" },
                if verb == "stop" { "stop" } else { "restart" },
                verb == "stop",
                pending,
                cx,
            );
        } else if verb == "stop" {
            self.run_services("stop", ids, false, cx);
        } else {
            self.run_services("restart", ids, false, cx);
        }
    }

    fn is_disabled(&self, id: &str) -> bool {
        self.catalog.disabled.iter().any(|item| item == id)
    }

    fn run_services(
        &mut self,
        verb: &'static str,
        ids: Vec<String>,
        kill_unowned: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.root() else {
            return;
        };
        let target = action_target(&ids);
        let marked = affected_ids(
            &ids,
            &self.catalog.groups,
            &self.catalog.disabled,
            &self.services,
        );
        let flight = inflight(verb, &target, marked, false);
        self.run_job(
            Some(flight),
            move || run_targets(&root, verb, &ids, kill_unowned),
            cx,
        );
    }

    fn run_shared_owned(&mut self, flight: InFlight, args: Vec<String>, cx: &mut Context<Self>) {
        self.run_job(
            Some(flight),
            move || {
                let result = ns_ui::run_tool("hearth", &args);
                Report {
                    ok: result.ok,
                    text: result.text(),
                }
            },
            cx,
        );
    }

    /// Explicit start of smp. Instances themselves still start when a workspace needs them.
    fn ensure_shared(&mut self, cx: &mut Context<Self>) {
        if self.shared_board_locked() {
            return;
        }
        let mut flight = inflight("start", "shared daemon", Vec::new(), true);
        flight.shared = true;
        flight.label = "Starting shared daemon…".into();
        self.run_shared_owned(
            flight,
            vec!["shared".into(), "ensure".into(), "--json".into()],
            cx,
        );
    }

    /// Jump from an attachment to that workspace's board.
    fn show_attached_workspace(&mut self, root: &str, cx: &mut Context<Self>) {
        if self.dialog_open() {
            return;
        }
        let Some(index) = find_workspace(&self.workspaces, root) else {
            return;
        };
        if self.pane != Pane::Workspaces {
            self.set_pane(Pane::Workspaces, cx);
        }
        self.select_workspace(index, cx);
    }

    fn run_job(
        &mut self,
        flight: Option<InFlight>,
        job: impl FnOnce() -> Report + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let shared;
        let root;
        let job_id;
        if let Some(mut item) = flight {
            item.root = self.root();
            if flight_conflicts(&self.inflight, &item) {
                return;
            }
            item.id = self.next_job;
            self.next_job += 1;
            job_id = Some(item.id);
            shared = item.shared;
            root = item.root.clone();
            self.note = item.label.clone();
            self.inflight.push(item);
        } else {
            job_id = None;
            shared = false;
            root = self.root();
            self.note = "Working".into();
        }
        // A per-target command does not take the global busy flag. Install,
        // update, and the board read still do. The row stays marked until the
        // reload started below paints, and only that mark is cleared.
        cx.notify();
        cx.spawn(async move |this, cx| {
            let report = cx.background_spawn(async move { job() }).await;
            this.update(cx, |this, cx| {
                // A workspace switch bumped the board out from under this
                // command. Drop its mark and do not paint or reload the new one.
                // The reload below still checks `generation` before it applies.
                if !shared && this.root() != root {
                    if let Some(id) = job_id {
                        remove_job(&mut this.inflight, id);
                    }
                    cx.notify();
                    return;
                }
                if let Some(text) = failure_notice(&report) {
                    this.warning(text, cx);
                }
                if shared {
                    this.load_shared_after(job_id, cx);
                } else {
                    this.reload_after(job_id, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn clear_log(&mut self) {
        self.log_lines = Rc::new(Vec::new());
        self.log_paints = Rc::new(Vec::new());
        self.rebuild_log_rows();
        self.log_origin = 0;
        self.log_end = 0;
        self.log_has_more = false;
        self.log_loading = false;
        self.log_refreshing = false;
        self.log_service.clear();
        self.log_shared = false;
        self.log_follow = true;
        self.log_sel = None;
        self.log_selecting = false;
        self.log_epoch = self.log_epoch.wrapping_add(1);
        self.log_watch = self.log_watch.wrapping_add(1);
    }

    fn bump_epoch(&mut self) -> u64 {
        self.log_epoch = self.log_epoch.wrapping_add(1);
        self.log_loading = true;
        self.log_epoch
    }

    fn bump_quiet(&mut self) -> u64 {
        self.log_epoch = self.log_epoch.wrapping_add(1);
        self.log_epoch
    }

    fn open_log(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root() else {
            return;
        };
        let Some(id) = self.service_id() else {
            self.clear_log();
            cx.notify();
            return;
        };
        self.log_service = id.clone();
        self.log_dir = logbuf::runtime_directory(Path::new(&root));
        self.log_shared = false;
        self.log_follow = true;
        self.log_sel = None;
        self.log_selecting = false;
        self.log_refreshing = false;
        let epoch = self.bump_epoch();
        self.spawn_log(id, epoch, LogKind::Tail, cx);
        self.watch_log(cx);
    }

    /// The same pane for a shared instance. smp keeps its logs in its own
    /// runtime directory, so the file is resolved from there instead of from a
    /// workspace root.
    fn open_shared_log(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.shared_pick else {
            self.clear_log();
            cx.notify();
            return;
        };
        let Some(instance) = self.instances.get(index) else {
            self.clear_log();
            cx.notify();
            return;
        };
        let id = instance.id.clone();
        self.log_service = id.clone();
        self.log_dir = logbuf::shared_runtime_directory();
        self.log_shared = true;
        self.log_follow = true;
        self.log_sel = None;
        self.log_selecting = false;
        self.log_refreshing = false;
        let epoch = self.bump_epoch();
        self.spawn_log(id, epoch, LogKind::Tail, cx);
        self.watch_log(cx);
    }

    fn refresh_log(&mut self, cx: &mut Context<Self>) {
        let id = if self.log_shared {
            self.log_service.clone()
        } else {
            match self.service_id() {
                Some(id) => id,
                None => return,
            }
        };
        if id.is_empty() {
            return;
        }
        if self.log_service != id || (self.log_end == 0 && self.log_lines.is_empty()) {
            if self.log_shared {
                self.open_shared_log(cx);
            } else {
                self.open_log(cx);
            }
            return;
        }
        if self.log_loading || self.log_refreshing {
            return;
        }
        let origin = self.log_origin;
        let end = self.log_end;
        let epoch = self.bump_quiet();
        self.log_refreshing = true;
        self.spawn_log(id, epoch, LogKind::Refresh { origin, end }, cx);
    }

    fn load_earlier(&mut self, cx: &mut Context<Self>) {
        if self.log_loading || !self.log_has_more || self.log_origin == 0 {
            return;
        }
        let id = self.log_service.clone();
        if id.is_empty() {
            return;
        }
        let origin = self.log_origin;
        let end = self.log_end;
        self.log_refreshing = false;
        let epoch = self.bump_epoch();
        self.spawn_log(id, epoch, LogKind::Earlier { origin, end }, cx);
    }

    /// Keep one event watcher per listed workspace. Called after the store
    /// re-read and on pane changes; roots that left the list drop their watch
    /// and their cached board.
    fn sync_watches(&mut self, cx: &mut Context<Self>) {
        let listed: HashSet<String> = self.workspaces.iter().map(|row| row.path.clone()).collect();
        self.watchers.retain(|root, _| listed.contains(root));
        self.watch_ids.retain(|root, _| listed.contains(root));
        self.boards.retain(|root, _| listed.contains(root));
        self.board_loading.retain(|root| listed.contains(root));
        self.board_dirty.retain(|root| listed.contains(root));
        for root in listed {
            if !self.watchers.contains_key(&root) {
                self.watch_root(root, cx);
            }
        }
    }

    fn watch_root(&mut self, root: String, cx: &mut Context<Self>) {
        let watch = self.next_watch;
        self.next_watch += 1;
        self.watch_ids.insert(root.clone(), watch);
        let (sender, receiver) = mpsc::channel();
        self.watchers
            .insert(root.clone(), DaemonWatch::spawn(root.clone(), sender));
        cx.spawn(async move |this, cx| {
            loop {
                #[cfg(feature = "devtools")]
                if perf::driving() {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                    continue;
                }
                match receiver.try_recv() {
                    Ok(()) => {
                        let keep = this.update(cx, |this, cx| {
                            if this.watch_ids.get(&root).copied() != Some(watch) {
                                return false;
                            }
                            this.invalidate_root(root.clone(), cx);
                            true
                        });
                        if !keep.unwrap_or(false) {
                            break;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        cx.background_spawn(async { thread::sleep(Duration::from_millis(100)) })
                            .await;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
        })
        .detach();
    }

    /// One watcher event. The selected workspace reloads its board through the
    /// usual path; every other root refreshes its sidebar cache in the
    /// background and never touches its daemon.
    fn invalidate_root(&mut self, root: String, cx: &mut Context<Self>) {
        let selected = self
            .root()
            .is_some_and(|current| root_key(&current) == root_key(&root));
        if selected {
            self.status_dirty = true;
            if !self.busy && !self.board_pending && self.pane == Pane::Workspaces {
                self.status_dirty = false;
                self.reload(cx);
            }
        } else {
            self.refresh_board(root, cx);
        }
    }

    /// Re-read one non-selected workspace into the sidebar cache. One read per
    /// root at a time; events that arrive during it queue exactly one more.
    fn refresh_board(&mut self, root: String, cx: &mut Context<Self>) {
        if self.board_loading.contains(&root) {
            self.board_dirty.insert(root);
            return;
        }
        self.board_loading.insert(root.clone());
        cx.spawn(async move |this, cx| {
            let read_root = root.clone();
            let board = cx
                .background_spawn(async move { load_board(&read_root) })
                .await;
            this.update(cx, |this, cx| {
                this.board_loading.remove(&root);
                if !this.watchers.contains_key(&root) {
                    return;
                }
                this.boards.insert(root.clone(), board);
                if this.board_dirty.remove(&root) {
                    this.refresh_board(root, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-read the log while it is open. A newer open or a cleared pane bumps
    /// `log_watch`, and this loop exits on the next tick.
    fn watch_log(&mut self, cx: &mut Context<Self>) {
        self.log_watch = self.log_watch.wrapping_add(1);
        let watch = self.log_watch;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_spawn(async {
                    std::thread::sleep(std::time::Duration::from_millis(500))
                })
                .await;
                let keep = this.update(cx, |this, cx| {
                    #[cfg(feature = "devtools")]
                    if perf::driving() {
                        return true;
                    }
                    if this.log_watch != watch || this.log_service.is_empty() {
                        return false;
                    }
                    if !this.log_loading && !this.log_refreshing {
                        this.refresh_log(cx);
                    }
                    true
                });
                if !keep.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }

    fn spawn_log(&mut self, id: String, epoch: u64, kind: LogKind, cx: &mut Context<Self>) {
        let dir = self.log_dir.clone();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move {
                    let Some(path) = logbuf::service_log_path(&dir, &id) else {
                        return LogFetch::Failed("No log yet.".into());
                    };
                    match kind {
                        LogKind::Tail => logbuf::fetch_tail(&path, logbuf::LOG_PAGE),
                        LogKind::Earlier { origin, end } => {
                            logbuf::fetch_earlier(&path, logbuf::LOG_PAGE, origin, end)
                        }
                        LogKind::Refresh { origin, end } => {
                            logbuf::fetch_refresh(&path, logbuf::LOG_PAGE, origin, end)
                        }
                    }
                })
                .await;
            this.update(cx, |this, cx| this.finish_log(epoch, fetched, cx))
                .ok();
        })
        .detach();
    }

    fn finish_log(&mut self, epoch: u64, fetched: LogFetch, cx: &mut Context<Self>) {
        if self.log_epoch != epoch {
            return;
        }
        self.log_refreshing = false;
        match fetched {
            LogFetch::Same => {
                self.log_loading = false;
            }
            LogFetch::Exhausted => {
                self.log_loading = false;
                self.log_has_more = false;
                cx.notify();
            }
            LogFetch::Failed(message) => {
                self.log_loading = false;
                if self.log_lines.is_empty() {
                    self.log_lines = Rc::new(vec![message]);
                    self.log_paints = Rc::new(vec![Vec::new()]);
                    self.rebuild_log_rows();
                    self.log_has_more = false;
                    self.log_origin = 0;
                    self.log_end = 0;
                    cx.notify();
                }
            }
            LogFetch::Page(page) => self.apply_page(page, cx),
        }
    }

    fn apply_page(&mut self, page: LogPage, cx: &mut Context<Self>) {
        let metrics = self.bar_metrics();
        let top = self.log_top_index();
        let inserted = page.inserted;
        let dropped = page.dropped;
        let reset = page.reset;
        let at_tail = logbuf::sticks_to_tail(metrics.max_scroll, metrics.scroll);
        // Rows, not lines, are what scroll offsets count. The dropped lines are
        // measured against the rows they covered before the swap.
        let dropped_rows = self.log_rows.rows_before(dropped);
        self.log_lines = Rc::new(page.lines);
        self.log_paints = Rc::new(page.paints);
        self.rebuild_log_rows();
        let count = self.log_rows.len();
        let inserted_rows = self.log_rows.rows_before(inserted);
        self.log_origin = page.origin;
        self.log_end = page.end;
        self.log_has_more = page.has_more;
        self.log_loading = false;
        self.retarget_selection(inserted, dropped, reset);
        let pin = reset || (!self.log_selecting && at_tail);
        if pin {
            self.pin_bottom();
        } else if dropped > 0 {
            let scroll = (metrics.scroll - dropped_rows as f32 * logview::ROW_H).max(0.);
            self.assign_scroll(scroll);
            if !self.log_selecting {
                let placed = self.bar_metrics();
                self.log_follow = logbuf::sticks_to_tail(placed.max_scroll, placed.scroll);
            }
        } else if inserted > 0 && count > 0 {
            let index = (top + inserted_rows).min(count - 1);
            self.scroll_item_now(index, ScrollStrategy::Top);
            self.log_follow = false;
        } else if !self.log_selecting && !at_tail {
            self.log_follow = false;
        }
        cx.notify();
    }

    /// Cut the loaded lines into display rows for the current pane width and
    /// find the level each line carries.
    fn rebuild_log_rows(&mut self) {
        self.log_rows = Rc::new(logview::wrap(&self.log_lines, self.log_cols));
        self.log_levels = Rc::new(logview::levels(&self.log_lines));
    }

    /// Re-wrap when the pane width changed since the last frame. A pane that
    /// follows the tail stays on it; otherwise the top line stays on top.
    fn fit_log(&mut self, window: &gpui::Window) {
        let width = self.log_view_bounds.get().size.width.as_f32();
        if width < 80. {
            return;
        }
        if self.log_char_w <= 0.5 {
            self.log_char_w = log_char_advance(window);
        }
        let usable = width - LOG_PAD - LOG_GUTTER;
        let cols = ((usable / self.log_char_w).floor() as usize).max(8);
        if cols == self.log_cols {
            return;
        }
        let top_line = self.log_rows.line_of(self.log_top_index());
        self.log_cols = cols;
        self.rebuild_log_rows();
        if self.log_follow {
            self.pin_bottom();
        } else if let Some(line) = top_line {
            self.scroll_item_now(self.log_rows.first_row(line), ScrollStrategy::Top);
        }
    }

    fn retarget_selection(&mut self, inserted: usize, dropped: usize, reset: bool) {
        if reset {
            self.log_sel = None;
            self.log_selecting = false;
            return;
        }
        let Some(selection) = self.log_sel else {
            return;
        };
        let count = self.log_lines.len();
        let shift = inserted as isize - dropped as isize;
        let map = |line: usize| {
            let next = line as isize + shift;
            (next >= 0).then_some(next as usize)
        };
        let (Some(anchor), Some(head)) = (map(selection.anchor_line), map(selection.head_line))
        else {
            self.log_sel = None;
            return;
        };
        if anchor >= count || head >= count {
            self.log_sel = None;
            return;
        }
        self.log_sel = Some(logbuf::LogSelection {
            anchor_line: anchor,
            anchor_col: selection
                .anchor_col
                .min(self.log_lines[anchor].chars().count()),
            head_line: head,
            head_col: selection.head_col.min(self.log_lines[head].chars().count()),
        });
    }

    fn pin_bottom(&mut self) {
        let Some(last) = self.log_rows.len().checked_sub(1) else {
            return;
        };
        self.log_follow = true;
        self.scroll_item_now(last, ScrollStrategy::Bottom);
    }

    /// Lands a scroll_to_item intent by setting the offset now, from the
    /// projection bar_metrics makes for a pending deferred scroll. The overlay
    /// Scrollbar reads the handle during the layout phase — one phase before
    /// prepaint applies a deferred scroll — so a jump left as deferred draws
    /// the thumb at the old position until some later repaint moves it.
    fn scroll_item_now(&mut self, index: usize, strategy: ScrollStrategy) {
        self.log_scroll.scroll_to_item_strict(index, strategy);
        let target = self.bar_metrics().scroll;
        self.assign_scroll(target);
    }

    fn log_top_index(&self) -> usize {
        let count = self.log_rows.len();
        if count == 0 {
            return 0;
        }
        let state = self.log_scroll.0.borrow();
        let Some(size) = state.last_item_size else {
            return 0;
        };
        let item_h = size.contents.height.as_f32() / count as f32;
        if item_h <= 0.5 {
            return 0;
        }
        let scroll = (-state.base_handle.offset().y).as_f32();
        ((scroll / item_h).floor() as usize).min(count - 1)
    }

    /// Pull the previous page once the user is at the top of a list that
    /// already has somewhere to scroll. The first paint stays pinned to the
    /// newest line, so this does not run until they leave that edge.
    fn sync_log_scroll(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "devtools")]
        if perf::driving() {
            return;
        }
        if self.log_loading || self.log_lines.is_empty() || self.selected_service.is_none() {
            return;
        }
        let metrics = self.bar_metrics();
        let pending = self.log_scroll.0.borrow().deferred_scroll_to_item.is_some();
        if pending || !metrics.scrollable {
            return;
        }
        let at_end = logbuf::sticks_to_tail(metrics.max_scroll, metrics.scroll);
        // A thumb drag moves the handle without a wheel event. Reading it here
        // is what lets that drag leave the tail.
        if self.log_follow {
            if !at_end {
                self.log_follow = false;
            } else {
                return;
            }
        } else if at_end {
            self.log_follow = true;
            return;
        }
        if !self.log_has_more || self.log_origin == 0 || metrics.scroll > 48. {
            return;
        }
        self.load_earlier(cx);
    }

    fn bar_metrics(&self) -> logbuf::Thumb {
        let state = self.log_scroll.0.borrow();
        let raw_scroll = (-state.base_handle.offset().y).as_f32();
        let bounds_h = state.base_handle.bounds().size.height.as_f32();
        let max_off = state.base_handle.max_offset().y.as_f32();
        let recorded = state
            .last_item_size
            .map(|size| (size.item.height.as_f32(), size.contents.height.as_f32()));
        let lines = self.log_rows.len();
        let viewport = recorded
            .map(|(item, _)| item)
            .filter(|item| *item > 1.)
            .or_else(|| (bounds_h > 1.).then_some(bounds_h))
            .unwrap_or_else(|| log_viewport(self));
        let estimated = lines as f32 * logview::ROW_H;
        // last_item_size is from the previous paint. After a page is prepended
        // it still describes the old line count, so prefer the current estimate
        // when the two disagree by more than one row.
        let measured = recorded
            .map(|(_, contents)| contents)
            .filter(|contents| *contents > viewport + 0.5)
            .or_else(|| (max_off > 0.5).then_some(viewport + max_off));
        let content = match measured {
            Some(contents) if (contents - estimated).abs() <= 24. => contents,
            _ => estimated,
        };
        let item_h = if lines > 0 {
            content / lines as f32
        } else {
            logview::ROW_H
        };
        // scroll_to_item is applied in prepaint, after this render. Paint the
        // thumb where that scroll will land, or the handle still reads 0 and
        // the thumb sits on the top of a list that is showing the newest line.
        let scroll = match state.deferred_scroll_to_item {
            Some(deferred) if lines > 0 && item_h > 0.5 => {
                let index = deferred.item_index.min(lines - 1) as f32;
                let item_top = index * item_h;
                let offset_px = deferred.offset as f32 * item_h;
                let max_scroll = (content - viewport).max(0.);
                let target = match deferred.strategy {
                    ScrollStrategy::Top => item_top - offset_px,
                    ScrollStrategy::Bottom => item_top + item_h - viewport,
                    ScrollStrategy::Center => {
                        let item_center = item_top + item_h / 2.;
                        let shrunk = (viewport - offset_px).max(0.);
                        item_center - (offset_px + shrunk / 2.)
                    }
                    ScrollStrategy::Nearest => raw_scroll,
                };
                if target.is_finite() {
                    target.clamp(0., max_scroll)
                } else {
                    raw_scroll
                }
            }
            _ => raw_scroll,
        };
        logbuf::thumb_metrics(viewport, content, scroll)
    }

    /// A wheel or a thumb drag moved the pane. Stay pinned only while that
    /// scroll lands on the newest line; leaving the tail is what loads earlier
    /// pages.
    fn note_log_scroll(&mut self) {
        let metrics = self.bar_metrics();
        if !metrics.scrollable {
            return;
        }
        self.log_follow = logbuf::sticks_to_tail(metrics.max_scroll, metrics.scroll);
    }

    fn assign_scroll(&mut self, scroll: f32) {
        let max = self.bar_metrics().max_scroll;
        let scroll = if scroll.is_finite() {
            scroll.clamp(0., max.max(0.))
        } else {
            0.
        };
        let mut state = self.log_scroll.0.borrow_mut();
        state.deferred_scroll_to_item = None;
        state.base_handle.set_offset(point(px(0.), px(-scroll)));
    }

    #[allow(clippy::too_many_arguments)]
    fn log_hit(
        &self,
        x: Pixels,
        y: Pixels,
        origin_x: Pixels,
        origin_y: Pixels,
        height: f32,
        pad: f32,
        char_w: f32,
    ) -> (usize, usize) {
        let count = self.log_rows.len();
        let Some(last_line) = self.log_lines.len().checked_sub(1) else {
            return (0, 0);
        };
        if count == 0 {
            return (0, 0);
        }
        let local_x = (x - origin_x).as_f32();
        let local_y = (y - origin_y).as_f32();
        if local_y < 0. {
            return (0, 0);
        }
        let scroll = self.bar_metrics().scroll;
        let end = (last_line, self.log_lines[last_line].chars().count());
        if local_y >= height {
            return end;
        }
        let at = ((scroll + local_y) / logview::ROW_H).floor();
        if at < 0. {
            return (0, 0);
        }
        let Some(row) = self.log_rows.rows.get(at as usize) else {
            return end;
        };
        let line = &self.log_lines[row.line];
        let indent = if row.cont {
            logview::CONTINUATION_CELLS as f32 * char_w
        } else {
            0.
        };
        let cell = if char_w <= 0.5 {
            0.
        } else {
            ((local_x - pad - indent) / char_w).max(0.)
        };
        (row.line, logview::char_at_cell(line, row, cell))
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_log_selection(
        &mut self,
        x: Pixels,
        y: Pixels,
        origin_x: Pixels,
        origin_y: Pixels,
        height: f32,
        pad: f32,
        char_w: f32,
        cx: &mut Context<Self>,
    ) {
        if self.log_lines.is_empty() {
            return;
        }
        if self.log_char_w <= 0.5 && char_w > 0.5 {
            self.log_char_w = char_w;
        }
        let (line, col) = self.log_hit(x, y, origin_x, origin_y, height, pad, self.log_char_w);
        self.log_selecting = true;
        self.log_sel = Some(logbuf::LogSelection {
            anchor_line: line,
            anchor_col: col,
            head_line: line,
            head_col: col,
        });
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn extend_log_selection(
        &mut self,
        x: Pixels,
        y: Pixels,
        origin_x: Pixels,
        origin_y: Pixels,
        height: f32,
        pad: f32,
        cx: &mut Context<Self>,
    ) {
        if !self.log_selecting {
            return;
        }
        let (line, col) = self.log_hit(x, y, origin_x, origin_y, height, pad, self.log_char_w);
        let Some(selection) = self.log_sel.as_mut() else {
            return;
        };
        if selection.head_line == line && selection.head_col == col {
            return;
        }
        selection.head_line = line;
        selection.head_col = col;
        cx.notify();
    }

    fn end_log_selection(&mut self, cx: &mut Context<Self>) {
        if !self.log_selecting {
            return;
        }
        self.log_selecting = false;
        if self.log_sel.is_some_and(|selection| selection.is_caret()) {
            self.log_sel = None;
        }
        cx.notify();
    }

    fn copy_log_selection(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if event.keystroke.key != "c" || event.keystroke.modifiers != gpui::Modifiers::command() {
            return;
        }
        let Some(selection) = self.log_sel.filter(|selection| !selection.is_caret()) else {
            return;
        };
        let text = logbuf::selected_text(&self.log_lines, selection);
        if text.is_empty() {
            return;
        }
        if ns_ui::copy_text(&text) {
            self.message("Copied the selection.", cx);
        } else {
            self.warning("Could not copy the log.", cx);
        }
        cx.stop_propagation();
    }

    fn commit_add(&mut self, cx: &mut Context<Self>) {
        match store::add(&self.file, &mut self.workspaces, &self.draft) {
            Ok((row, created)) => {
                self.draft.clear();
                self.pane = Pane::Workspaces;
                let previous = self.root();
                self.selected = self.workspaces.iter().position(|item| item.id == row.id);
                if workspace_root_changed(previous.as_deref(), self.root().as_deref()) {
                    self.clear_board();
                    self.selected_service = None;
                    self.clear_log();
                    self.board_pending = true;
                }
                if created {
                    self.message(
                        format!(
                            "Added {}. Trust it to start its daemon.",
                            ns_ui::path_name(&row.path)
                        ),
                        cx,
                    );
                }
                self.reload(cx);
            }
            Err(error) => self.warning(error, cx),
        }
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add Workspace".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |this, cx| {
                this.draft = path.display().to_string();
                this.commit_add(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Tabs stay pinned, the list scrolls, and Add Folder… sits in the footer
    /// so it does not scroll away with the rows.
    fn sidebar_column(&self, window: &mut gpui::Window, cx: &mut Context<Self>) -> gpui::Div {
        let pane = self.pane;
        let focused = self.list_focus.is_focused(window);
        let tabs = div().flex_shrink_0().w_full().child(tab_strip(
            "pane-tabs",
            [
                ("workspaces", "folder", "Workspaces"),
                ("shared", "server", "Shared"),
            ],
            if pane == Pane::Workspaces {
                "workspaces"
            } else {
                "shared"
            },
            cx.listener(|this, value: &SharedString, _, cx| {
                this.set_pane(
                    if value.as_ref() == "workspaces" {
                        Pane::Workspaces
                    } else {
                        Pane::Shared
                    },
                    cx,
                );
            }),
        ));
        let mut list = div().flex().flex_col().w_full();
        if pane == Pane::Workspaces {
            list = self.workspace_rows(list, focused, cx);
        } else {
            list = self.shared_rows(list, focused, cx);
        }
        let mut column = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(tabs)
            .child(sidebar_keys(
                "hearth-sidebar",
                &self.list_focus,
                sidebar_scroll_tracked(&self.list_scroll, list),
                cx.listener(|this, key: &SidebarKey, _, cx| this.on_sidebar_key(*key, cx)),
            ));
        if pane == Pane::Workspaces {
            column = column.child(sidebar_actions().primary(sidebar_button(
                "add-folder",
                "folder-plus",
                "Add Folder…",
                "Add a workspace folder",
                None,
                cx.listener(|this, _, _, cx| this.choose_folder(cx)),
            )));
        }
        column
    }

    /// Sidebar dot for one workspace row. The selected row reads the live
    /// board; every other row reads its background cache, so all rows stay
    /// current without being opened.
    fn workspace_status(&self, index: usize) -> Option<(Tone, String)> {
        if self.selected == Some(index) {
            if self.board_pending {
                return None;
            }
            let counts = count_services(&self.services, &self.catalog.disabled, self.hide_disabled);
            return Some(workspace_status_text(self.daemon_up, &counts));
        }
        let path = self.workspaces.get(index)?.path.clone();
        let board = self.boards.get(root_key(&path))?;
        Some(board_status(board, self.hide_disabled))
    }

    fn workspace_rows(
        &self,
        mut list: gpui::Div,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let hovered = self.hovered_workspace;
        for (index, workspace) in self.workspaces.iter().enumerate() {
            let path = workspace.path.clone();
            let label = ns_ui::path_name(&path);
            let selected = self.selected == Some(index);
            let (tone, status) = match self.workspace_status(index) {
                Some((tone, text)) => (Some(tone), Some(text)),
                None => (None, None),
            };
            let focus = self.list_focus.clone();
            let mut row = sidebar_row(SharedString::from(format!("ws-{index}")), label)
                .icon("folder")
                .selected(selected)
                .kbd(focused && selected)
                .tooltip(
                    ns_ui::display_path(&path),
                    status.clone().map(SharedString::from),
                )
                .reveal_actions(hovered == Some(index))
                .actions(icon_button(
                    SharedString::from(format!("forget-{index}")),
                    "trash",
                    "Remove from this list. Its services keep running.",
                    IconTone::Danger,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.selected = Some(index);
                        this.request_forget(cx);
                    }),
                ))
                .on_click(cx.listener(move |this, _, window, cx| {
                    focus.focus(window, cx);
                    this.select_workspace(index, cx);
                }));
            if let Some(hint) = parent_hint(&path) {
                row = row.hint(hint);
            }
            if let (Some(tone), Some(text)) = (tone, status) {
                row = row.status(tone, Some(text.into()));
            }
            list = list.child(row);
        }
        if self.workspaces.is_empty() {
            list = list.child(sidebar_empty("No workspaces yet"));
        }
        list
    }

    fn shared_rows(&self, mut list: gpui::Div, focused: bool, cx: &mut Context<Self>) -> gpui::Div {
        list = list.child(sidebar_section("Instances").count(self.instances.len()));
        if self.instances.is_empty() {
            list = list.child(sidebar_empty("No instances"));
        }
        for (index, instance) in self.instances.iter().enumerate() {
            let id = instance.id.clone();
            let state = if instance.actual.is_empty() {
                "Unknown state".to_string()
            } else {
                instance.actual.clone()
            };
            let selected = self.shared_pick == Some(index);
            let focus = self.list_focus.clone();
            list = list.child(
                sidebar_row(SharedString::from(format!("inst-{index}")), id.clone())
                    .icon("server")
                    .selected(selected)
                    .kbd(focused && selected)
                    .status(instance_tone(&instance.actual), Some(state.clone().into()))
                    .tooltip(id, Some(state.into()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        focus.focus(window, cx);
                        this.shared_pick = Some(index);
                        this.recipe_pick = None;
                        cx.notify();
                    })),
            );
        }
        list = list.child(sidebar_section("Recipes").count(self.recipes.len()));
        if self.recipes.is_empty() {
            list = list.child(sidebar_empty("No recipes"));
        }
        for (index, (name, version)) in self.recipes.iter().enumerate() {
            let name = name.clone();
            let version = version.clone();
            let full = format!("{name}@{version}");
            let selected = self.recipe_pick == Some(index);
            let focus = self.list_focus.clone();
            list = list.child(
                sidebar_row(SharedString::from(format!("recipe-{index}")), name)
                    .icon("plug")
                    .hint(version)
                    .selected(selected)
                    .kbd(focused && selected)
                    .tooltip(full, None)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        focus.focus(window, cx);
                        this.recipe_pick = Some(index);
                        this.shared_pick = None;
                        cx.notify();
                    })),
            );
        }
        list
    }

    fn on_sidebar_key(&mut self, key: SidebarKey, cx: &mut Context<Self>) {
        if key == SidebarKey::Open {
            ns_ui::show_detail();
            cx.notify();
            return;
        }
        match self.pane {
            Pane::Workspaces => {
                let len = self.workspaces.len();
                let Some(index) = sidebar_step(self.selected, len, key) else {
                    return;
                };
                sidebar_reveal(
                    &self.list_scroll,
                    index as f32 * SIDEBAR_ROW_H,
                    SIDEBAR_ROW_H,
                );
                if self.selected != Some(index) {
                    self.select_workspace(index, cx);
                }
            }
            Pane::Shared => {
                let len = self.instances.len() + self.recipes.len();
                let current = self.shared_cursor();
                let Some(index) = sidebar_step(current, len, key) else {
                    return;
                };
                sidebar_reveal(&self.list_scroll, self.shared_top(index), SIDEBAR_ROW_H);
                if current != Some(index) {
                    self.set_shared_cursor(index, cx);
                }
            }
        }
    }

    fn shared_cursor(&self) -> Option<usize> {
        if let Some(index) = self.shared_pick {
            Some(index)
        } else {
            self.recipe_pick.map(|index| self.instances.len() + index)
        }
    }

    fn set_shared_cursor(&mut self, index: usize, cx: &mut Context<Self>) {
        let instances = self.instances.len();
        if index < instances {
            self.shared_pick = Some(index);
            self.recipe_pick = None;
        } else {
            self.shared_pick = None;
            self.recipe_pick = Some(index - instances);
        }
        cx.notify();
    }

    /// Points from the top of the shared list to the row at a flat index.
    fn shared_top(&self, index: usize) -> f32 {
        let instances = self.instances.len();
        let instance_block = SIDEBAR_SECTION_H
            + if instances == 0 {
                SIDEBAR_ROW_H
            } else {
                instances as f32 * SIDEBAR_ROW_H
            };
        if index < instances {
            SIDEBAR_SECTION_H + index as f32 * SIDEBAR_ROW_H
        } else {
            instance_block + SIDEBAR_SECTION_H + (index - instances) as f32 * SIDEBAR_ROW_H
        }
    }
}

/// The folder above a workspace name, so two folders with the same name stay
/// distinct. The full path is the row tooltip.
fn parent_hint(path: &str) -> Option<String> {
    let shown = ns_ui::display_path(path);
    match shown.rsplit_once('/') {
        Some((parent, name)) if !parent.is_empty() && !name.is_empty() => Some(parent.to_string()),
        _ => None,
    }
}

/// Status-bar badge text for the failed services the board lists.
fn failed_label(failed: &[String]) -> Option<String> {
    match failed {
        [] => None,
        [one] => Some(format!("{one} failed")),
        many => Some(format!("{} failed: {}", many.len(), many.join(", "))),
    }
}

fn instance_tone(state: &str) -> Tone {
    match state {
        "ready" | "running" => Tone::Ok,
        "failed" | "crashed" => Tone::Err,
        "starting" | "stopping" | "restarting" | "preparing" | "running-unready"
        | "queued-start" => Tone::Warn,
        _ => Tone::Neutral,
    }
}

/// An empty note lets the status bar describe the pane on screen.
fn self_note(window: &mut HearthWindow, fallback: String) {
    window.note = fallback;
}

impl Render for HearthWindow {
    fn render(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(feature = "devtools")]
        let _clock = perf::clock();
        self.view_h = window.viewport_size().height.as_f32();
        self.fit_log(window);
        self.sync_log_scroll(cx);
        self.refresh_caches();
        let pane = self.pane;
        let sidebar = self.sidebar_column(window, cx);

        let title = self
            .root()
            .map(|path| ns_ui::display_path(&path))
            .unwrap_or_else(|| "Hearth".into());
        let mut body = div().flex().flex_col().gap_1();
        if pane == Pane::Workspaces {
            body = workspace_body(self, body, cx);
        } else {
            body = shared_body(self, body, cx);
        }
        // The accent word is the specific action. "Working" stays for a plain reload.
        let root = self.root();
        let status = busy_status(&self.inflight, self.busy);
        let meta = if self.inflight.is_empty() && !self.note.is_empty() {
            self.note.clone()
        } else if pane == Pane::Workspaces {
            self.scope_summary()
        } else {
            format!(
                "{} recipes · {} instances",
                self.recipes.len(),
                self.instances.len()
            )
        };
        let loading_name = root
            .as_deref()
            .map(ns_ui::path_name)
            .unwrap_or_else(|| "workspace".into());
        let dock = if pane == Pane::Workspaces {
            Some(if self.board_pending {
                loading_workspace(&loading_name, true).into_any_element()
            } else if self.selected_service.is_some() {
                log_pane(self, cx).into_any_element()
            } else if let Some(flight) = self
                .inflight
                .iter()
                .rev()
                .find(|job| same_board(job, false, root.as_deref()))
            {
                action_panel(&flight.label, true).into_any_element()
            } else {
                log_placeholder("Select a service to show its log.").into_any_element()
            })
        } else {
            Some(shared_dock(self, cx).into_any_element())
        };
        let confirm = self.dialog_overlay(cx);
        let palette = self.palette_overlay(cx);
        let banner = self.update_banner(cx);
        let toasts = ToastViewport::new("hearth-toasts", &self.toaster);
        let chrome = chrome_busy(
            sidebar,
            title,
            body,
            dock,
            if self.settings_open {
                Some(
                    side_column("Settings", hearth_settings(self, cx))
                        .w(px(360.))
                        .into_any_element(),
                )
            } else {
                None
            },
            Some(
                settings_rail(
                    "rail-top",
                    "refresh",
                    false,
                    "Refresh",
                    cx.listener(|this, _, _, cx| {
                        if !this.refresh_locked() {
                            this.reload(cx);
                        }
                    }),
                    self.settings_open,
                    if self.settings_open {
                        "Hide Settings"
                    } else {
                        "Show Settings"
                    },
                    cx.listener(|this, _, _, cx| {
                        this.settings_open = !this.settings_open;
                        cx.notify();
                    }),
                )
                .into_any_element(),
            ),
            status,
            self.busy || self.board_pending || !self.inflight.is_empty(),
            meta,
            Some({
                let label = if pane == Pane::Workspaces {
                    failed_label(&self.board_cache.failed)
                } else {
                    None
                };
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(label.map(|text| {
                        Badge::new(SharedString::from(text))
                            .tone(BadgeTone::Danger)
                            .dot()
                    }))
                    .child(div().text_xs().text_color(theme().muted).child("⌘K"))
                    .into_any_element()
            }),
        );
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(banner)
            .child(div().flex_1().min_h(px(0.)).child(chrome))
            .children(confirm)
            .children(palette)
            .child(toasts)
    }
}

impl HearthWindow {
    /// An offered release as a slim bar over the window. Install still asks
    /// first, the same as the button in Settings.
    fn update_banner(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let offer = self.update.as_ref().filter(|offer| offer.available)?;
        if self.update_dismissed {
            return None;
        }
        let entity = cx.entity();
        let later = entity.clone();
        let state = if self.busy && self.note.contains("Installing update") {
            UpdateState::Downloading(0.5)
        } else {
            UpdateState::Available
        };
        Some(
            UpdateBanner::new("hearth-update", offer.latest.clone(), state)
                .on_download(move |_, cx| {
                    entity.update(cx, |this, cx| this.request_install_update(cx));
                })
                .on_later(move |_, cx| {
                    later.update(cx, |this, cx| {
                        this.update_dismissed = true;
                        cx.notify();
                    });
                })
                .into_any_element(),
        )
    }

    fn request_install_update(&mut self, cx: &mut Context<Self>) {
        let latest = self
            .update
            .as_ref()
            .map(|offer| offer.text.clone())
            .unwrap_or_default();
        self.ask_for(
            "Install the hearth update?",
            &format!("{latest}\n\nRunning daemons keep the previous binary until they restart."),
            "Install",
            "download",
            false,
            Pending::InstallUpdate,
            cx,
        );
    }

    /// ⌘K: every workspace command and every service, fuzzy-matched.
    fn palette_overlay(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.palette_open {
            return None;
        }
        let close = cx.entity();
        let run = cx.entity();
        let mut actions = vec![
            Command::new("act:refresh", "Refresh"),
            Command::new("act:start-all", "Start all services"),
            Command::new("act:stop-all", "Stop all services"),
            Command::new("act:start-daemon", "Start daemon"),
            Command::new("act:stop-daemon", "Stop daemon"),
            Command::new("act:restart-daemon", "Restart daemon"),
            Command::new("act:settings", "Toggle settings"),
            Command::new("act:workspaces", "Show workspaces"),
            Command::new("act:shared", "Show shared services"),
        ];
        actions.sort_by(|a, b| a.label.cmp(&b.label));
        let mut palette = CommandPalette::new("hearth-palette", move |_, cx| {
            close.update(cx, |this, cx| {
                this.palette_open = false;
                cx.notify();
            });
        })
        .group("Actions", actions);
        let mut services = Vec::new();
        for service in &self.services {
            if self.is_disabled(&service.id) {
                continue;
            }
            let verb = if service_shows_stop(&service.state) {
                "Stop"
            } else {
                "Start"
            };
            services.push(Command::new(
                format!("svc:{}:{}", verb.to_lowercase(), service.id),
                format!("{verb} {}", service.id),
            ));
            services.push(Command::new(
                format!("svc:restart:{}", service.id),
                format!("Restart {}", service.id),
            ));
            services.push(Command::new(
                format!("svc:log:{}", service.id),
                format!("Show log of {}", service.id),
            ));
        }
        if !services.is_empty() {
            palette = palette.group("Services", services);
        }
        palette = palette.on_run(move |value, _, cx| {
            let value = value.to_string();
            run.update(cx, |this, cx| {
                this.palette_open = false;
                this.run_command(&value, cx);
                cx.notify();
            });
        });
        Some(palette.into_any_element())
    }

    fn run_command(&mut self, value: &str, cx: &mut Context<Self>) {
        match value {
            "act:refresh" => {
                if !self.refresh_locked() {
                    self.reload(cx);
                }
            }
            "act:start-all" => self.request_all("start", cx),
            "act:stop-all" => self.request_all("stop", cx),
            "act:start-daemon" => self.request_trust(cx),
            "act:stop-daemon" => self.request_stop_daemon(cx),
            "act:restart-daemon" => self.request_restart_daemon(cx),
            "act:settings" => self.settings_open = !self.settings_open,
            "act:workspaces" => self.set_pane(Pane::Workspaces, cx),
            "act:shared" => self.set_pane(Pane::Shared, cx),
            _ => {
                if let Some((verb, id)) = parse_service_command(value) {
                    self.act_on_service(verb, id, cx);
                }
            }
        }
    }

    /// Select a service by id and run a verb on it. Shared by the palette and
    /// the row context menu, so both go through the same confirmations.
    fn act_on_service(&mut self, verb: &str, id: &str, cx: &mut Context<Self>) {
        let Some(index) = self.services.iter().position(|service| service.id == id) else {
            return;
        };
        self.selected_service = Some(index);
        match verb {
            "start" => self.request_service("start", id.to_string(), cx),
            "stop" => self.request_service("stop", id.to_string(), cx),
            "restart" => self.request_service("restart", id.to_string(), cx),
            "log" => {
                self.open_log(cx);
                ns_ui::show_dock();
            }
            _ => {}
        }
        cx.notify();
    }
}

fn hearth_settings(window: &HearthWindow, cx: &mut Context<HearthWindow>) -> gpui::Div {
    let mut body = div().flex().flex_col().gap_4();
    let mut updates = SettingsSection::new("Updates");
    if window.update.is_none() && !window.busy {
        updates = updates.description("Check whether hearth is already up to date.");
    }
    if window.busy && window.inflight.is_empty() {
        let label = if window.note.contains("Installing update") {
            "Installing update…"
        } else {
            "Checking for updates…"
        };
        updates = updates.row(working_row("settings-wait", label));
    }
    if let Some(offer) = &window.update {
        updates = updates.description(offer.text.clone());
    }
    updates = updates.row(SettingsRow::new("hearth").control(update_control(window, cx)));
    body = body.child(updates);
    let entity = cx.entity();
    body = body.child(
        SettingsSection::new("View").row(
            SettingsRow::new("Hide disabled")
                .description("Leave out services the catalog disables.")
                .control(
                    Switch::new("hide-disabled", window.hide_disabled).on_change(
                        move |on, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.hide_disabled = on;
                                write_hide_disabled(on);
                                this.board_cache_dirty = true;
                                cx.notify();
                            });
                        },
                    ),
                ),
        ),
    );
    body
}

/// A check with nothing newer stays a quiet button. An available release
/// becomes the install button and asks before it runs.
fn update_control(window: &HearthWindow, cx: &mut Context<HearthWindow>) -> gpui::AnyElement {
    if window.update.as_ref().is_some_and(|offer| offer.available) {
        button(
            "install-update",
            "download",
            "Install update",
            cx.listener(|this, _, _, cx| this.request_install_update(cx)),
        )
        .into_any_element()
    } else {
        quiet_button(
            "check-update",
            "download",
            "Check for updates",
            cx.listener(|this, _, _, cx| this.check_update(cx)),
        )
        .into_any_element()
    }
}

fn parse_update(text: &str) -> ns_ui::UpdateReport {
    ns_ui::parse_update("hearth", text)
}

fn workspace_body(
    window: &mut HearthWindow,
    mut body: gpui::Div,
    cx: &mut Context<HearthWindow>,
) -> gpui::Div {
    if window.board_pending {
        let name = window
            .root()
            .map(|path| ns_ui::path_name(&path))
            .unwrap_or_else(|| "workspace".into());
        return body.child(loading_workspace(&name, false));
    }
    let theme = theme();
    let root = window.root();
    if window.current().is_none() {
        return body.child(empty_state(
            "folder-plus",
            "No workspace open",
            "Add a folder with a hearth.yaml to see its services.",
            Some(
                quiet_button_md(
                    "empty-add-folder",
                    "folder-plus",
                    "Add Folder…",
                    cx.listener(|this, _, _, cx| this.choose_folder(cx)),
                )
                .into_any_element(),
            ),
        ));
    }
    // The newest daemon command, when one is in flight. Its label is the tooltip
    // status ("Starting daemon…"). An empty label still counts as working.
    let working_label = window
        .inflight
        .iter()
        .rev()
        .find(|job| same_board(job, false, root.as_deref()) && job.daemon)
        .map(|job| job.label.clone());
    let refresh_dim = window.refresh_locked();
    let bulk_dim = window.workspace_bulk_locked();
    // Working wins, including a blank label. The badge is not part of bulk_dim:
    // the buttons fade while a command runs, and the status mark stays readable.
    let dot = match daemon_dot_tone(window.daemon_up, working_label.is_some()) {
        DaemonDotTone::Running => theme.ok,
        DaemonDotTone::Stopped => theme.err,
        DaemonDotTone::Working => theme.warn,
    };
    let daemon_tip = daemon_tooltip(
        window.daemon_up,
        working_label.as_deref(),
        window.port,
        &window.daemon_note,
    );
    // Daemon controls and service controls are two labelled groups. Every
    // action is an outline with the same weight at rest; the destructive ones
    // turn red only under the pointer, and each still asks first.
    let daemon_controls = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_1()
        .child(daemon_badge(dot, daemon_tip))
        .child(
            text_action(
                "refresh",
                "refresh",
                "Refresh",
                Weight::Quiet,
                cx.listener(|this, _, _, cx| {
                    if this.refresh_locked() {
                        return;
                    }
                    this.reload(cx);
                }),
            )
            .when(refresh_dim, |button| button.opacity(0.4)),
        )
        .child(if window.daemon_up {
            text_action(
                "stop-daemon",
                "stop",
                "Stop",
                Weight::Danger,
                cx.listener(|this, _, _, cx| this.request_stop_daemon(cx)),
            )
            .when(bulk_dim, |button| button.opacity(0.4))
            .into_any_element()
        } else {
            text_action(
                "start-daemon",
                "play",
                "Start",
                Weight::Accent,
                cx.listener(|this, _, _, cx| this.request_trust(cx)),
            )
            .when(bulk_dim, |button| button.opacity(0.4))
            .into_any_element()
        })
        .child(
            text_action(
                "restart-daemon",
                "restart",
                "Restart",
                Weight::Quiet,
                cx.listener(|this, _, _, cx| this.request_restart_daemon(cx)),
            )
            .when(bulk_dim, |button| button.opacity(0.4)),
        );
    let service_controls = div()
        .flex()
        .flex_wrap()
        .gap_1()
        .child(
            text_action(
                "start-all",
                "play",
                "Start all",
                Weight::Accent,
                cx.listener(|this, _, _, cx| this.request_all("start", cx)),
            )
            .when(bulk_dim, |button| button.opacity(0.4)),
        )
        .child(
            text_action(
                "stop-all",
                "stop",
                "Stop all",
                Weight::Danger,
                cx.listener(|this, _, _, cx| this.request_all("stop", cx)),
            )
            .when(bulk_dim, |button| button.opacity(0.4)),
        );
    body = body.child(
        div()
            .flex()
            .flex_wrap()
            .items_start()
            .gap_x_4()
            .gap_y_2()
            .pb_2()
            .border_b_1()
            .border_color(theme.raised)
            .child(toolbar_group("Daemon", daemon_controls))
            // The rule only separates the groups when they sit side by side.
            .when(ns_ui::body_width() >= 520., |row| {
                row.child(div().self_stretch().w(px(1.)).flex_none().bg(theme.raised))
            })
            .child(toolbar_group("Services", service_controls)),
    );
    let selected_service = window.selected_service;
    let cache = &window.board_cache;
    for section in &cache.sections {
        let title = section.title.clone();
        let folded = window.collapsed.contains(&title);
        let mut group_targets = vec![title.as_str()];
        for index in &section.indexes {
            if let Some(service) = window.services.get(*index) {
                group_targets.push(service.id.as_str());
            }
        }
        let group_dim = action_blocked(&window.inflight, &group_targets, false, root.as_deref());
        let pending = window
            .inflight
            .iter()
            .rev()
            .find(|job| {
                same_board(job, false, root.as_deref()) && !job.daemon && job.target == title
            })
            .map(|job| pending_state_for(job.verb));
        let group_actions = section.actions.then(|| {
            let start_name = title.clone();
            let stop_name = title.clone();
            let restart_name = title.clone();
            div()
                .flex_none()
                // Same inset as a service row, so the capsule ends where its icons do.
                .pr_2()
                .when(group_dim, |row| row.opacity(0.4))
                .child(group_cluster(vec![
                    row_action(
                        format!("gs-{title}"),
                        "play",
                        format!("Start all in {title}"),
                        Weight::Accent,
                        cx.listener(move |this, _, _, cx| {
                            this.request_group("start", start_name.clone(), cx);
                        }),
                    )
                    .into_any_element(),
                    row_action(
                        format!("gp-{title}"),
                        "stop",
                        format!("Stop all in {title}"),
                        Weight::Danger,
                        cx.listener(move |this, _, _, cx| {
                            this.request_group("stop", stop_name.clone(), cx);
                        }),
                    )
                    .into_any_element(),
                    row_action(
                        format!("gr-{title}"),
                        "restart",
                        format!("Restart all in {title}"),
                        Weight::Quiet,
                        cx.listener(move |this, _, _, cx| {
                            this.request_group("restart", restart_name.clone(), cx);
                        }),
                    )
                    .into_any_element(),
                ]))
                .into_any_element()
        });
        let fold_name = title.clone();
        body = body.child(group_header(
            SharedString::from(format!("group-{title}")),
            &title,
            section.indexes.len(),
            folded,
            pending,
            group_actions,
            cx.listener(move |this, _, _, cx| {
                if !this.collapsed.remove(&fold_name) {
                    this.collapsed.insert(fold_name.clone());
                }
                cx.notify();
            }),
        ));
        if folded {
            continue;
        }
        for &index in &section.indexes {
            let service = &window.services[index];
            let id = service.id.clone();
            let is_disabled = cache.disabled.contains(&id);
            let shown = row_state(&service.state, &id, &window.inflight, root.as_deref());
            let marked = row_marked(&id, &window.inflight, root.as_deref());
            let failed = matches!(shown, "failed" | "crashed");
            let is_selected = selected_service == Some(index);
            let chrome = row_chrome(is_selected, marked, failed);
            let row_dim = action_blocked(&window.inflight, &[&id], false, root.as_deref());
            // Disabled rows have no lifecycle buttons. The others show Start or
            // Stop, then Restart. Externally owned ports use Start as Reclaim.
            let mut actions = div()
                .flex()
                .flex_none()
                .items_center()
                .justify_end()
                .gap_1()
                .w(px(ACTIONS_COL));
            if !is_disabled {
                if service_shows_stop(&service.state) {
                    let stop_id = id.clone();
                    actions = actions.child(row_action(
                        format!("svc-stop-{index}"),
                        "stop",
                        format!("Stop {id}"),
                        Weight::Danger,
                        cx.listener(move |this, _, _, cx| {
                            this.selected_service = Some(index);
                            this.request_service("stop", stop_id.clone(), cx);
                        }),
                    ));
                } else {
                    let start_id = id.clone();
                    let verb = if service.state == "externally-owned" {
                        "Reclaim"
                    } else {
                        "Start"
                    };
                    actions = actions.child(row_action(
                        format!("svc-start-{index}"),
                        "play",
                        format!("{verb} {id}"),
                        Weight::Accent,
                        cx.listener(move |this, _, _, cx| {
                            this.selected_service = Some(index);
                            this.request_service("start", start_id.clone(), cx);
                        }),
                    ));
                }
                let restart_id = id.clone();
                actions = actions.child(row_action(
                    format!("svc-restart-{index}"),
                    "restart",
                    format!("Restart {id}"),
                    Weight::Quiet,
                    cx.listener(move |this, _, _, cx| {
                        this.selected_service = Some(index);
                        this.request_service("restart", restart_id.clone(), cx);
                    }),
                ));
                // The stored state still picks Start or Stop. Dimming only says this row's click is waiting.
                if row_dim {
                    actions = actions.opacity(0.4);
                }
            }
            let badge = if is_disabled {
                tag_badge("disabled")
            } else {
                state_badge(shown)
            };
            let hit = div()
                .id(SharedString::from(format!("svc-hit-{index}")))
                .h(px(30.))
                .flex()
                .items_center()
                .gap_2()
                .cursor_pointer()
                .child(
                    div()
                        .id(SharedString::from(format!("svc-name-{index}")))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(if failed {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::NORMAL
                        })
                        .when(is_disabled, |name| name.text_color(theme.muted))
                        .tooltip(Tooltip::text(id.clone()))
                        .child(id.clone()),
                )
                .child(badge)
                // The slot is always 12px, empty until this row is in flight.
                .child(pending_mark(
                    marked,
                    SharedString::from(format!("svc-spin-{id}")),
                ))
                .child(actions);
            let mut row = div()
                .id(SharedString::from(format!("svc-{index}")))
                .w_full()
                .px_2()
                .rounded_md()
                .flex()
                .flex_col()
                .border_l_2()
                .border_color(if chrome.accent_edge {
                    theme.accent
                } else if chrome.failed_edge {
                    theme.err
                } else {
                    gpui::Rgba {
                        r: 0.,
                        g: 0.,
                        b: 0.,
                        a: 0.,
                    }
                })
                // Pending wash and the failed wash never replace the selected fill.
                .when(chrome.pending_fill, |row| row.bg(theme.warn_bg))
                .when(chrome.selected_fill, |row| row.bg(theme.selected))
                .when(chrome.failed_fill, |row| row.bg(theme.err_bg))
                .when(chrome.hover, |row| row.hover(|style| style.bg(theme.hover)))
                .child(hit.on_click(cx.listener(move |this, _, _, cx| {
                    this.selected_service = Some(index);
                    this.open_log(cx);
                    // A narrow window has no room beside the board: open the log over it.
                    ns_ui::show_dock();
                })));
            if let Some(url_indexes) = cache.urls_by_service.get(&id) {
                for &url_index in url_indexes {
                    let url = &window.urls[url_index];
                    let target = url.url.clone();
                    let link_color = if url.visible {
                        theme.accent
                    } else {
                        theme.muted
                    };
                    // The label is its own run. Two spaces inside one string collapse, so
                    // "storefront" used to sit against the URL.
                    let label = if url.label.is_empty() {
                        None
                    } else {
                        Some(
                            div()
                                .flex_shrink_0()
                                .text_color(theme.muted)
                                .child(url.label.clone()),
                        )
                    };
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("url-row-{index}-{url_index}")))
                            .w_full()
                            .min_h(px(22.))
                            .pl(px(10.))
                            .flex()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .text_xs()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_service = Some(index);
                                this.open_log(cx);
                                ns_ui::show_dock();
                            }))
                            .children(label)
                            .child(
                                div()
                                    .id(SharedString::from(format!("url-{index}-{url_index}")))
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_color(link_color)
                                    .cursor_pointer()
                                    // A hover underline on the text run never paints: GPUI reuses the
                                    // shaped line when only the decoration changes. A bottom border is
                                    // the same width as this shrink-wrapped link.
                                    .border_b_1()
                                    .border_color(gpui::rgba(0x00000000))
                                    .hover(|style| style.border_color(link_color))
                                    .tooltip(Tooltip::text(format!("Open {}", url.url)))
                                    .child(url.url.clone())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.selected_service = Some(index);
                                        this.open_log(cx);
                                        ns_ui::open_url(&target);
                                    })),
                            ),
                    );
                }
            }
            let mut menu = Menu::new();
            if !is_disabled {
                let menu_item = |label: &'static str, verb: &'static str| {
                    let entity = cx.entity();
                    let target = id.clone();
                    MenuItem::new(label).on_click(move |_, cx| {
                        entity.update(cx, |this, cx| this.act_on_service(verb, &target, cx));
                    })
                };
                menu = menu
                    .item(if service_shows_stop(&service.state) {
                        menu_item("Stop", "stop")
                    } else if service.state == "externally-owned" {
                        menu_item("Reclaim and start", "start")
                    } else {
                        menu_item("Start", "start")
                    })
                    .item(menu_item("Restart", "restart"))
                    .separator();
            }
            let copy_id = id.clone();
            menu = menu
                .item(MenuItem::new("Show log").on_click({
                    let entity = cx.entity();
                    let target = id.clone();
                    move |_, cx| {
                        entity.update(cx, |this, cx| this.act_on_service("log", &target, cx));
                    }
                }))
                .item(MenuItem::new("Copy name").on_click(move |_, _| {
                    ns_ui::copy_text(&copy_id);
                }));
            body = body.child(
                ContextMenu::new(SharedString::from(format!("svc-menu-{index}")), menu)
                    .w_full()
                    .child(row),
            );
        }
    }
    if window.services.is_empty() && !window.busy {
        // Catalog rows still list while the daemon is down. This hint is only
        // for a list that has nothing to show.
        let detail = if !window.daemon_up && working_label.is_none() {
            "Start the daemon to run services."
        } else {
            "Define services in hearth.yaml, then refresh."
        };
        body = body.child(empty_state(
            "server",
            "No services in this workspace",
            detail,
            None,
        ));
    } else if window.hide_disabled
        && !window.services.is_empty()
        && window.board_cache.counts.ready
            + window.board_cache.counts.stopped
            + window.board_cache.counts.failed
            == 0
    {
        body = body.child(empty_state(
            "eye-off",
            "Every service is disabled",
            "Turn off Hide disabled in Settings to list them.",
            None,
        ));
    }
    body
}

/// Xs spinner width. Present on every row so the mark does not change height or columns.
const PENDING_SLOT: f32 = 12.;
/// Instance state word plus the reserved spinner. Fixed so a pending word does not move the actions.
const INSTANCE_STATE_COL: f32 = 128.;
/// Daemon status row. The start button is absent while smp is up, and the height stays.
const DAEMON_ROW_H: f32 = 32.;
/// "Used by" header plus the scrolling attachment list, at its tallest.
const SHARED_DETAIL_BLOCK: f32 = 260.;
const DETAIL_LIST_MAX: f32 = 220.;

fn row_spinner(id: SharedString) -> Spinner {
    Spinner::new(id)
        .size(IconSize::Xs)
        .color(theme().accent.into())
}

fn pending_mark(active: bool, id: SharedString) -> gpui::Div {
    let mut slot = div()
        .w(px(PENDING_SLOT))
        .h(px(PENDING_SLOT))
        .flex_none()
        .flex()
        .items_center()
        .justify_center();
    if active {
        slot = slot.child(row_spinner(id));
    }
    slot
}

fn action_panel(label: &str, fill: bool) -> gpui::Div {
    let theme = theme();
    let mut panel = div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .px_4();
    panel = if fill {
        panel.size_full()
    } else {
        panel.min_h(px(240.))
    };
    panel.child(
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(spinner("dock-action"))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(label.to_string()),
            ),
    )
}

fn loading_workspace(name: &str, fill: bool) -> gpui::Div {
    let theme = theme();
    let mut panel = div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .px_4();
    panel = if fill {
        panel.size_full()
    } else {
        panel.min_h(px(240.))
    };
    panel
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(spinner("workspace-wait"))
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child("Loading workspace…"),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted)
                .child(name.to_string()),
        )
        .child(
            div()
                .w_full()
                .max_w(px(360.))
                .child(skeleton_list("workspace-skeleton", 5)),
        )
}

fn log_placeholder(message: &str) -> impl IntoElement {
    let theme = theme();
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(40.))
                .px_3()
                .flex()
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child("Log"),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_4()
                .text_sm()
                .text_color(theme.muted)
                .child(message.to_string()),
        )
}

fn log_pane(window: &HearthWindow, cx: &mut Context<HearthWindow>) -> impl IntoElement {
    let theme = theme();
    let name = if window.log_shared {
        window.log_service.clone()
    } else {
        window
            .service_id()
            .unwrap_or_else(|| "Select a service".into())
    };
    let copy = quiet_button(
        "log-copy",
        "copy",
        "Copy",
        cx.listener(|this, _, _, cx| {
            let selection = this.log_sel.filter(|selection| !selection.is_caret());
            let text = if let Some(selection) = selection {
                logbuf::selected_text(&this.log_lines, selection)
            } else {
                this.log_lines.join("\n")
            };
            if text.is_empty() {
                this.message("The log is empty.", cx);
            } else if ns_ui::copy_text(&text) {
                this.message(
                    if selection.is_some() {
                        "Copied the selection."
                    } else {
                        "Copied the log."
                    },
                    cx,
                );
            } else {
                this.warning("Could not copy the log.", cx);
            }
        }),
    );
    let loading = (window.log_loading && !window.log_lines.is_empty())
        .then(|| working_row("log-earlier", "Loading earlier…"));
    let theme_for_footer = theme;
    let following = window.log_follow || window.log_rows.is_empty();
    let mut pane = div()
        .size_full()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(40.))
                .flex_shrink_0()
                .px_3()
                .flex()
                .items_center()
                .gap_2()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex_shrink_0()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("Log"),
                )
                .child(
                    div()
                        .id("log-name")
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(theme.accent)
                        .tooltip(Tooltip::text(name.clone()))
                        .child(name),
                )
                .children(loading)
                .child(copy),
        )
        .child(
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .overflow_hidden()
                .relative()
                .child(log_view(window, cx))
                .child(Scrollbar::new(
                    "log-scroll",
                    &window.log_scroll.0.borrow().base_handle,
                    Axis::Vertical,
                )),
        );
    // The jump control sits in its own band under the text, so it never covers a line.
    if !following {
        pane = pane.child(
            div()
                .h(px(32.))
                .flex_shrink_0()
                .px_3()
                .flex()
                .items_center()
                .justify_between()
                .border_t_1()
                .border_color(theme_for_footer.border)
                .bg(theme_for_footer.editor)
                .child(
                    div()
                        .text_xs()
                        .text_color(theme_for_footer.muted)
                        .child("Scrolled back in the log"),
                )
                .child(quiet_button(
                    "log-latest",
                    "arrow-down-to-line",
                    "Jump to latest",
                    cx.listener(|this, _, _, cx| {
                        this.pin_bottom();
                        cx.notify();
                    }),
                )),
        );
    }
    pane
}

fn log_view(window: &HearthWindow, cx: &mut Context<HearthWindow>) -> impl IntoElement {
    let down = cx.entity().clone();
    let drag = cx.entity().clone();
    let up = cx.entity().clone();
    let down_bounds = window.log_view_bounds.clone();
    let drag_bounds = window.log_view_bounds.clone();
    div()
        .flex_1()
        .min_w(px(0.))
        .min_h(px(0.))
        .h_full()
        .overflow_hidden()
        .cursor_text()
        .track_focus(&window.log_focus)
        .id("log-view")
        .child(bounds_probe(window.log_view_bounds.clone()))
        .child(if window.log_lines.is_empty() {
            if window.log_loading || window.log_refreshing {
                div()
                    .size_full()
                    .p_3()
                    .child(skeleton_list("log-skeleton", 8))
                    .into_any_element()
            } else {
                log_empty().into_any_element()
            }
        } else {
            log_list(window).into_any_element()
        })
        .on_scroll_wheel(cx.listener(|this, _: &ScrollWheelEvent, _, _| this.note_log_scroll()))
        .on_key_down(
            cx.listener(|this, event: &KeyDownEvent, _, cx| this.copy_log_selection(event, cx)),
        )
        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
            let bounds = down_bounds.get();
            let advance = log_char_advance(window);
            let pad = window.rem_size().as_f32() * 0.5;
            let focus = down.update(cx, |this, cx| {
                this.begin_log_selection(
                    event.position.x,
                    event.position.y,
                    bounds.origin.x,
                    bounds.origin.y,
                    bounds.size.height.as_f32(),
                    pad,
                    advance,
                    cx,
                );
                this.log_focus.clone()
            });
            focus.focus(window, cx);
            window.refresh();
            cx.stop_propagation();
        })
        .on_mouse_move(move |event, window, cx| {
            if !event.dragging() {
                return;
            }
            let bounds = drag_bounds.get();
            let pad = window.rem_size().as_f32() * 0.5;
            let acted = drag.update(cx, |this, cx| {
                if !this.log_selecting {
                    return false;
                }
                this.extend_log_selection(
                    event.position.x,
                    event.position.y,
                    bounds.origin.x,
                    bounds.origin.y,
                    bounds.size.height.as_f32(),
                    pad,
                    cx,
                );
                true
            });
            if acted {
                window.refresh();
                cx.stop_propagation();
            }
        })
        .on_mouse_up(MouseButton::Left, move |_event, window, cx| {
            let acted = up.update(cx, |this, cx| {
                if !this.log_selecting {
                    return false;
                }
                this.end_log_selection(cx);
                true
            });
            if acted {
                window.refresh();
            }
        })
}

/// Records the bounds of the element it is placed in. GPUI's mouse callbacks no
/// longer hand the hitbox to the handler, so a pane that needs its own geometry
/// reads the last prepaint. The canvas takes no hitbox, so it never steals a
/// click from the element around it.
fn bounds_probe(slot: Rc<Cell<Bounds<Pixels>>>) -> impl IntoElement {
    canvas(move |bounds, _, _| slot.set(bounds), |_, _, _, _| {})
        .absolute()
        .top_0()
        .left_0()
        .size_full()
}

fn log_char_advance(window: &gpui::Window) -> f32 {
    let size = px(window.rem_size().as_f32() * 0.75);
    let layout = window.text_system().layout_line(
        "0",
        size,
        &[gpui::TextRun {
            len: 1,
            font: gpui::font(ns_ui::CODE_FONT),
            ..Default::default()
        }],
        None,
    );
    let width = layout.width.as_f32();
    if width > 0.5 { width } else { 7.2 }
}

/// Left padding of a log row, border included, and the strip kept clear for the scrollbar.
const LOG_PAD: f32 = 8.;
const LOG_GUTTER: f32 = 16.;

fn log_empty() -> impl IntoElement {
    let theme = theme();
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .px_4()
        .child(div().text_sm().child("No log yet"))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted)
                .child("It fills once the service writes a line."),
        )
}

fn level_color(level: logview::Level, theme: &ns_ui::Theme) -> Option<gpui::Rgba> {
    match level {
        logview::Level::Warn => Some(theme.warn),
        logview::Level::Error => Some(theme.err),
        logview::Level::None => None,
    }
}

fn rgb_u32(color: gpui::Rgba) -> u32 {
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u32;
    (channel(color.r) << 16) | (channel(color.g) << 8) | channel(color.b)
}

fn log_highlight_span(
    selection: logbuf::LogSelection,
    index: usize,
    line: &str,
) -> Option<std::ops::Range<usize>> {
    if let Some(span) = logbuf::selection_bytes(selection, index, line) {
        return Some(span);
    }
    if line.is_empty() {
        let (start, end) = selection.ordered();
        if index == start.0 && index < end.0 || index > start.0 && index < end.0 {
            return Some(0..1);
        }
    }
    None
}

fn log_line_highlights(
    line: &str,
    paints: &[logbuf::LogPaint],
    selection: Option<std::ops::Range<usize>>,
    theme: &ns_ui::Theme,
    selected: gpui::HighlightStyle,
) -> Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> {
    let len = line.len();
    let mut cuts = vec![0, len];
    for paint in paints {
        cuts.push(paint.start.min(len));
        cuts.push(paint.end.min(len));
    }
    if let Some(span) = &selection {
        cuts.push(span.start.min(len));
        cuts.push(span.end.min(len));
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    for pair in cuts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if start >= end {
            continue;
        }
        if selection
            .as_ref()
            .is_some_and(|span| start >= span.start && end <= span.end)
        {
            out.push((start..end, selected));
            continue;
        }
        if let Some(paint) = paints
            .iter()
            .find(|paint| paint.start <= start && paint.end >= end)
        {
            out.push((start..end, paint_style(paint, theme)));
        }
    }
    out
}

fn paint_style(paint: &logbuf::LogPaint, theme: &ns_ui::Theme) -> gpui::HighlightStyle {
    let mut foreground = paint.fg.map(gpui::rgb);
    let mut background = paint.bg.map(gpui::rgb);
    if paint.reverse {
        let fore = foreground.unwrap_or(theme.text);
        let back = background.unwrap_or(theme.editor);
        foreground = Some(back);
        background = Some(fore);
    }
    gpui::HighlightStyle {
        color: foreground.map(gpui::Hsla::from),
        background_color: background.map(gpui::Hsla::from),
        font_weight: paint.bold.then_some(gpui::FontWeight::BOLD),
        font_style: paint.italic.then_some(gpui::FontStyle::Italic),
        underline: paint.underline.then(|| gpui::UnderlineStyle {
            thickness: px(1.),
            color: foreground.map(gpui::Hsla::from),
            wavy: false,
        }),
        fade_out: paint.dim.then_some(0.45),
        ..Default::default()
    }
}

fn log_list(window: &HearthWindow) -> impl IntoElement {
    let theme = theme();
    let selection = window.log_sel.filter(|selection| !selection.is_caret());
    let highlight = gpui::HighlightStyle {
        background_color: Some(gpui::Hsla::from(theme.accent)),
        color: Some(gpui::Hsla::from(theme.accent_ink)),
        ..Default::default()
    };
    let lines = window.log_lines.clone();
    let paints = window.log_paints.clone();
    let rows = window.log_rows.clone();
    let levels = window.log_levels.clone();
    let indent = logview::CONTINUATION_CELLS as f32 * window.log_char_w;
    let count = rows.len();
    uniform_list("log-lines", count, move |range, _, _| {
        let mut out = Vec::new();
        for at in range {
            let row = rows.rows[at];
            let line = &lines[row.line];
            let span_of = row.start..row.end;
            let slice = &line[span_of.clone()];
            let shown: SharedString = if slice.is_empty() {
                " ".into()
            } else {
                slice.to_string().into()
            };
            let level = levels.get(row.line);
            let tone = level.and_then(|found| level_color(found.level, &theme));
            let mut row_paints: Vec<logbuf::LogPaint> = Vec::new();
            if let (Some(found), Some(color)) = (level, tone) {
                let token = found
                    .token
                    .clone()
                    .and_then(|token| logview::clip(token, span_of.clone()));
                if let Some(token) = token {
                    row_paints.push(logbuf::LogPaint {
                        start: token.start,
                        end: token.end,
                        fg: Some(rgb_u32(color)),
                        bg: None,
                        bold: true,
                        dim: false,
                        italic: false,
                        underline: false,
                        reverse: false,
                    });
                }
            }
            for paint in paints.get(row.line).map(Vec::as_slice).unwrap_or(&[]) {
                if let Some(span) = logview::clip(paint.start..paint.end, span_of.clone()) {
                    row_paints.push(logbuf::LogPaint {
                        start: span.start,
                        end: span.end,
                        ..*paint
                    });
                }
            }
            let span = selection
                .and_then(|selection| log_highlight_span(selection, row.line, line))
                .and_then(|span| {
                    if line.is_empty() {
                        Some(span)
                    } else {
                        logview::clip(span, span_of.clone())
                    }
                });
            let highlights =
                log_line_highlights(shown.as_ref(), &row_paints, span, &theme, highlight);
            let text = if highlights.is_empty() {
                gpui::StyledText::new(shown)
            } else {
                gpui::StyledText::new(shown).with_highlights(highlights)
            };
            let error = level.is_some_and(|found| found.level == logview::Level::Error);
            out.push(
                div()
                    .h(px(logview::ROW_H))
                    .w_full()
                    .pl(px(LOG_PAD - 2. + if row.cont { indent } else { 0. }))
                    .pr(px(LOG_GUTTER))
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font_family(ns_ui::CODE_FONT)
                    .text_xs()
                    .text_color(theme.text)
                    .border_l_2()
                    .border_color(tone.map_or(gpui::transparent_black(), gpui::Hsla::from))
                    .when(error, |el| el.bg(theme.err_bg))
                    .child(text),
            );
        }
        out
    })
    .track_scroll(&window.log_scroll)
    .with_sizing_behavior(gpui::ListSizingBehavior::Auto)
    .flex_1()
    .h(px(log_viewport(window)))
    .min_w(px(0.))
    .min_h(px(0.))
}

/// Height of the log list. A selected instance keeps a details block above the
/// log, so the list gives that block its max height back.
fn log_viewport(window: &HearthWindow) -> f32 {
    let mut height = window.view_h - 74.;
    if window.pane == Pane::Shared && window.shared_pick.is_some() {
        height -= SHARED_DETAIL_BLOCK;
    }
    height.max(120.)
}

/// Intro, shared daemon, then the three lists. Nothing in an action inserts a
/// block above those lists: pending work stays on the row that owns it.
fn shared_body(
    window: &mut HearthWindow,
    mut body: gpui::Div,
    cx: &mut Context<HearthWindow>,
) -> gpui::Div {
    body = body.child(shared_intro());
    body = body.child(shared_daemon_row(window, cx));
    let cache = &window.shared_cache;
    body = body.child(board_section("In use", cache.in_use.len()));
    if window.instances.is_empty() {
        body = body.child(awareness(
            "Add a `shared:` entry to a workspace's hearth.yaml, then start it. Or install a recipe below.",
        ));
    } else if cache.in_use.is_empty() {
        body = body.child(awareness("No instance is in use."));
    } else {
        for index in &cache.in_use {
            body = body.child(instance_row(window, *index, cx));
        }
    }
    body = body.child(board_section("Not in use", cache.idle.len()));
    if !window.instances.is_empty() && cache.idle.is_empty() {
        body = body.child(awareness("Every instance is in use."));
    } else {
        for index in &cache.idle {
            body = body.child(instance_row(window, *index, cx));
        }
    }
    body = body.child(board_section("Available to install", window.recipes.len()));
    if cache.recipe_groups.is_empty() {
        body = body.child(awareness("No recipes in the catalog."));
    }
    for group in &cache.recipe_groups {
        body = body.child(recipe_group(window, group, cx));
    }
    body
}

/// One or two sentences, always in the same place.
fn shared_intro() -> gpui::Div {
    let theme = theme();
    div()
        .px_2()
        .pt_1()
        .pb_2()
        .min_h(px(36.))
        .text_xs()
        .text_color(theme.muted)
        .child("Shared services run one copy of common infrastructure (databases, caches, storage) for all your workspaces. A workspace uses one by adding a `shared:` entry to its hearth.yaml.")
}

/// Status of smp, plus Refresh. "Start shared daemon" is only there while it is down.
/// The row height does not change between those two states.
fn shared_daemon_row(window: &HearthWindow, cx: &mut Context<HearthWindow>) -> gpui::Div {
    let theme = theme();
    let pending = shared_daemon_pending(&window.inflight);
    let ensure_dim = window.shared_board_locked();
    let refresh_dim = window.dialog_open() || window.busy;
    let (dot, label) = if window.smp_up {
        (theme.ok, "Shared daemon running")
    } else {
        (theme.err, "Shared daemon not running")
    };
    let dot = if pending { theme.warn } else { dot };
    let mut row = div()
        .h(px(DAEMON_ROW_H))
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .child(div().w(px(8.)).h(px(8.)).flex_none().rounded_full().bg(dot))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(label),
        )
        .child(pending_mark(pending, "smp-spin".into()));
    if !window.smp_up {
        row = row.child(
            shared_button(
                "shared-ensure",
                "play",
                "Start shared daemon",
                "Run hearth shared ensure. Instances start when a workspace needs them.",
                Weight::Accent,
                cx.listener(|this, _, _, cx| this.ensure_shared(cx)),
            )
            .when(ensure_dim, |button| button.opacity(0.4)),
        );
    }
    row.child(
        shared_button(
            "shared-refresh",
            "refresh",
            "Refresh",
            "Read the shared registry again.",
            Weight::Quiet,
            cx.listener(|this, _, _, cx| {
                if this.dialog_open() || this.busy {
                    return;
                }
                this.load_shared(cx);
            }),
        )
        .when(refresh_dim, |button| button.opacity(0.4)),
    )
}

/// A labelled button with a tooltip. The shared pane does not use a control that
/// only says its name.
fn shared_button(
    id: impl Into<SharedString>,
    glyph: &'static str,
    label: &'static str,
    tip: impl Into<SharedString>,
    weight: Weight,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme();
    let (rest, hot, wash) = match weight {
        Weight::Quiet => (theme.muted, theme.text, theme.hover),
        Weight::Accent => (theme.accent, theme.accent_hover, theme.selected),
        Weight::Danger => (theme.muted, theme.err, theme.err_bg),
    };
    let id = id.into();
    let tip = tip.into();
    let group = SharedString::from(format!("{id}-group"));
    div()
        .id(id)
        .group(group.clone())
        .role(Role::Button)
        .aria_label(tip.clone())
        .flex()
        .flex_none()
        .items_center()
        .gap_1p5()
        .h(px(24.))
        .px_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .cursor_pointer()
        .tab_index(0)
        .hover(move |style| {
            let style = style.bg(wash);
            if weight == Weight::Danger {
                style.border_color(hot)
            } else {
                style
            }
        })
        .focus(move |style| style.border_color(theme.accent))
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .tooltip(Tooltip::text(tip))
        .on_click(on_click)
        .child(
            Icon::new(ns_ui::icon_name(glyph))
                .size(IconSize::Xs)
                .color(rest)
                .group_hover_color(group.clone(), hot),
        )
        .child(
            div()
                .text_color(theme.text)
                .group_hover(group, move |style| {
                    if weight == Weight::Danger {
                        style.text_color(hot)
                    } else {
                        style
                    }
                })
                .child(label),
        )
}

/// One instance: name, state, ports, install failure, workspace count, and actions.
fn instance_row(
    window: &HearthWindow,
    index: usize,
    cx: &mut Context<HearthWindow>,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme();
    let instance = &window.instances[index];
    let text = &window.shared_cache.rows[index];
    let id = instance.id.clone();
    let marked = instance_marked(&id, &window.inflight);
    let row_dim = action_blocked(&window.inflight, &[&id], true, None);
    let state_label =
        display_instance_state(&instance.actual, &id, &window.inflight, window.smp_up);
    let color = state_color(&state_label);
    let up = matches!(
        instance.actual.as_str(),
        "ready" | "running" | "running-unready"
    );
    let selected = window.shared_pick == Some(index);
    let chrome = row_chrome(selected, marked, false);
    let force = !instance.attachments.is_empty();

    let mut actions = div().flex().items_center().gap_1();
    if up {
        let stop_id = id.clone();
        let tip = text.stop_tip.clone();
        let message = text.stop_message.clone();
        actions = actions.child(icon_button(
            SharedString::from(format!("inst-stop-{index}")),
            "stop",
            tip,
            IconTone::Danger,
            cx.listener(move |this, _, _, cx| {
                this.shared_pick = Some(index);
                if this.shared_target_locked(&stop_id) {
                    return;
                }
                let label = stop_id.clone();
                this.ask_for(
                    &format!("Stop {label}?"),
                    &message,
                    "Stop",
                    "stop",
                    true,
                    Pending::StopShared(label),
                    cx,
                );
            }),
        ));
    } else {
        let start_id = id.clone();
        let tip = text.start_tip.clone();
        actions = actions.child(icon_button(
            SharedString::from(format!("inst-start-{index}")),
            "play",
            tip,
            IconTone::Accent,
            cx.listener(move |this, _, _, cx| {
                this.shared_pick = Some(index);
                if this.shared_target_locked(&start_id) {
                    return;
                }
                let label = start_id.clone();
                this.ask_for(
                    &format!("Start {label}?"),
                    "This can start the shared daemon, then start the instance.",
                    "Start",
                    "play",
                    false,
                    Pending::StartShared(label),
                    cx,
                );
            }),
        ));
    }
    let restart_id = id.clone();
    let tip = text.restart_tip.clone();
    let restart_message = text.restart_message.clone();
    actions = actions.child(icon_button(
        SharedString::from(format!("inst-restart-{index}")),
        "restart",
        tip,
        IconTone::Quiet,
        cx.listener(move |this, _, _, cx| {
            this.shared_pick = Some(index);
            if this.shared_target_locked(&restart_id) {
                return;
            }
            let label = restart_id.clone();
            this.ask_for(
                &format!("Restart {label}?"),
                &restart_message,
                "Restart",
                "restart",
                false,
                Pending::RestartShared(label),
                cx,
            );
        }),
    ));
    let remove_id = id.clone();
    let tip = text.remove_tip.clone();
    let remove_message = text.remove_message.clone();
    actions = actions.child(icon_button(
        SharedString::from(format!("inst-remove-{index}")),
        "trash",
        tip,
        IconTone::Danger,
        cx.listener(move |this, _, _, cx| {
            this.shared_pick = Some(index);
            if this.shared_target_locked(&remove_id) {
                return;
            }
            let label = remove_id.clone();
            this.ask_for(
                &format!("Remove {label}?"),
                &remove_message,
                "Remove",
                "trash",
                true,
                Pending::RemoveShared { id: label, force },
                cx,
            );
        }),
    ));
    if row_dim {
        actions = actions.opacity(0.4);
    }

    let spin_id = SharedString::from(format!("inst-spin-{id}"));
    let hit = div()
        .id(SharedString::from(format!("inst-hit-{index}")))
        .h(px(28.))
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .child(div().w(px(8.)).h(px(8.)).flex_none().rounded_sm().bg(color))
        .child(
            div()
                .id(SharedString::from(format!("inst-name-{index}")))
                .flex_1()
                .min_w_0()
                .truncate()
                .tooltip(Tooltip::text(id.clone()))
                .child(id),
        )
        .child(
            div()
                .w(px(INSTANCE_STATE_COL))
                .flex_none()
                .flex()
                .items_center()
                .gap_1()
                .overflow_hidden()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(color)
                        .child(state_label),
                )
                .child(pending_mark(marked, spin_id)),
        )
        .child(actions)
        .on_click(cx.listener(move |this, _, _, cx| {
            this.shared_pick = Some(index);
            this.recipe_pick = None;
            this.open_shared_log(cx);
            ns_ui::show_dock();
        }));

    let mut column = div()
        .id(SharedString::from(format!("inst-{index}")))
        .px_2()
        .rounded_md()
        .flex()
        .flex_col()
        .border_l_2()
        .border_color(if chrome.accent_edge {
            theme.accent
        } else {
            gpui::Rgba {
                r: 0.,
                g: 0.,
                b: 0.,
                a: 0.,
            }
        })
        .when(chrome.pending_fill, |row| row.bg(theme.warn_bg))
        .when(chrome.selected_fill, |row| row.bg(theme.selected))
        .when(chrome.hover, |row| row.hover(|style| style.bg(theme.hover)))
        .child(hit)
        .child(
            div()
                .id(SharedString::from(format!("inst-facts-{index}")))
                .w_full()
                .min_h(px(22.))
                .px_2()
                .flex()
                .items_center()
                .text_xs()
                .text_color(theme.muted)
                .truncate()
                .tooltip(Tooltip::text(text.facts.clone()))
                .child(text.facts.clone()),
        );
    if let Some(error) = text.error.clone() {
        column = column.child(
            div()
                .id(SharedString::from(format!("inst-err-{index}")))
                .w_full()
                .h(px(18.))
                .px_2()
                .text_xs()
                .text_color(theme.err)
                .truncate()
                .tooltip(Tooltip::text(error.clone()))
                .child(error),
        );
    }
    column
}

/// One service name, with a chip and actions for each version.
fn recipe_group(
    window: &HearthWindow,
    group: &RecipeGroupCache,
    cx: &mut Context<HearthWindow>,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme();
    let mut row = div()
        .id(SharedString::from(format!("recipe-{}", group.name)))
        .w_full()
        .px_2()
        .py_1()
        .min_h(px(DAEMON_ROW_H))
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .rounded_md()
        .hover(|style| style.bg(theme.hover))
        .child(
            div()
                .id(SharedString::from(format!("recipe-name-{}", group.name)))
                .w(px(88.))
                .flex_none()
                .min_w_0()
                .truncate()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .tooltip(Tooltip::text(group.name.clone()))
                .child(group.name.clone()),
        );
    for entry in &group.versions {
        let index = entry.index;
        let id = entry.id.clone();
        let pending = install_pending(&id, &window.inflight);
        let row_dim = action_blocked(&window.inflight, &[&id], true, None);
        let install_id = id.clone();
        let copy_entry = entry.copy_entry.clone();
        let copy_tip = entry.copy_tip.clone();
        let install_label = if entry.installed {
            "Reinstall"
        } else {
            "Install"
        };
        let install_tip = entry.install_tip.clone();
        let mut actions = div().flex().items_center().gap_1().child(shared_button(
            SharedString::from(format!("recipe-install-{index}")),
            "download",
            install_label,
            install_tip,
            Weight::Accent,
            cx.listener(move |this, _, _, cx| {
                if this.shared_target_locked(&install_id) {
                    return;
                }
                let label = install_id.clone();
                this.ask_for(
                    &format!("Install {label}?"),
                    "This can start the shared daemon, then install the recipe.",
                    "Install",
                    "download",
                    false,
                    Pending::InstallShared(label),
                    cx,
                );
            }),
        ));
        actions = actions.child(shared_button(
            SharedString::from(format!("recipe-copy-{index}")),
            "copy",
            "Copy hearth.yaml entry",
            copy_tip,
            Weight::Quiet,
            cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(copy_entry.clone()));
                this.message("Copied", cx);
            }),
        ));
        if row_dim {
            actions = actions.opacity(0.4);
        }
        row = row.child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .h(px(28.))
                .child(
                    div()
                        .px_1p5()
                        .h(px(20.))
                        .flex()
                        .items_center()
                        .rounded_md()
                        .text_xs()
                        .bg(if entry.installed {
                            theme.ok_bg
                        } else {
                            theme.editor
                        })
                        .child(entry.version.clone()),
                )
                .child(
                    div()
                        .w(px(68.))
                        .flex_none()
                        .text_xs()
                        .text_color(theme.ok)
                        .child(if entry.installed { "installed" } else { "" }),
                )
                .child(pending_mark(
                    pending,
                    SharedString::from(format!("recipe-spin-{index}")),
                ))
                .child(actions),
        );
    }
    row
}

/// Selected instance: attachment details, then the same log pane as before.
fn shared_dock(window: &mut HearthWindow, cx: &mut Context<HearthWindow>) -> gpui::Div {
    let mut dock = div()
        .size_full()
        .flex()
        .flex_col()
        .min_h(px(0.))
        .overflow_hidden();
    if let Some(index) = window.shared_pick
        && index < window.instances.len()
    {
        dock = dock.child(instance_detail(window, index, cx));
    }
    let log = if window.log_shared && !window.log_service.is_empty() {
        log_pane(window, cx).into_any_element()
    } else if let Some(flight) = window.inflight.iter().rev().find(|job| job.shared) {
        action_panel(&flight.label, true).into_any_element()
    } else {
        log_placeholder("Select an instance to show its log.").into_any_element()
    };
    dock.child(div().flex_1().min_h(px(0.)).overflow_hidden().child(log))
}

fn instance_detail(
    window: &HearthWindow,
    index: usize,
    cx: &mut Context<HearthWindow>,
) -> gpui::Div {
    let theme = theme();
    let count = window.instances[index].attachments.len();
    let mut list = div().flex().flex_col().gap_1().px_3().pb_2();
    if count == 0 {
        list = list.child(
            div()
                .text_xs()
                .text_color(theme.muted)
                .child("No workspace uses this instance."),
        );
    }
    for att_index in 0..count {
        list = list.child(attachment_row(window, index, att_index, cx));
    }
    div()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .border_b_1()
        .border_color(theme.border)
        .bg(theme.panel)
        .child(
            div()
                .h(px(32.))
                .px_3()
                .flex()
                .items_center()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child("Used by"),
        )
        .child(
            ScrollArea::new("shared-used-by")
                .vertical()
                .max_h(px(DETAIL_LIST_MAX))
                .child(list),
        )
}

fn attachment_row(
    window: &HearthWindow,
    instance_index: usize,
    att_index: usize,
    cx: &mut Context<HearthWindow>,
) -> gpui::Div {
    let theme = theme();
    let attachment = &window.instances[instance_index].attachments[att_index];
    let from_path = ns_ui::path_name(&attachment.root);
    let name = if from_path.is_empty() {
        attachment.project_id.clone()
    } else {
        from_path
    };
    let path = ns_ui::display_path(&attachment.root);
    let root = attachment.root.clone();
    let known = find_workspace(&window.workspaces, &attachment.root).is_some();
    let mut head = div().flex().items_center().gap_2().min_h(px(24.)).child(
        div()
            .flex_none()
            .text_xs()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .child(name),
    );
    head = head.child(
        div()
            .id(SharedString::from(format!(
                "att-path-{instance_index}-{att_index}"
            )))
            .flex_1()
            .min_w_0()
            .truncate()
            .text_xs()
            .text_color(theme.muted)
            .tooltip(Tooltip::text(path.clone()))
            .child(path),
    );
    let date = attach_date(&attachment.attached_at);
    if !date.is_empty() {
        head = head.child(
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.muted)
                .child(format!("attached {date}")),
        );
    }
    if known {
        head = head.child(icon_button(
            SharedString::from(format!("att-open-{instance_index}-{att_index}")),
            "folder",
            "Show this workspace in Hearth",
            IconTone::Quiet,
            cx.listener(move |this, _, _, cx| {
                this.show_attached_workspace(&root, cx);
            }),
        ));
    }
    let mut block = div().flex().flex_col().gap_0p5().py_1().child(head);
    if let Some(url) = attachment.url.clone() {
        block = block.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .min_h(px(22.))
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "att-url-{instance_index}-{att_index}"
                        )))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .font_family(ns_ui::CODE_FONT)
                        .text_color(theme.text)
                        .tooltip(Tooltip::text(url.clone()))
                        .child(url.clone()),
                )
                .child(icon_button(
                    SharedString::from(format!("att-url-copy-{instance_index}-{att_index}")),
                    "copy",
                    "Copy connection URL",
                    IconTone::Quiet,
                    cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                        this.message("Copied", cx);
                    }),
                )),
        );
    }
    for (env_index, (key, value)) in attachment.env.iter().enumerate() {
        let secret = env_is_secret(key);
        let shown = env_shown(key, value);
        let tip = if secret {
            format!("{key} is hidden. Copy pastes the real value.")
        } else {
            shown.clone()
        };
        let copy_value = value.clone();
        let copy_tip = format!("Copy {key}");
        block = block.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .min_h(px(22.))
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "att-env-{instance_index}-{att_index}-{env_index}"
                        )))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .font_family(ns_ui::CODE_FONT)
                        .tooltip(Tooltip::text(tip))
                        .child(shown),
                )
                .child(icon_button(
                    SharedString::from(format!(
                        "att-env-copy-{instance_index}-{att_index}-{env_index}"
                    )),
                    "copy",
                    copy_tip,
                    IconTone::Quiet,
                    cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_value.clone()));
                        this.message("Copied", cx);
                    }),
                )),
        );
    }
    if let Some(error) = attachment.error.clone() {
        block = block.child(
            div()
                .id(SharedString::from(format!(
                    "att-err-{instance_index}-{att_index}"
                )))
                .h(px(18.))
                .text_xs()
                .text_color(theme.err)
                .truncate()
                .tooltip(Tooltip::text(error.clone()))
                .child(error),
        );
    }
    block
}
/// Title and count badge that head a shared-board section. The service board
/// uses the collapsible `group_header` with the same text and badge.
fn section_head(title: &str, count: usize) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(title.to_string()),
        )
        .child(Badge::new(SharedString::from(count.to_string())))
}

/// A labeled cluster of toolbar controls. The caption sits above the buttons so
/// daemon actions, service actions, and view options stay distinct when the
/// row wraps.
fn toolbar_group(caption: &str, controls: impl IntoElement) -> gpui::Div {
    let theme = theme();
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(
            div().flex().flex_none().items_center().child(
                div()
                    .text_xs()
                    .text_color(theme.muted)
                    .child(caption.to_string()),
            ),
        )
        .child(controls)
}

/// Diameter of the coloured mark inside the daemon badge. Fixed, so a colour
/// change does not move the buttons beside it.
const DAEMON_DOT: f32 = 8.;

/// Non-interactive daemon status. A circle the same size as the outline
/// buttons beside it: their 1px `theme.border` and no fill at rest, with the
/// dot centered. No click, no hover wash, the arrow cursor, and it is not on
/// the tab order. Hover shows `tip`.
fn daemon_badge(color: gpui::Rgba, tip: String) -> gpui::Stateful<gpui::Div> {
    let theme = theme();
    div()
        .id("daemon-badge")
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(TEXT_ACTION_HEIGHT))
        .h(px(TEXT_ACTION_HEIGHT))
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .cursor_default()
        .tooltip(Tooltip::text(tip))
        .child(
            div()
                .w(px(DAEMON_DOT))
                .h(px(DAEMON_DOT))
                .flex_none()
                .rounded_full()
                .bg(color),
        )
}

/// Colour of the daemon status dot.
///
/// `working` means a daemon command is in flight. That wins over whether the
/// daemon is up. A blank in-flight label still counts: the caller passes
/// `label.is_some()`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DaemonDotTone {
    Running,
    Stopped,
    Working,
}

/// Sidebar dot for one workspace: daemon state first, then how its services
/// are doing. A daemon that is up but has failed services shows warn, so a
/// row the user never opened still signals trouble.
fn workspace_status_text(up: bool, counts: &Counts) -> (Tone, String) {
    if !up {
        return (Tone::Err, "Daemon stopped".to_string());
    }
    let mut text = format!("Daemon running · {} ready", counts.ready);
    if counts.failed > 0 {
        text.push_str(&format!(" · {} failed", counts.failed));
        return (Tone::Warn, text);
    }
    (Tone::Ok, text)
}

/// `workspace_status_text` for one read board.
fn board_status(board: &Board, hide_disabled: bool) -> (Tone, String) {
    let counts = count_services(&board.services, &board.catalog.disabled, hide_disabled);
    workspace_status_text(board.daemon_up, &counts)
}

fn daemon_dot_tone(up: bool, working: bool) -> DaemonDotTone {
    if working {
        DaemonDotTone::Working
    } else if up {
        DaemonDotTone::Running
    } else {
        DaemonDotTone::Stopped
    }
}

/// Tooltip for the daemon status badge.
///
/// `working` is the in-flight label (`Some("")` reads as "Daemon working…").
/// A running daemon names `port` when it has one. The note, when it has text,
/// is a second line: `"<msg>\nexit N"` becomes `"<msg> (exit N)"`, and any
/// other note joins its lines with ` · `. Blank lines are dropped. Every
/// other word from the note stays.
fn daemon_tooltip(up: bool, working: Option<&str>, port: Option<u64>, note: &str) -> String {
    let status = if let Some(label) = working {
        let label = label.trim();
        if label.is_empty() {
            "Daemon working…".to_string()
        } else {
            label.to_string()
        }
    } else if up {
        match port {
            Some(port) => format!("Daemon running · port {port}"),
            None => "Daemon running".to_string(),
        }
    } else {
        "Daemon stopped".to_string()
    };
    let note = format_daemon_note(note);
    if note.is_empty() {
        status
    } else {
        format!("{status}\n{note}")
    }
}

/// One line of daemon-note text. A trailing `exit N` after a single message
/// is wrapped as `(exit N)`. Everything else is joined with ` · `.
fn format_daemon_note(note: &str) -> String {
    let lines: Vec<&str> = note
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    match lines.as_slice() {
        [] => String::new(),
        [line] => (*line).to_string(),
        [message, exit] if exit_status_line(exit) => format!("{message} ({exit})"),
        lines => lines.join(" · "),
    }
}

fn exit_status_line(line: &str) -> bool {
    let Some(code) = line.strip_prefix("exit ") else {
        return false;
    };
    !code.is_empty() && code.bytes().all(|byte| byte.is_ascii_digit())
}

fn board_section(title: &str, count: usize) -> gpui::Div {
    div()
        .mt_2()
        .px_2()
        .h(px(28.))
        .flex()
        .items_center()
        .justify_between()
        .child(section_head(title, count))
}

const HIDE_DISABLED_KEY: &str = "hearth.hideDisabledServices";

fn read_hide_disabled() -> bool {
    let Ok(output) = std::process::Command::new("defaults")
        .args(["read", "ns.hearth", HIDE_DISABLED_KEY])
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    matches!(text.trim(), "1" | "true" | "YES")
}

fn write_hide_disabled(value: bool) {
    let _ = std::process::Command::new("defaults")
        .args([
            "write",
            "ns.hearth",
            HIDE_DISABLED_KEY,
            "-bool",
            if value { "true" } else { "false" },
        ])
        .status();
}

/// `shared installed` masks secrets unless `--reveal`. Copy pastes these values.
fn shared_installed_args() -> [&'static str; 4] {
    ["shared", "installed", "--json", "--reveal"]
}

/// `shared status` masks secrets unless `--reveal`. Copy pastes these values.
fn shared_status_args() -> [&'static str; 4] {
    ["shared", "status", "--json", "--reveal"]
}

/// This window is not a TTY, so `shared remove` exits 6 without `--yes`.
fn shared_remove_args(id: &str, force: bool) -> Vec<String> {
    let mut args = vec![
        "shared".into(),
        "remove".into(),
        id.into(),
        "--json".into(),
        "--yes".into(),
    ];
    if force {
        args.push("--force".into());
    }
    args
}

/// This window is not a TTY, so `manager stop` exits 6 without `--yes`.
fn manager_args(verb: &'static str) -> Vec<&'static str> {
    let mut args = vec!["manager", verb];
    if verb == "stop" {
        args.push("--yes");
    }
    args
}

/// This window is not a TTY, so `start --kill-unowned` exits 6 without `--yes`.
fn target_args(verb: &str, id: &str, kill_unowned: bool) -> Vec<String> {
    let mut args = vec![verb.to_string(), id.to_string()];
    if kill_unowned {
        args.push("--kill-unowned".into());
        args.push("--yes".into());
    }
    args
}

fn hearth(root: Option<&str>, args: &[&str]) -> ns_ui::ToolResult {
    let mut owned = Vec::new();
    if let Some(root) = root {
        owned.push("--root".into());
        owned.push(root.into());
    }
    owned.extend(args.iter().map(|arg| (*arg).to_string()));
    ns_ui::run_tool("hearth", &owned)
}

fn run_targets(root: &str, verb: &str, ids: &[String], kill_unowned: bool) -> Report {
    let mut failed = Vec::new();
    for id in ids {
        let args = target_args(verb, id, kill_unowned);
        let result = hearth(
            Some(root),
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        if !result.ok {
            failed.push(format!(
                "{id}: {}",
                result.text().lines().next().unwrap_or("failed")
            ));
        }
    }
    if failed.is_empty() {
        Report {
            ok: true,
            text: String::new(),
        }
    } else {
        Report {
            ok: false,
            text: failed.join("\n"),
        }
    }
}

#[derive(Deserialize)]
struct StatusFile {
    #[serde(default)]
    services: Vec<RawService>,
}

#[derive(Deserialize)]
struct RawService {
    #[serde(rename = "serviceId")]
    id: String,
    #[serde(rename = "actualState", default)]
    actual_state: String,
}

#[derive(Deserialize)]
struct UrlFile {
    #[serde(default)]
    urls: Vec<RawUrl>,
}

#[derive(Deserialize)]
struct RawUrl {
    #[serde(rename = "serviceId", default)]
    service: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "requiresRunning", default = "default_true")]
    requires_running: bool,
    #[serde(default)]
    running: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct ManagerStatus {
    port: Option<u64>,
}

/// Handle to one `hearth events --follow` child. The reader thread blocks on
/// the child's stdout, so a quiet stream would never notice `stop`; dropping
/// or stopping the handle kills the child, which closes stdout and ends the
/// thread. The child sits in the shared slot until the thread reaps it, so a
/// kill never targets an already-waited process. An unexpected child exit is
/// respawned after a short pause; only `stop` or a dropped receiver ends it.
struct DaemonWatch {
    shared: Arc<WatchShared>,
}

#[derive(Default)]
struct WatchShared {
    stop: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl WatchShared {
    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut slot) = self.child.lock()
            && let Some(child) = slot.as_mut()
        {
            let _ = child.kill();
        }
    }
}

impl DaemonWatch {
    fn spawn(root: String, sender: mpsc::Sender<()>) -> Self {
        let shared = Arc::new(WatchShared::default());
        let worker = shared.clone();
        thread::spawn(move || watch_daemon_events(root, &worker, sender));
        Self { shared }
    }
}

impl Drop for DaemonWatch {
    fn drop(&mut self) {
        self.shared.stop();
    }
}

fn watch_daemon_events(root: String, shared: &WatchShared, sender: mpsc::Sender<()>) {
    // `hearth events` only exits when killed, but a crash must not leave the
    // workspace dark: respawn until stopped or the receiver is gone.
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        if !stream_daemon_events(&root, shared, &sender) {
            return;
        }
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// Runs one `hearth events --follow` child to completion. False ends the whole
/// watch (stop requested, poisoned lock, or the receiver is gone); true asks
/// for a respawn.
fn stream_daemon_events(root: &str, shared: &WatchShared, sender: &mpsc::Sender<()>) -> bool {
    let mut command = ProcessCommand::new(ns_ui::tool_path("hearth"));
    command.args(["--root", root, "events", "--follow", "--json"]);
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else {
        return true;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return true;
    };
    {
        // `stop` is set before the stopper locks, so checking it under the
        // lock cannot miss a stop that raced the spawn.
        let Ok(mut slot) = shared.child.lock() else {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        };
        if shared.stop.load(Ordering::Relaxed) {
            drop(slot);
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        *slot = Some(child);
    }
    let mut gate = EventGate::default();
    let mut receiver_gone = false;
    for line in BufReader::new(stdout).lines() {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(line) = line else {
            break;
        };
        if gate.accept(&line) && sender.send(()).is_err() {
            receiver_gone = true;
            break;
        }
    }
    let child = shared.child.lock().ok().and_then(|mut slot| slot.take());
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait();
    }
    !(receiver_gone || shared.stop.load(Ordering::Relaxed))
}

/// Edge-triggers daemon-unavailable notifications: the follow stream repeats
/// `unavailable` (plus `connection`) while the manager is down, and each
/// invalidation reloads the board, so only the first outage line may pass.
#[derive(Default)]
struct EventGate {
    unavailable: bool,
}

impl EventGate {
    fn accept(&mut self, line: &str) -> bool {
        if !event_invalidates_board(line) {
            return false;
        }
        let unavailable = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|value| {
                value
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(|kind| kind == "unavailable")
            })
            .unwrap_or(false);
        if unavailable {
            !std::mem::replace(&mut self.unavailable, true)
        } else {
            self.unavailable = false;
            true
        }
    }
}

fn event_invalidates_board(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    if matches!(
        value.get("kind").and_then(Value::as_str),
        Some("snapshot") | Some("unavailable")
    ) {
        return true;
    }
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("service.lifecycle")
            | Some("manager.catalog-reloaded")
            | Some("manager.stopped")
            | Some("operation.accepted")
            | Some("operation.updated")
    )
}

fn load_board(root: &str) -> Board {
    let status = hearth(Some(root), &["status", "--json"]);
    let urls = hearth(Some(root), &["urls", "--json"]);
    let manager = hearth(Some(root), &["manager", "status"]);
    let catalog_text = ["hearth.yaml", "hearth.yml"]
        .iter()
        .find_map(|name| std::fs::read_to_string(Path::new(root).join(name)).ok())
        .unwrap_or_default();
    let catalog = catalog::parse_catalog(&catalog_text);
    let (services, note) = match serde_json::from_str::<StatusFile>(&status.stdout) {
        Ok(file) => {
            let mut services: Vec<Service> = file
                .services
                .into_iter()
                .map(|service| Service {
                    id: service.id,
                    state: service.actual_state,
                })
                .collect();
            sort_services(&mut services);
            (services, String::new())
        }
        Err(_) => (stopped_services(&catalog), status.text()),
    };
    let url_rows = serde_json::from_str::<UrlFile>(&urls.stdout)
        .map(|file| file.urls)
        .unwrap_or_default();
    let urls = url_rows
        .into_iter()
        .map(|row| UrlRow {
            visible: !row.requires_running || row.running,
            service: row.service,
            label: row.label,
            url: row.url,
        })
        .collect();
    let (daemon_up, port, daemon_note) =
        match serde_json::from_str::<ManagerStatus>(&manager.stdout) {
            Ok(info) if manager.ok => (true, info.port, String::new()),
            _ => (false, None, manager.text()),
        };
    // A stopped daemon is a normal state: the rows already show every service
    // stopped and the daemon badge tooltip says why, so `status` failing
    // with "manager is unavailable" must not raise a warning.
    let note = if daemon_up { note } else { String::new() };
    Board {
        services,
        urls,
        catalog,
        daemon_up,
        port,
        daemon_note,
        note,
        stamp: catalog_stamp(root),
    }
}

fn catalog_stamp(root: &str) -> Option<u64> {
    for name in ["hearth.yaml", "hearth.yml"] {
        let Ok(meta) = std::fs::metadata(Path::new(root).join(name)) else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
            return Some(since.as_secs());
        }
    }
    None
}

struct SharedBoard {
    recipes: Vec<(String, String)>,
    instances: Vec<Instance>,
    smp_up: bool,
}

fn load_shared_board() -> SharedBoard {
    // `shared list` masks catalog secrets unless `--reveal`.
    let list = hearth(None, &["shared", "list", "--json", "--reveal"]);
    let installed = hearth(None, &shared_installed_args());
    let status = hearth(None, &shared_status_args());
    let mut instances = parse_instances(&installed.stdout);
    // `installed` is the local registry and still answers when smp is down.
    // `status` is the live view. A non-zero exit ("smp is not running") leaves
    // states empty so the pane can say Unknown.
    let smp_up = status.ok;
    if smp_up {
        merge_live_instances(&mut instances, parse_instances(&status.stdout));
    }
    apply_catalog_labels(&mut instances, &catalog_port_labels(&list.stdout));
    sort_instances(&mut instances);
    SharedBoard {
        recipes: parse_recipes(&list.stdout),
        instances,
        smp_up,
    }
}

fn parse_recipes(text: &str) -> Vec<(String, String)> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(services) = value.get("services").and_then(|item| item.as_object()) else {
        return Vec::new();
    };
    let mut recipes = Vec::new();
    for (name, body) in services {
        if let Some(versions) = body.get("versions").and_then(|item| item.as_object()) {
            for version in versions.keys() {
                recipes.push((name.clone(), version.clone()));
            }
        }
    }
    recipes.sort();
    recipes
}

fn parse_instances(text: &str) -> Vec<Instance> {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(list) = value.get("instances").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut instances: Vec<Instance> = list
        .iter()
        .filter_map(|item| {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let version = item.get("version").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() || version.is_empty() {
                return None;
            }
            let attachments = item
                .get("attachments")
                .map(parse_attachments)
                .unwrap_or_default();
            Some(Instance {
                id: format!("{name}@{version}"),
                port: item
                    .get("port")
                    .and_then(Value::as_u64)
                    .filter(|port| *port > 0),
                extra_ports: item
                    .get("extraPorts")
                    .map(json_u64_list)
                    .unwrap_or_default(),
                extra_labels: recipe_labels(item),
                install: item
                    .get("installState")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                install_error: json_opt_string(item, "installError"),
                actual: item
                    .get("state")
                    .and_then(|state| state.get("actualState"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                attachments,
            })
        })
        .collect();
    sort_instances(&mut instances);
    instances
}

/// Live rows carry `actualState`. They do not carry attach dates, so a known
/// instance keeps the attachment list that `installed` already parsed.
fn merge_live_instances(installed: &mut Vec<Instance>, live: Vec<Instance>) {
    for live_row in live {
        match installed.iter_mut().find(|row| row.id == live_row.id) {
            Some(row) => {
                row.actual = live_row.actual;
                if row.port.is_none() {
                    row.port = live_row.port;
                }
                if row.extra_ports.is_empty() {
                    row.extra_ports = live_row.extra_ports;
                }
                if row.extra_labels.is_empty() {
                    row.extra_labels = live_row.extra_labels;
                }
                if live_row.install_error.is_some() {
                    row.install_error = live_row.install_error;
                }
                if !live_row.install.is_empty() {
                    row.install = live_row.install;
                }
            }
            None => installed.push(live_row),
        }
    }
}

fn sort_instances(instances: &mut [Instance]) {
    instances.sort_by(|left, right| left.id.cmp(&right.id));
}

fn recipe_labels(item: &Value) -> Vec<String> {
    item.get("recipe")
        .and_then(|recipe| recipe.get("extraPortLabels"))
        .map(json_strings)
        .unwrap_or_default()
}

fn catalog_port_labels(text: &str) -> HashMap<(String, String), Vec<String>> {
    let mut out = HashMap::new();
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return out;
    };
    let Some(services) = value.get("services").and_then(Value::as_object) else {
        return out;
    };
    for (name, body) in services {
        let Some(versions) = body.get("versions").and_then(Value::as_object) else {
            continue;
        };
        for (version, recipe) in versions {
            let labels = recipe
                .get("extraPortLabels")
                .map(json_strings)
                .unwrap_or_default();
            if !labels.is_empty() {
                out.insert((name.clone(), version.clone()), labels);
            }
        }
    }
    out
}

fn apply_catalog_labels(
    instances: &mut [Instance],
    labels: &HashMap<(String, String), Vec<String>>,
) {
    for instance in instances {
        if !instance.extra_labels.is_empty() {
            continue;
        }
        let Some((name, version)) = instance.id.split_once('@') else {
            continue;
        };
        let key = (name.to_string(), version.to_string());
        if let Some(found) = labels.get(&key) {
            instance.extra_labels.clone_from(found);
        }
    }
}

fn parse_attachments(value: &Value) -> Vec<Attachment> {
    let mut out = Vec::new();
    if let Some(map) = value.as_object() {
        for (project_id, body) in map {
            if let Some(item) = parse_attachment(project_id, body) {
                out.push(item);
            }
        }
    } else if let Some(list) = value.as_array() {
        for body in list {
            let project_id = body.get("projectId").and_then(Value::as_str).unwrap_or("");
            if let Some(item) = parse_attachment(project_id, body) {
                out.push(item);
            }
        }
    }
    out.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then_with(|| left.project_id.cmp(&right.project_id))
    });
    out
}

fn parse_attachment(project_id: &str, body: &Value) -> Option<Attachment> {
    if !body.is_object() {
        return None;
    }
    let root = json_opt_string(body, "projectRoot").unwrap_or_default();
    let id = if project_id.is_empty() {
        json_opt_string(body, "projectId").unwrap_or_default()
    } else {
        project_id.to_string()
    };
    if root.is_empty() && id.is_empty() {
        return None;
    }
    let connection = body.get("connection");
    Some(Attachment {
        project_id: id,
        root,
        attached_at: json_opt_string(body, "attachedAt").unwrap_or_default(),
        url: connection.and_then(|item| json_opt_string(item, "url")),
        env: connection.map(parse_env).unwrap_or_default(),
        error: json_opt_string(body, "error"),
    })
}

fn parse_env(connection: &Value) -> Vec<(String, String)> {
    let Some(env) = connection.get("env").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    for (key, value) in env {
        let Some(text) = json_scalar(value) else {
            continue;
        };
        pairs.push((key.clone(), text));
    }
    pairs.sort_by(|left, right| left.0.cmp(&right.0));
    pairs
}

fn json_opt_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn json_scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn json_strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn json_u64_list(value: &Value) -> Vec<u64> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_u64)
                .filter(|port| *port > 0)
                .collect()
        })
        .unwrap_or_default()
}

fn group_recipes(recipes: &[(String, String)]) -> Vec<RecipeGroup> {
    let mut groups: Vec<RecipeGroup> = Vec::new();
    for (name, version) in recipes {
        if let Some(group) = groups.iter_mut().find(|group| group.name == *name) {
            if !group.versions.iter().any(|item| item == version) {
                group.versions.push(version.clone());
            }
        } else {
            groups.push(RecipeGroup {
                name: name.clone(),
                versions: vec![version.clone()],
            });
        }
    }
    for group in &mut groups {
        group.versions.sort();
    }
    groups.sort_by(|left, right| left.name.cmp(&right.name));
    groups
}

fn recipe_index(recipes: &[(String, String)], name: &str, version: &str) -> Option<usize> {
    recipes
        .iter()
        .position(|(item_name, item_version)| item_name == name && item_version == version)
}

/// Reference split used by tests; the render path reads the shared cache.
#[cfg(test)]
fn split_instances(instances: &[Instance]) -> (Vec<usize>, Vec<usize>) {
    let mut in_use = Vec::new();
    let mut idle = Vec::new();
    for (index, instance) in instances.iter().enumerate() {
        if instance.attachments.is_empty() {
            idle.push(index);
        } else {
            in_use.push(index);
        }
    }
    (in_use, idle)
}

/// `port 43721 · console 43722`. A missing label reads `port 2`, `port 3`, …
fn port_line(port: Option<u64>, extras: &[u64], labels: &[String]) -> String {
    let mut parts = Vec::new();
    if let Some(port) = port {
        parts.push(format!("port {port}"));
    }
    for (index, extra) in extras.iter().enumerate() {
        let label = labels
            .get(index)
            .map(String::as_str)
            .filter(|label| !label.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("port {}", index + 2));
        parts.push(format!("{label} {extra}"));
    }
    parts.join(" · ")
}

fn display_instance_state(actual: &str, id: &str, jobs: &[InFlight], smp_up: bool) -> String {
    let shown = instance_state(actual, id, jobs);
    if !shown.is_empty() {
        shown.to_string()
    } else if smp_up {
        "—".into()
    } else {
        "Unknown".into()
    }
}

fn install_pending(id: &str, jobs: &[InFlight]) -> bool {
    jobs.iter()
        .any(|job| job.shared && job.target == id && job.label.starts_with("Installing"))
}

fn shared_daemon_pending(jobs: &[InFlight]) -> bool {
    jobs.iter()
        .any(|job| job.shared && job.daemon && job.target == "shared daemon")
}

fn env_is_secret(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.contains("SECRET") || key.contains("PASSWORD") || key.contains("TOKEN")
}

/// Masked text for the pane. Copy buttons pass `value` through unchanged.
fn env_shown(key: &str, value: &str) -> String {
    if env_is_secret(key) {
        format!("{key}=••••••")
    } else {
        format!("{key}={value}")
    }
}

fn hearth_yaml_entry(name: &str, version: &str) -> String {
    format!("shared:\n  {name}: \"{version}\"")
}

fn attach_date(value: &str) -> &str {
    value
        .split(['T', ' '])
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or(value)
}

fn workspace_count(count: usize) -> String {
    match count {
        1 => "1 workspace".into(),
        n => format!("{n} workspaces"),
    }
}

fn show_install(install: &str) -> bool {
    !install.is_empty() && install != "installed"
}

fn loss_sentence(count: usize, tail: &str) -> String {
    match count {
        0 => "No workspace uses it.".into(),
        1 => format!("1 workspace loses it{tail}"),
        n => format!("{n} workspaces lose it{tail}"),
    }
}

fn stop_tip(id: &str, count: usize) -> String {
    format!(
        "Stop {id}. {}",
        loss_sentence(count, " until it starts again.")
    )
}

fn start_tip(id: &str) -> String {
    format!("Start {id}. This can start the shared daemon, then start the instance.")
}

fn restart_tip(id: &str, count: usize) -> String {
    format!(
        "Restart {id}. It stops, then starts. {}",
        loss_sentence(count, " until it is back.")
    )
}

fn remove_tip(id: &str, count: usize) -> String {
    format!(
        "Remove {id}. The instance leaves this machine. {}",
        loss_sentence(count, ".")
    )
}

fn path_block(instance: &Instance) -> String {
    instance
        .attachments
        .iter()
        .map(|item| ns_ui::display_path(&item.root))
        .collect::<Vec<_>>()
        .join("\n")
}

fn stop_confirm(instance: &Instance) -> String {
    if instance.attachments.is_empty() {
        "No workspace uses this instance.".into()
    } else {
        format!(
            "{}\n{}",
            loss_sentence(instance.attachments.len(), " until it starts again."),
            path_block(instance)
        )
    }
}

fn remove_confirm(instance: &Instance) -> String {
    if instance.attachments.is_empty() {
        "The instance leaves this machine. No workspace uses this instance.".into()
    } else {
        format!(
            "The instance leaves this machine.\n{}",
            path_block(instance)
        )
    }
}

fn root_key(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() { path } else { trimmed }
}

fn find_workspace(workspaces: &[store::Workspace], root: &str) -> Option<usize> {
    let want = root_key(root);
    workspaces
        .iter()
        .position(|row| root_key(&row.path) == want)
}

fn sort_services(services: &mut [Service]) {
    services.sort_by(|left, right| {
        state_rank(&left.state)
            .cmp(&state_rank(&right.state))
            .then_with(|| left.id.cmp(&right.id))
    });
}

/// Catalog rows for a daemon that did not return status JSON. `stopped` is the
/// same stand-in the previous window used when a catalog id had no live row.
fn stopped_services(catalog: &Catalog) -> Vec<Service> {
    let mut services: Vec<Service> = offline_ids(catalog)
        .into_iter()
        .map(|id| Service {
            id,
            state: "stopped".into(),
        })
        .collect();
    sort_services(&mut services);
    services
}

/// Live rows show Stop. Everything else, including a failed or externally owned
/// row, shows Start in that same slot.
fn service_shows_stop(state: &str) -> bool {
    matches!(
        state,
        "queued-start"
            | "preparing"
            | "starting"
            | "running"
            | "running-unready"
            | "ready"
            | "stopping"
    )
}

fn state_rank(state: &str) -> u8 {
    match state {
        "failed" | "crashed" => 0,
        "starting" | "stopping" | "preparing" | "queued-start" => 1,
        "externally-owned" => 2,
        "ready" | "running" | "running-unready" => 3,
        _ => 4,
    }
}

fn state_color(state: &str) -> gpui::Rgba {
    let theme = theme();
    match state {
        "ready" | "running" => theme.ok,
        "failed" | "crashed" => theme.err,
        "starting" | "stopping" | "restarting" | "preparing" | "running-unready" => theme.warn,
        _ => theme.muted,
    }
}

fn inflight(verb: &'static str, target: &str, ids: Vec<String>, daemon: bool) -> InFlight {
    InFlight {
        id: 0,
        label: action_label(verb, target),
        verb,
        target: target.to_string(),
        ids,
        daemon,
        root: None,
        clear_after: None,
        shared: false,
    }
}

fn action_label(verb: &str, target: &str) -> String {
    let verb = match verb {
        "stop" => "Stopping",
        "restart" => "Restarting",
        _ => "Starting",
    };
    format!("{verb} {target}…")
}

fn pending_state_for(verb: &str) -> &'static str {
    match verb {
        "stop" => "stopping",
        "restart" => "restarting",
        _ => "starting",
    }
}

/// One id stays itself, including a group name. `all` and a bulk list read as every service.
fn action_target(ids: &[String]) -> String {
    if ids.len() == 1 && ids[0] != "all" {
        ids[0].clone()
    } else {
        "all services".into()
    }
}

/// Group names expand to their leaves. Disabled rows and unknown ids stay out.
fn affected_ids(
    targets: &[String],
    groups: &[(String, Vec<String>)],
    disabled: &[String],
    services: &[Service],
) -> Vec<String> {
    let mut out = Vec::new();
    for target in targets {
        let members = if groups.iter().any(|(name, _)| name == target) {
            expand(target, groups)
        } else {
            vec![target.clone()]
        };
        for member in members {
            if disabled.iter().any(|item| item == &member) {
                continue;
            }
            if services.iter().any(|service| service.id == member)
                && !out.iter().any(|item| item == &member)
            {
                out.push(member);
            }
        }
    }
    out
}

fn stopping_service_ids(services: &[Service], disabled: &[String]) -> Vec<String> {
    services
        .iter()
        .filter(|service| {
            service_shows_stop(&service.state) && !disabled.iter().any(|item| item == &service.id)
        })
        .map(|service| service.id.clone())
        .collect()
}

/// How a board row is painted. Selection is decided on its own, then the
/// pending and failed washes fill only the rows that are not the cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct RowChrome {
    selected_fill: bool,
    pending_fill: bool,
    failed_fill: bool,
    hover: bool,
    /// Accent left edge. The failed stripe keeps that edge when the row failed.
    accent_edge: bool,
    failed_edge: bool,
}

fn row_chrome(selected: bool, pending: bool, failed: bool) -> RowChrome {
    RowChrome {
        selected_fill: selected,
        pending_fill: pending && !selected,
        failed_fill: failed && !pending && !selected,
        hover: !selected && !pending && !failed,
        accent_edge: selected && !failed,
        failed_edge: failed,
    }
}

/// A workspace job with no recorded root still counts. Callers pass `None`
/// when the board is the shared registry or a test flight has no folder yet.
fn same_board(job: &InFlight, shared: bool, root: Option<&str>) -> bool {
    if job.shared != shared {
        return false;
    }
    if shared {
        return true;
    }
    match root {
        None => true,
        Some(root) => job.root.as_deref().is_none_or(|job_root| job_root == root),
    }
}

fn job_is_all(job: &InFlight) -> bool {
    job.target == "all services" || job.target == "all" || job.ids.iter().any(|id| id == "all")
}

fn board_has_job(jobs: &[InFlight], shared: bool, root: Option<&str>) -> bool {
    jobs.iter().any(|job| same_board(job, shared, root))
}

/// A daemon command or an "all" command blocks every target on its board.
/// Anything else blocks only the ids it marks, plus its own target name.
fn action_blocked(jobs: &[InFlight], targets: &[&str], shared: bool, root: Option<&str>) -> bool {
    jobs.iter().any(|job| {
        if !same_board(job, shared, root) {
            return false;
        }
        if job.daemon || job_is_all(job) {
            return true;
        }
        targets
            .iter()
            .any(|target| job.target == *target || job.ids.iter().any(|id| id == *target))
    })
}

fn flight_conflicts(jobs: &[InFlight], next: &InFlight) -> bool {
    let root = next.root.as_deref();
    if next.daemon || job_is_all(next) {
        return board_has_job(jobs, next.shared, root);
    }
    let mut targets = Vec::with_capacity(1 + next.ids.len());
    targets.push(next.target.as_str());
    for id in &next.ids {
        targets.push(id.as_str());
    }
    action_blocked(jobs, &targets, next.shared, root)
}

fn remove_job(jobs: &mut Vec<InFlight>, id: u64) {
    jobs.retain(|job| job.id != id);
}

/// Drop marks whose command finished at or before `generation` on one board.
/// The other board, and commands still running, stay.
fn clear_applied(jobs: &mut Vec<InFlight>, generation: u64, shared: bool) {
    jobs.retain(|job| {
        if job.shared != shared {
            return true;
        }
        !matches!(job.clear_after, Some(mark) if mark <= generation)
    });
}

fn row_state<'a>(actual: &'a str, id: &str, jobs: &[InFlight], root: Option<&str>) -> &'a str {
    match jobs
        .iter()
        .rev()
        .find(|job| same_board(job, false, root) && job.ids.iter().any(|item| item == id))
    {
        Some(job) => pending_state_for(job.verb),
        None => actual,
    }
}

fn row_marked(id: &str, jobs: &[InFlight], root: Option<&str>) -> bool {
    jobs.iter()
        .any(|job| same_board(job, false, root) && job.ids.iter().any(|item| item == id))
}

/// A shared flight names the instance in `target` and leaves `ids` empty.
/// Daemon flights and workspace flights stay out of the instance row.
fn instance_marked(id: &str, jobs: &[InFlight]) -> bool {
    jobs.iter()
        .any(|job| job.shared && !job.daemon && job.target == id)
}

fn instance_state<'a>(actual: &'a str, id: &str, jobs: &[InFlight]) -> &'a str {
    match jobs
        .iter()
        .rev()
        .find(|job| job.shared && !job.daemon && job.target == id)
    {
        Some(job) => pending_state_for(job.verb),
        None => actual,
    }
}

/// A finished command refreshes the board. Only a failure is worth a dialog.
fn failure_notice(report: &Report) -> Option<String> {
    let text = report.text.trim();
    if report.ok || text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Accent status text. One command names itself. Several that share a verb
/// read as "Restarting api, web…". A plain reload says Working.
fn busy_status(jobs: &[InFlight], busy: bool) -> String {
    match jobs {
        [] if busy => "Working".into(),
        [] => "Ready".into(),
        [job] => job.label.clone(),
        many => summarise_jobs(many),
    }
}

fn summarise_jobs(jobs: &[InFlight]) -> String {
    let newest = &jobs[jobs.len() - 1];
    let uniform = jobs
        .iter()
        .all(|job| job.verb == newest.verb && job.label == action_label(job.verb, &job.target));
    if !uniform {
        return newest.label.clone();
    }
    let verb = match newest.verb {
        "stop" => "Stopping",
        "restart" => "Restarting",
        _ => "Starting",
    };
    let mut names: Vec<&str> = Vec::new();
    for job in jobs {
        if !names.contains(&job.target.as_str()) {
            names.push(job.target.as_str());
        }
    }
    let shown = if names.len() > 3 { &names[..3] } else { &names };
    format!("{verb} {}…", shown.join(", "))
}

/// First line is the dialog title. Further lines are the detail. Blank text is skipped.
/// `svc:<verb>:<id>` from a palette row. The id may itself contain colons
/// (`postgres@16.4` does not, but a group name could), so only the first two
/// separators split. Anything else, or an unknown verb, is not a command.
fn parse_service_command(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix("svc:")?;
    let (verb, id) = rest.split_once(':')?;
    if id.is_empty() || !matches!(verb, "start" | "stop" | "restart" | "log") {
        return None;
    }
    Some((verb, id))
}

/// True when the folder on screen is no longer the folder being opened.
fn workspace_root_changed(previous: Option<&str>, next: Option<&str>) -> bool {
    previous != next
}

/// Service counts for the rows on screen. Disabled services the list hides are
/// counted apart, so the totals match what the user can see.
#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    ready: usize,
    stopped: usize,
    failed: usize,
    hidden: usize,
}

fn count_services(services: &[Service], disabled: &[String], hide_disabled: bool) -> Counts {
    let mut counts = Counts::default();
    for service in services {
        if hide_disabled && disabled.iter().any(|item| item == &service.id) {
            counts.hidden += 1;
            continue;
        }
        match service.state.as_str() {
            "ready" | "running" | "running-unready" => counts.ready += 1,
            "failed" | "crashed" => counts.failed += 1,
            _ => counts.stopped += 1,
        }
    }
    counts
}

/// `viclass · 10 ready · 1 stopped`. Failures get their own badge in the status
/// bar, so they are left out of the text.
fn scope_text(workspace: &str, counts: &Counts) -> String {
    let mut text = format!(
        "{workspace} · {} ready · {} stopped",
        counts.ready, counts.stopped
    );
    if counts.hidden > 0 {
        text.push_str(&format!(" · {} disabled hidden", counts.hidden));
    }
    text
}

fn main() {
    ns_ui::use_tint(ns_ui::Tint::Hearth);
    #[cfg(feature = "devtools")]
    ns_ui::open_dev("Hearth", |_, cx| HearthWindow::new(cx));
    #[cfg(not(feature = "devtools"))]
    ns_ui::open("Hearth", |_, cx| HearthWindow::new(cx));
}

/// Names `dev` handles. `spec()` lists these, and the match fallthrough checks them.
#[cfg(feature = "devtools")]
const DEV_COMMANDS: &[&str] = &[
    "workspace",
    "service",
    "fold",
    "mock",
    "pane",
    "hover",
    "nav",
    "state",
    "perf",
];

#[cfg(feature = "devtools")]
struct HearthSelection<'a> {
    index: usize,
    path: Option<&'a str>,
}

#[cfg(feature = "devtools")]
struct HearthDevState<'a> {
    workspace: Option<&'a str>,
    pane: &'a str,
    services: usize,
    log_rows: usize,
    log_lines: usize,
    log_cols: usize,
    screen: &'a str,
    selection: Option<HearthSelection<'a>>,
    busy: bool,
}

#[cfg(feature = "devtools")]
fn hearth_state_value(state: &HearthDevState<'_>) -> serde_json::Value {
    let selection = state.selection.as_ref().map(|item| {
        serde_json::json!({
            "index": item.index,
            "path": item.path,
        })
    });
    serde_json::json!({
        "workspace": state.workspace,
        "pane": state.pane,
        "services": state.services,
        "logRows": state.log_rows,
        "logLines": state.log_lines,
        "logCols": state.log_cols,
        "screen": state.screen,
        "selection": selection,
        "busy": state.busy,
    })
}

#[cfg(feature = "devtools")]
fn hearth_state_schema() -> serde_json::Value {
    ns_ui::state_schema(
        serde_json::json!({
            "workspace": {"type": ["string", "null"]},
            "pane": {"type": "string", "enum": ["workspaces", "shared"]},
            "services": {"type": "integer", "minimum": 0},
            "logRows": {"type": "integer", "minimum": 0},
            "logLines": {"type": "integer", "minimum": 0},
            "logCols": {"type": "integer", "minimum": 0}
        }),
        &[
            "workspace",
            "pane",
            "services",
            "logRows",
            "logLines",
            "logCols",
        ],
    )
}

#[cfg(feature = "devtools")]
fn dev_commands() -> Vec<ns_ui::DevCommand> {
    vec![
        ns_ui::DevCommand::new("workspace", "Open the workspace at an index.", true).arg(
            "index",
            "integer",
            true,
            "Index into the workspace list.",
        ),
        ns_ui::DevCommand::new("service", "Select a service and open its log.", true).arg(
            "id",
            "string",
            true,
            "Service id.",
        ),
        ns_ui::DevCommand::new("fold", "Fold or unfold a service group.", true).arg(
            "group",
            "string",
            true,
            "Group name.",
        ),
        ns_ui::DevCommand::new(
            "mock",
            "Repaint one service row in memory. Runs no CLI command.",
            true,
        )
        .arg("id", "string", true, "Service id.")
        .arg(
            "state",
            "string",
            false,
            "Row state to paint. Default \"failed\".",
        ),
        ns_ui::DevCommand::new(
            "pane",
            "Switch the sidebar between workspaces and shared.",
            true,
        )
        .arg("name", "string", true, "\"workspaces\" or \"shared\"."),
        ns_ui::DevCommand::new(
            "hover",
            "Show or clear a workspace row's hover actions.",
            true,
        )
        .arg(
            "index",
            "integer",
            false,
            "Row index. Omit it to clear the hover.",
        ),
        ns_ui::DevCommand::new("nav", "Show the navigation column.", true),
        ns_ui::DevCommand::new("state", "Read the window.", false),
        ns_ui::DevCommand::new(
            "perf",
            "Time in-memory forced draws; writes hearth-perf-<label>.json in the temp dir. Reuses the loaded board; short logs use a synthetic 50k-line buffer.",
            true,
        )
        .arg("label", "string", false, "Report name (letters, digits, - or _). Default run.")
        .arg("frames", "integer", false, "Frames per scenario, 1–1000. Default 60."),
    ]
}

/// A listed command with no match arm is a bug. Anything else is unknown.
#[cfg(feature = "devtools")]
fn reject_unknown(command: &str) -> String {
    if DEV_COMMANDS.contains(&command) {
        format!("command \"{command}\" is listed but has no handler")
    } else {
        ns_ui::unknown_command(command)
    }
}

#[cfg(feature = "devtools")]
impl ns_ui::DevControl for HearthWindow {
    fn spec() -> ns_ui::DevSpec {
        ns_ui::DevSpec {
            commands: dev_commands(),
            state_schema: hearth_state_schema(),
        }
    }

    fn dev(
        &mut self,
        command: &str,
        args: &serde_json::Value,
        cx: &mut gpui::Context<Self>,
    ) -> Result<serde_json::Value, String> {
        match command {
            // `workspace index=1`, `service id=fe`, `fold group=core`, `state`.
            "workspace" => {
                let index = args
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or("workspace needs index=")? as usize;
                if index >= self.workspaces.len() {
                    return Err(format!("no workspace {index}"));
                }
                self.select_workspace(index, cx);
                Ok(serde_json::json!({ "selected": index }))
            }
            "service" => {
                let id = args
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("service needs id=")?;
                let index = self
                    .services
                    .iter()
                    .position(|service| service.id == id)
                    .ok_or_else(|| format!("no service {id}"))?;
                self.selected_service = Some(index);
                self.open_log(cx);
                ns_ui::show_dock();
                cx.notify();
                Ok(serde_json::json!({ "selected": id }))
            }
            "fold" => {
                let group = args
                    .get("group")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("fold needs group=")?
                    .to_string();
                if !self.collapsed.remove(&group) {
                    self.collapsed.insert(group);
                }
                cx.notify();
                Ok(serde_json::json!({ "collapsed": self.collapsed.len() }))
            }
            // Dev only: repaint one row in memory as another state, to check the
            // failed styling. Runs no CLI command; the next status read replaces it.
            "mock" => {
                let id = args
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("mock needs id=")?;
                let state = args
                    .get("state")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("failed");
                let service = self
                    .services
                    .iter_mut()
                    .find(|service| service.id == id)
                    .ok_or_else(|| format!("no service {id}"))?;
                service.state = state.to_string();
                self.board_cache_dirty = true;
                cx.notify();
                Ok(serde_json::json!({ "id": id, "state": state }))
            }
            // `pane name=workspaces|shared`: the sidebar tab. Reads only.
            "pane" => {
                let pane = match args.get("name").and_then(serde_json::Value::as_str) {
                    Some("shared") => Pane::Shared,
                    Some("workspaces") => Pane::Workspaces,
                    _ => return Err("pane needs name=workspaces|shared".into()),
                };
                self.set_pane(pane, cx);
                Ok(serde_json::json!({ "pane": args.get("name") }))
            }
            // `hover index=1` shows the row's hover actions; `hover` clears them.
            "hover" => {
                self.hovered_workspace = args
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .map(|index| index as usize);
                cx.notify();
                Ok(serde_json::json!({ "hovered": self.hovered_workspace }))
            }
            "nav" => {
                ns_ui::show_nav();
                cx.notify();
                Ok(serde_json::json!({ "pane": "nav" }))
            }
            "state" => {
                let workspace = self.root();
                let screen = if self.pane == Pane::Workspaces {
                    "workspaces"
                } else {
                    "shared"
                };
                let selection = self.selected.map(|index| HearthSelection {
                    index,
                    path: self.workspaces.get(index).map(|row| row.path.as_str()),
                });
                let busy = self.busy
                    || self.board_pending
                    || !self.inflight.is_empty()
                    || self.pending.is_some()
                    || self.log_loading
                    || self.log_refreshing;
                Ok(hearth_state_value(&HearthDevState {
                    workspace: workspace.as_deref(),
                    pane: screen,
                    services: self.services.len(),
                    log_rows: self.log_rows.len(),
                    log_lines: self.log_lines.len(),
                    log_cols: self.log_cols,
                    screen,
                    selection,
                    busy,
                }))
            }
            "perf" => {
                let (label, frames) = perf::args(args)?;
                perf::start(self, &label, frames, cx)
            }
            other => Err(reject_unknown(other)),
        }
    }
}

#[cfg(all(test, feature = "devtools"))]
mod dev_contract_tests {
    use super::{
        DEV_COMMANDS, HearthDevState, HearthSelection, HearthWindow, hearth_state_value,
        reject_unknown,
    };
    use ns_ui::DevControl;

    #[test]
    fn spec_names_match_the_command_list() {
        let spec = HearthWindow::spec();
        let names: Vec<&str> = spec
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(names, DEV_COMMANDS);
    }

    #[test]
    fn an_unlisted_command_points_at_help() {
        assert_eq!(reject_unknown("nope"), ns_ui::unknown_command("nope"));
        assert!(reject_unknown("state").contains("listed"));
    }

    #[test]
    fn argument_kinds_are_protocol_kinds() {
        for command in HearthWindow::spec().commands {
            for arg in command.args {
                assert!(
                    ns_ui::ARG_KINDS.contains(&arg.kind.as_str()),
                    "{} {}",
                    command.name,
                    arg.kind
                );
            }
        }
    }

    #[test]
    fn state_schema_requires_the_contract_keys() {
        let schema = HearthWindow::spec().state_schema;
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().expect("required");
        for key in ["protocol", "app", "screen", "selection", "busy"] {
            assert!(required.iter().any(|item| item == key), "{key}");
        }
    }

    #[test]
    fn a_fixture_state_keeps_old_keys_and_the_contract() {
        let stamped = ns_ui::stamp_state(
            hearth_state_value(&HearthDevState {
                workspace: Some("/work"),
                pane: "workspaces",
                services: 3,
                log_rows: 2,
                log_lines: 4,
                log_cols: 100,
                screen: "workspaces",
                selection: Some(HearthSelection {
                    index: 1,
                    path: Some("/work"),
                }),
                busy: true,
            }),
            "hearth",
        );
        assert_eq!(stamped["workspace"], "/work");
        assert_eq!(stamped["pane"], "workspaces");
        assert_eq!(stamped["services"], 3);
        assert_eq!(stamped["logRows"], 2);
        assert_eq!(stamped["logLines"], 4);
        assert_eq!(stamped["logCols"], 100);
        assert_eq!(stamped["screen"], "workspaces");
        assert_eq!(stamped["selection"]["index"], 1);
        assert_eq!(stamped["selection"]["path"], "/work");
        assert_eq!(stamped["busy"], true);
        assert_eq!(stamped["protocol"], 1);
        assert_eq!(stamped["app"], "hearth");

        let empty = ns_ui::stamp_state(
            hearth_state_value(&HearthDevState {
                workspace: None,
                pane: "shared",
                services: 0,
                log_rows: 0,
                log_lines: 0,
                log_cols: 80,
                screen: "shared",
                selection: None,
                busy: false,
            }),
            "hearth",
        );
        assert!(empty["workspace"].is_null());
        assert!(empty["selection"].is_null());
        assert_eq!(empty["screen"], "shared");
        assert_eq!(empty["busy"], false);
    }
}

#[cfg(test)]
mod service_action_tests {
    use super::{
        Board, Catalog, Counts, DaemonDotTone, EventGate, InFlight, Report, Service, Tone,
        action_blocked, action_label, action_target, affected_ids, board_status, busy_status,
        clear_applied, count_services, daemon_dot_tone, daemon_tooltip, event_invalidates_board,
        failure_notice, inflight, instance_marked, instance_state, parse_service_command,
        remove_job, row_chrome, row_marked, row_state, scope_text, service_shows_stop, state_color,
        stopping_service_ids, workspace_root_changed, workspace_status_text,
    };

    #[test]
    fn a_palette_service_command_splits_verb_from_id() {
        assert_eq!(
            parse_service_command("svc:restart:api"),
            Some(("restart", "api"))
        );
        assert_eq!(
            parse_service_command("svc:log:web:blue"),
            Some(("log", "web:blue")),
            "only the first two separators split"
        );
        assert_eq!(parse_service_command("svc:delete:api"), None);
        assert_eq!(parse_service_command("svc:stop:"), None);
        assert_eq!(parse_service_command("act:refresh"), None);
    }

    #[test]
    fn daemon_events_only_invalidate_authoritative_board_changes() {
        assert!(event_invalidates_board(
            r#"{"type":"service.lifecycle","data":{"serviceId":"api"}}"#
        ));
        assert!(event_invalidates_board(r#"{"kind":"snapshot"}"#));
        assert!(event_invalidates_board(r#"{"kind":"unavailable"}"#));
        assert!(!event_invalidates_board(
            r#"{"type":"service.log","data":{"serviceId":"api"}}"#
        ));
        assert!(!event_invalidates_board("not json"));
    }

    fn gated(lines: &[&str]) -> usize {
        let mut gate = EventGate::default();
        lines.iter().filter(|line| gate.accept(line)).count()
    }

    #[test]
    fn repeated_unavailable_lines_invalidate_once() {
        let connection = r#"{"kind":"connection"}"#;
        let unavailable = r#"{"kind":"unavailable","message":"hearth manager is unavailable"}"#;
        assert_eq!(
            gated(&[
                connection,
                unavailable,
                connection,
                unavailable,
                connection,
                unavailable
            ]),
            1
        );
    }

    #[test]
    fn a_snapshot_resets_the_outage_so_the_next_one_notifies() {
        let unavailable = r#"{"kind":"unavailable"}"#;
        let snapshot = r#"{"kind":"snapshot"}"#;
        assert_eq!(gated(&[unavailable, unavailable, snapshot, unavailable]), 3);
        let lifecycle = r#"{"type":"service.lifecycle","data":{"serviceId":"api"}}"#;
        assert_eq!(
            gated(&[unavailable, lifecycle, unavailable, unavailable]),
            3
        );
    }

    #[test]
    fn lifecycle_events_still_invalidate_and_logs_do_not() {
        let lifecycle = r#"{"type":"service.lifecycle","data":{"serviceId":"api"}}"#;
        let log = r#"{"type":"service.log","data":{"serviceId":"api"}}"#;
        assert_eq!(gated(&[lifecycle, lifecycle, log, "not json"]), 2);
    }

    #[test]
    fn daemon_tooltip_names_the_port_the_stop_and_the_full_note() {
        assert_eq!(
            daemon_tooltip(true, None, Some(61649), ""),
            "Daemon running · port 61649"
        );
        assert_eq!(daemon_tooltip(true, None, None, ""), "Daemon running");
        // A stale port is not part of the stopped line.
        assert_eq!(
            daemon_tooltip(false, None, Some(61649), ""),
            "Daemon stopped"
        );

        let stopped = daemon_tooltip(false, None, None, "hearth manager is unavailable\nexit 3");
        assert_eq!(
            stopped,
            "Daemon stopped\nhearth manager is unavailable (exit 3)"
        );
        for word in ["hearth", "manager", "is", "unavailable", "exit", "3"] {
            assert!(stopped.contains(word), "{stopped} missing {word}");
        }

        assert_eq!(
            daemon_tooltip(true, Some("Restarting daemon…"), Some(61649), ""),
            "Restarting daemon…"
        );
        assert_eq!(
            daemon_tooltip(false, Some("  "), None, ""),
            "Daemon working…"
        );
        assert_eq!(
            daemon_tooltip(
                false,
                Some("Starting daemon…"),
                None,
                "hearth manager is unavailable\nexit 3"
            ),
            "Starting daemon…\nhearth manager is unavailable (exit 3)"
        );

        // More than a message plus `exit N` stays one line, and every line remains.
        assert_eq!(
            daemon_tooltip(false, None, None, "could not bind\naddress in use\nexit 3"),
            "Daemon stopped\ncould not bind · address in use · exit 3"
        );
        assert_eq!(
            daemon_tooltip(false, None, None, "line one\nline two"),
            "Daemon stopped\nline one · line two"
        );
        assert_eq!(
            daemon_tooltip(false, None, None, "  exit 3  \n"),
            "Daemon stopped\nexit 3"
        );
        assert_eq!(
            daemon_tooltip(false, None, None, "  \n\t"),
            "Daemon stopped"
        );
        // A trailing exit code after one message keeps both, including odd spacing.
        assert_eq!(
            daemon_tooltip(
                false,
                None,
                None,
                "  hearth manager is unavailable  \n  exit 3  \n"
            ),
            "Daemon stopped\nhearth manager is unavailable (exit 3)"
        );
    }

    #[test]
    fn daemon_dot_tone_is_working_before_up_or_down() {
        assert_eq!(daemon_dot_tone(true, false), DaemonDotTone::Running);
        assert_eq!(daemon_dot_tone(false, false), DaemonDotTone::Stopped);
        // A command in flight wins, whether the daemon is up or down.
        assert_eq!(daemon_dot_tone(true, true), DaemonDotTone::Working);
        assert_eq!(daemon_dot_tone(false, true), DaemonDotTone::Working);
    }

    #[test]
    fn a_stopped_daemon_shows_err_and_ignores_the_counts() {
        let counts = Counts {
            ready: 3,
            ..Counts::default()
        };
        assert_eq!(
            workspace_status_text(false, &counts),
            (Tone::Err, "Daemon stopped".to_string())
        );
    }

    #[test]
    fn a_running_daemon_names_its_ready_services() {
        let counts = Counts {
            ready: 10,
            stopped: 1,
            ..Counts::default()
        };
        assert_eq!(
            workspace_status_text(true, &counts),
            (Tone::Ok, "Daemon running · 10 ready".to_string())
        );
    }

    #[test]
    fn failures_turn_the_dot_warn_and_name_the_count() {
        let counts = Counts {
            ready: 2,
            failed: 1,
            ..Counts::default()
        };
        assert_eq!(
            workspace_status_text(true, &counts),
            (
                Tone::Warn,
                "Daemon running · 2 ready · 1 failed".to_string()
            )
        );
    }

    #[test]
    fn a_cached_board_sums_into_the_row_status() {
        let board = Board {
            services: vec![
                Service {
                    id: "api".into(),
                    state: "ready".into(),
                },
                Service {
                    id: "fe".into(),
                    state: "failed".into(),
                },
                Service {
                    id: "nginx-devtool".into(),
                    state: "stopped".into(),
                },
            ],
            urls: Vec::new(),
            catalog: Catalog::default(),
            daemon_up: true,
            port: None,
            daemon_note: String::new(),
            note: String::new(),
            stamp: None,
        };
        assert_eq!(
            board_status(&board, false),
            (
                Tone::Warn,
                "Daemon running · 1 ready · 1 failed".to_string()
            )
        );
        let mut down = board.clone();
        down.daemon_up = false;
        assert_eq!(
            board_status(&down, false),
            (Tone::Err, "Daemon stopped".to_string())
        );
    }

    #[test]
    fn a_different_folder_pending_clears_and_the_same_folder_does_not() {
        assert!(workspace_root_changed(Some("/viclass"), Some("/infra")));
        assert!(workspace_root_changed(Some("/viclass"), None));
        assert!(workspace_root_changed(None, Some("/viclass")));
        assert!(!workspace_root_changed(Some("/viclass"), Some("/viclass")));
        assert!(!workspace_root_changed(None, None));
    }

    #[test]
    fn a_lifecycle_action_names_its_target_and_paints_those_rows() {
        assert_eq!(
            action_label("start", "shop-backend"),
            "Starting shop-backend…"
        );
        assert_eq!(action_label("stop", "infra"), "Stopping infra…");
        assert_eq!(action_label("restart", "daemon"), "Restarting daemon…");
        assert_eq!(action_label("ensure", "daemon"), "Starting daemon…");
        assert_eq!(action_target(&["shop-backend".into()]), "shop-backend");
        assert_eq!(action_target(&["infra".into()]), "infra");
        assert_eq!(action_target(&["all".into()]), "all services");
        assert_eq!(
            action_target(&["shop".into(), "money".into()]),
            "all services"
        );

        let groups = vec![
            ("infra".into(), vec!["mongod".into(), "redis".into()]),
            ("shop".into(), vec!["shop-backend".into()]),
        ];
        let services = vec![
            Service {
                id: "mongod".into(),
                state: "ready".into(),
            },
            Service {
                id: "redis".into(),
                state: "stopped".into(),
            },
            Service {
                id: "shop-backend".into(),
                state: "ready".into(),
            },
            Service {
                id: "shorturl".into(),
                state: "running".into(),
            },
        ];
        let disabled = vec!["shorturl".into()];
        assert_eq!(
            affected_ids(&["infra".into()], &groups, &[], &services),
            vec!["mongod".to_string(), "redis".to_string()]
        );
        assert_eq!(
            affected_ids(&["shop-backend".into()], &groups, &[], &services),
            vec!["shop-backend".to_string()]
        );
        assert_eq!(
            affected_ids(&["shorturl".into()], &groups, &disabled, &services),
            Vec::<String>::new()
        );

        let flight = inflight(
            "stop",
            "infra",
            affected_ids(&["infra".into()], &groups, &[], &services),
            false,
        );
        assert_eq!(flight.label, "Stopping infra…");
        assert_eq!(
            row_state("ready", "mongod", std::slice::from_ref(&flight), None),
            "stopping"
        );
        assert_eq!(
            row_state("ready", "shop-backend", std::slice::from_ref(&flight), None),
            "ready"
        );
        assert!(row_marked("mongod", std::slice::from_ref(&flight), None));
        assert!(!row_marked(
            "shop-backend",
            std::slice::from_ref(&flight),
            None
        ));
        assert_eq!(row_state("ready", "mongod", &[], None), "ready");
        assert_eq!(
            row_state(
                "running",
                "redis",
                std::slice::from_ref(&inflight("restart", "redis", vec!["redis".into()], false)),
                None
            ),
            "restarting"
        );
        assert_eq!(state_color("restarting"), state_color("starting"));
        assert_eq!(state_color("stopping"), state_color("starting"));
        assert_eq!(
            busy_status(std::slice::from_ref(&flight), true),
            "Stopping infra…"
        );
        assert_eq!(busy_status(&[], true), "Working");
        assert_eq!(busy_status(&[], false), "Ready");
        assert_eq!(
            failure_notice(&Report {
                ok: true,
                text: "start finished.".into()
            }),
            None
        );
        assert_eq!(
            failure_notice(&Report {
                ok: true,
                text: String::new()
            }),
            None
        );
        assert_eq!(
            failure_notice(&Report {
                ok: false,
                text: "shop-backend: port in use".into()
            })
            .as_deref(),
            Some("shop-backend: port in use")
        );
        let mut installing = inflight("start", "redis", Vec::new(), false);
        installing.shared = true;
        assert_eq!(
            busy_status(std::slice::from_ref(&installing), true),
            "Starting redis…"
        );

        let live = stopping_service_ids(&services, &disabled);
        assert_eq!(live, vec!["mongod".to_string(), "shop-backend".to_string()]);

        // Shared flights carry an empty id list. The instance row matches `target`.
        assert!(!row_marked(
            "redis@7",
            std::slice::from_ref(&installing),
            None
        ));
        assert!(instance_marked("redis", std::slice::from_ref(&installing)));
        assert_eq!(
            instance_state("stopped", "redis", std::slice::from_ref(&installing)),
            "starting"
        );
        assert_eq!(
            instance_state("ready", "other", std::slice::from_ref(&installing)),
            "ready"
        );
        let mut restarting = inflight("restart", "redis@7", Vec::new(), false);
        restarting.shared = true;
        assert_eq!(
            instance_state("ready", "redis@7", std::slice::from_ref(&restarting)),
            "restarting"
        );
        let mut stopping = inflight("stop", "redis@7", Vec::new(), false);
        stopping.shared = true;
        assert_eq!(
            instance_state("ready", "redis@7", std::slice::from_ref(&stopping)),
            "stopping"
        );
        let mut daemon = inflight("stop", "daemon", vec!["redis@7".into()], true);
        daemon.shared = true;
        assert!(!instance_marked("daemon", std::slice::from_ref(&daemon)));
        assert!(!instance_marked("redis@7", std::slice::from_ref(&daemon)));
        assert_eq!(
            instance_state("", "redis@7", std::slice::from_ref(&daemon)),
            ""
        );
        // A workspace command with the same name does not mark the instance.
        let workspace = inflight("restart", "redis", vec!["redis".into()], false);
        assert!(!instance_marked("redis", std::slice::from_ref(&workspace)));
    }

    fn tagged(id: u64, verb: &'static str, target: &str, ids: &[&str], daemon: bool) -> InFlight {
        let mut job = inflight(
            verb,
            target,
            ids.iter().map(|item| (*item).to_string()).collect(),
            daemon,
        );
        job.id = id;
        job
    }

    #[test]
    fn another_group_stays_free_while_one_group_is_in_flight() {
        let infra = tagged(1, "restart", "infra", &["mongod", "redis"], false);
        let jobs = [infra];
        assert!(action_blocked(&jobs, &["infra"], false, None));
        assert!(action_blocked(&jobs, &["mongod"], false, None));
        assert!(action_blocked(&jobs, &["redis"], false, None));
        assert!(!action_blocked(&jobs, &["web"], false, None));
        assert!(!action_blocked(&jobs, &["shop"], false, None));
        assert!(row_marked("mongod", &jobs, None));
        assert!(!row_marked("web", &jobs, None));
        assert_eq!(row_state("ready", "web", &jobs, None), "ready");

        let all = tagged(2, "stop", "all services", &["mongod", "web"], false);
        assert!(action_blocked(
            std::slice::from_ref(&all),
            &["shop"],
            false,
            None
        ));
        let daemon = tagged(3, "restart", "daemon", &[], true);
        assert!(action_blocked(
            std::slice::from_ref(&daemon),
            &["web"],
            false,
            None
        ));

        let mut shared = tagged(4, "restart", "redis", &[], false);
        shared.shared = true;
        assert!(!action_blocked(
            std::slice::from_ref(&shared),
            &["redis"],
            false,
            None
        ));
        assert!(action_blocked(
            std::slice::from_ref(&shared),
            &["redis"],
            true,
            None
        ));
        assert!(!action_blocked(
            std::slice::from_ref(&shared),
            &["minio"],
            true,
            None
        ));

        // A job for another folder does not block this board.
        let mut elsewhere = tagged(5, "restart", "web", &["web"], false);
        elsewhere.root = Some("/other".into());
        assert!(!action_blocked(
            std::slice::from_ref(&elsewhere),
            &["web"],
            false,
            Some("/here")
        ));
        assert!(action_blocked(
            std::slice::from_ref(&elsewhere),
            &["web"],
            false,
            Some("/other")
        ));
    }

    #[test]
    fn finishing_one_job_keeps_the_others_marked() {
        let mut jobs = vec![
            tagged(1, "restart", "api", &["api"], false),
            tagged(2, "restart", "web", &["web"], false),
            tagged(3, "start", "redis", &[], false),
        ];
        jobs[2].shared = true;
        jobs[0].clear_after = Some(4);
        remove_job(&mut jobs, 1);
        assert_eq!(
            jobs.iter().map(|job| job.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(row_marked("web", &jobs, None));
        clear_applied(&mut jobs, 4, false);
        assert_eq!(
            jobs.iter().map(|job| job.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
        jobs[0].clear_after = Some(4);
        clear_applied(&mut jobs, 4, false);
        assert_eq!(jobs.iter().map(|job| job.id).collect::<Vec<_>>(), vec![3]);
        jobs[0].clear_after = Some(4);
        clear_applied(&mut jobs, 4, true);
        assert!(jobs.is_empty());
    }

    #[test]
    fn several_jobs_share_one_status_line() {
        let jobs = [
            tagged(1, "restart", "api", &["api"], false),
            tagged(2, "restart", "web", &["web"], false),
        ];
        assert_eq!(busy_status(&jobs, true), "Restarting api, web…");
        let mixed = [
            tagged(1, "restart", "api", &["api"], false),
            tagged(2, "stop", "web", &["web"], false),
        ];
        assert_eq!(busy_status(&mixed, true), "Stopping web…");
    }

    #[test]
    fn selection_paint_does_not_depend_on_pending() {
        for pending in [false, true] {
            for failed in [false, true] {
                let on = row_chrome(true, pending, failed);
                let off = row_chrome(false, pending, failed);
                assert!(on.selected_fill, "pending={pending} failed={failed}");
                assert!(!off.selected_fill, "pending={pending} failed={failed}");
            }
        }
        let pending_selected = row_chrome(true, true, false);
        assert!(pending_selected.selected_fill);
        assert!(pending_selected.accent_edge);
        assert!(!pending_selected.pending_fill);
        let pending_only = row_chrome(false, true, false);
        assert!(pending_only.pending_fill);
        assert!(!pending_only.selected_fill);
        assert!(!pending_only.accent_edge);
        // Disabled is not an input. The fill stays the selected bit.
        let disabled_looking = row_chrome(true, true, false);
        assert!(disabled_looking.selected_fill);
        assert!(disabled_looking.accent_edge);
        let failed_selected = row_chrome(true, false, true);
        assert!(failed_selected.selected_fill);
        assert!(failed_selected.failed_edge);
        assert!(!failed_selected.accent_edge);
    }

    #[test]
    fn counts_cover_only_the_rows_on_screen_and_name_what_is_hidden() {
        let row = |id: &str, state: &str| Service {
            id: id.into(),
            state: state.into(),
        };
        let services = vec![
            row("user", "ready"),
            row("metadata", "failed"),
            row("nats", "stopped"),
            row("fe", "succeeded"),
        ];
        let disabled = vec!["nats".to_string(), "metadata".to_string()];
        assert_eq!(
            count_services(&services, &disabled, false),
            Counts {
                ready: 1,
                stopped: 2,
                failed: 1,
                hidden: 0
            }
        );
        let hiding = count_services(&services, &disabled, true);
        assert_eq!(
            hiding,
            Counts {
                ready: 1,
                stopped: 1,
                failed: 0,
                hidden: 2
            }
        );
        assert_eq!(
            scope_text("viclass", &hiding),
            "viclass · 1 ready · 1 stopped · 2 disabled hidden"
        );
        assert_eq!(
            scope_text("viclass", &count_services(&services, &disabled, false)),
            "viclass · 1 ready · 2 stopped"
        );
    }

    #[test]
    fn running_unready_uses_the_warning_color() {
        assert_eq!(state_color("running-unready"), state_color("starting"));
        assert_ne!(state_color("running-unready"), state_color("ready"));
        assert_ne!(state_color("running-unready"), state_color("failed"));
        assert_eq!(state_color("ready"), state_color("running"));
    }

    #[test]
    fn start_and_stop_share_one_slot() {
        for state in [
            "queued-start",
            "preparing",
            "starting",
            "running",
            "running-unready",
            "ready",
            "stopping",
        ] {
            assert!(service_shows_stop(state), "{state}");
        }
        for state in [
            "stopped",
            "succeeded",
            "failed",
            "crashed",
            "orphaned",
            "externally-owned",
            "",
        ] {
            assert!(!service_shows_stop(state), "{state}");
        }
    }
}

#[cfg(test)]
mod shared_board_tests {
    use super::{
        apply_catalog_labels, attach_date, catalog_port_labels, display_instance_state,
        env_is_secret, env_shown, find_workspace, group_recipes, hearth_yaml_entry, inflight,
        install_pending, merge_live_instances, parse_instances, parse_recipes, port_line,
        split_instances, stop_confirm, stop_tip,
    };
    use crate::store::Workspace;

    const INSTALLED: &str = r#"{
      "instances": [
        {
          "name": "redis",
          "version": "8.2.10",
          "port": 43721,
          "installState": "installed",
          "recipe": {},
          "attachments": {
            "zeta": {
              "projectRoot": "/tmp/zeta",
              "provisioned": true,
              "connection": {
                "url": "redis://127.0.0.1:43721/0",
                "env": {
                  "REDIS_URL": "redis://127.0.0.1:43721/0",
                  "REDIS_TOKEN": "not-a-real-token"
                }
              },
              "attachedAt": "2026-10-02T12:00:00Z"
            },
            "alpha": {
              "projectRoot": "/tmp/alpha",
              "provisioned": true,
              "connection": {
                "url": "redis://127.0.0.1:43721/0",
                "env": { "REDIS_URL": "redis://127.0.0.1:43721/0" }
              },
              "attachedAt": "2026-10-01T08:00:00Z",
              "error": "provision failed"
            }
          }
        },
        {
          "name": "minio",
          "version": "2025-10-15",
          "port": 43730,
          "extraPorts": [43731],
          "installState": "failed",
          "installError": "extract failed",
          "recipe": { "extraPortLabels": ["console"] },
          "attachments": {}
        },
        {
          "name": "kafka",
          "version": "4.3.1",
          "port": 43110,
          "extraPorts": [43111, 43112],
          "installState": "installed",
          "recipe": {},
          "attachments": {}
        }
      ]
    }"#;

    const CATALOG: &str = r#"{
      "services": {
        "redis": { "versions": { "8.2.10": {}, "7.2.4": {} } },
        "kafka": {
          "versions": {
            "4.3.1": { "extraPortLabels": ["controller"] },
            "3.7.0": {}
          }
        },
        "minio": { "versions": { "2025-10-15": { "extraPortLabels": ["ignored"] } } }
      }
    }"#;

    const STATUS: &str = r#"{
      "instances": [
        {
          "name": "redis",
          "version": "8.2.10",
          "port": 43721,
          "extraPorts": [],
          "installState": "installed",
          "state": { "actualState": "ready" },
          "attachments": [
            {
              "projectId": "alpha",
              "projectRoot": "/tmp/alpha",
              "provisioned": true,
              "connection": { "url": "redis://127.0.0.1:43721/0" }
            }
          ]
        }
      ]
    }"#;

    #[test]
    fn installed_json_keeps_attachments_ports_and_the_install_error() {
        let instances = parse_instances(INSTALLED);
        assert_eq!(
            instances
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["kafka@4.3.1", "minio@2025-10-15", "redis@8.2.10"]
        );

        let minio = instances
            .iter()
            .find(|item| item.id == "minio@2025-10-15")
            .unwrap();
        assert_eq!(minio.extra_ports, vec![43731]);
        assert_eq!(minio.extra_labels, vec!["console".to_string()]);
        assert_eq!(minio.install, "failed");
        assert_eq!(minio.install_error.as_deref(), Some("extract failed"));
        assert_eq!(
            port_line(minio.port, &minio.extra_ports, &minio.extra_labels),
            "port 43730 · console 43731"
        );
        assert!(minio.attachments.is_empty());

        let redis = instances
            .iter()
            .find(|item| item.id == "redis@8.2.10")
            .unwrap();
        assert_eq!(redis.attachments.len(), 2);
        assert_eq!(redis.attachments[0].project_id, "alpha");
        assert_eq!(redis.attachments[0].root, "/tmp/alpha");
        assert_eq!(redis.attachments[1].root, "/tmp/zeta");
        assert_eq!(attach_date(&redis.attachments[0].attached_at), "2026-10-01");
        assert_eq!(
            redis.attachments[0].url.as_deref(),
            Some("redis://127.0.0.1:43721/0")
        );
        assert_eq!(
            redis.attachments[0].error.as_deref(),
            Some("provision failed")
        );
        assert_eq!(
            redis.attachments[1].env,
            vec![
                ("REDIS_TOKEN".into(), "not-a-real-token".into()),
                ("REDIS_URL".into(), "redis://127.0.0.1:43721/0".into()),
            ]
        );

        let (in_use, idle) = split_instances(&instances);
        assert_eq!(in_use, vec![2]);
        assert_eq!(idle, vec![0, 1]);
        let text = stop_confirm(redis);
        assert!(text.contains("2 workspaces lose it until it starts again."));
        assert!(text.contains("/tmp/alpha"));
        assert!(text.contains("/tmp/zeta"));
        assert_eq!(
            stop_tip("redis@8.2.10", 2),
            "Stop redis@8.2.10. 2 workspaces lose it until it starts again."
        );
    }

    #[test]
    fn live_status_overlays_state_and_keeps_attach_dates() {
        let mut installed = parse_instances(INSTALLED);
        let live = parse_instances(STATUS);
        assert_eq!(live[0].attachments[0].project_id, "alpha");
        assert!(live[0].attachments[0].attached_at.is_empty());
        merge_live_instances(&mut installed, live);
        let redis = installed
            .iter()
            .find(|item| item.id == "redis@8.2.10")
            .unwrap();
        assert_eq!(redis.actual, "ready");
        assert_eq!(redis.attachments.len(), 2);
        assert_eq!(redis.attachments[0].attached_at, "2026-10-01T08:00:00Z");
        assert_eq!(
            redis.attachments[0].error.as_deref(),
            Some("provision failed")
        );
    }

    #[test]
    fn catalog_labels_fill_only_when_the_snapshot_has_none() {
        let mut instances = parse_instances(INSTALLED);
        apply_catalog_labels(&mut instances, &catalog_port_labels(CATALOG));
        let kafka = instances
            .iter()
            .find(|item| item.id == "kafka@4.3.1")
            .unwrap();
        assert_eq!(kafka.extra_labels, vec!["controller".to_string()]);
        assert_eq!(
            port_line(kafka.port, &kafka.extra_ports, &kafka.extra_labels),
            "port 43110 · controller 43111 · port 3 43112"
        );
        let minio = instances
            .iter()
            .find(|item| item.id == "minio@2025-10-15")
            .unwrap();
        assert_eq!(minio.extra_labels, vec!["console".to_string()]);
        assert_eq!(port_line(Some(1), &[2], &[]), "port 1 · port 2 2");
    }

    #[test]
    fn secret_env_keys_are_masked_and_the_stored_value_stays() {
        assert!(env_is_secret("AWS_SECRET_ACCESS_KEY"));
        assert!(env_is_secret("minio_root_password"));
        assert!(env_is_secret("Session_Token"));
        assert!(!env_is_secret("REDIS_URL"));
        assert!(!env_is_secret("AWS_ACCESS_KEY_ID"));
        assert_eq!(
            env_shown("REDIS_TOKEN", "not-a-real-token"),
            "REDIS_TOKEN=••••••"
        );
        assert_eq!(
            env_shown("REDIS_URL", "redis://127.0.0.1:43721/0"),
            "REDIS_URL=redis://127.0.0.1:43721/0"
        );
        let redis = parse_instances(INSTALLED)
            .into_iter()
            .find(|item| item.id == "redis@8.2.10")
            .unwrap();
        let token = redis.attachments[1]
            .env
            .iter()
            .find(|(key, _)| key == "REDIS_TOKEN")
            .unwrap();
        assert_eq!(token.1, "not-a-real-token");
        assert_eq!(env_shown(&token.0, &token.1), "REDIS_TOKEN=••••••");
    }

    #[test]
    fn hearth_yaml_entry_is_a_shared_map() {
        assert_eq!(
            hearth_yaml_entry("redis", "8.2.10"),
            "shared:\n  redis: \"8.2.10\""
        );
    }

    #[test]
    fn recipes_group_by_service_name() {
        let groups = group_recipes(&parse_recipes(CATALOG));
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].name, "kafka");
        assert_eq!(
            groups[0].versions,
            vec!["3.7.0".to_string(), "4.3.1".to_string()]
        );
        assert_eq!(groups[1].name, "minio");
        assert_eq!(groups[2].name, "redis");
        assert_eq!(
            groups[2].versions,
            vec!["7.2.4".to_string(), "8.2.10".to_string()]
        );
    }

    #[test]
    fn a_down_shared_daemon_reads_as_unknown_until_a_flight_says_otherwise() {
        assert_eq!(
            display_instance_state("", "redis@8.2.10", &[], false),
            "Unknown"
        );
        assert_eq!(display_instance_state("", "redis@8.2.10", &[], true), "—");
        assert_eq!(
            display_instance_state("ready", "redis@8.2.10", &[], false),
            "ready"
        );
        let mut starting = inflight("start", "redis@8.2.10", Vec::new(), false);
        starting.shared = true;
        assert_eq!(
            display_instance_state("", "redis@8.2.10", std::slice::from_ref(&starting), false),
            "starting"
        );
        let mut installing = inflight("start", "redis@8.2.10", Vec::new(), false);
        installing.shared = true;
        installing.label = "Installing redis@8.2.10…".into();
        assert!(install_pending(
            "redis@8.2.10",
            std::slice::from_ref(&installing)
        ));
        assert!(!install_pending(
            "redis@8.2.10",
            std::slice::from_ref(&starting)
        ));
    }

    #[test]
    fn an_attached_root_matches_a_workspace_ignoring_a_trailing_slash() {
        let rows = vec![Workspace {
            id: "1".into(),
            path: "/tmp/alpha/".into(),
            trusted: true,
            added_at: "2026-10-01".into(),
        }];
        assert_eq!(find_workspace(&rows, "/tmp/alpha"), Some(0));
        assert_eq!(find_workspace(&rows, "/tmp/zeta"), None);
    }
}

#[cfg(test)]
mod command_arg_tests {
    use super::{
        manager_args, shared_installed_args, shared_remove_args, shared_status_args, target_args,
    };

    #[test]
    fn shared_board_reads_reveal_secrets() {
        assert_eq!(
            shared_installed_args(),
            ["shared", "installed", "--json", "--reveal"]
        );
        assert_eq!(
            shared_status_args(),
            ["shared", "status", "--json", "--reveal"]
        );
    }

    #[test]
    fn destructive_hearth_calls_pass_yes() {
        assert_eq!(
            shared_remove_args("redis@8.2.10", false),
            ["shared", "remove", "redis@8.2.10", "--json", "--yes"]
        );
        assert_eq!(
            shared_remove_args("redis@8.2.10", true),
            [
                "shared",
                "remove",
                "redis@8.2.10",
                "--json",
                "--yes",
                "--force"
            ]
        );
        assert_eq!(manager_args("stop"), ["manager", "stop", "--yes"]);
        assert_eq!(manager_args("restart"), ["manager", "restart"]);
        assert_eq!(manager_args("ensure"), ["manager", "ensure"]);
        assert_eq!(
            target_args("start", "api", true),
            ["start", "api", "--kill-unowned", "--yes"]
        );
        assert_eq!(target_args("stop", "api", false), ["stop", "api"]);
        assert_eq!(target_args("restart", "api", false), ["restart", "api"]);
    }
}
