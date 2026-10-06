//! `hearth tui` app shell. It reads the workspace file and talks to each
//! project's daemon over HTTP+SSE. Selecting a workspace only discovers a daemon that is already
//! running. Enter on a trusted workspace is what spawns one. An untrusted folder takes a second
//! enter, and that is the confirm.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    DisableMouseCapture, Event, EventStream, KeyEventKind, MouseButton, MouseEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use hearth_cli::{
    discover, ensure, request, request_with_timeout, restart_manager, stop_manager, Client,
    Discovery, LocalctlOptions,
};
use hearth_core::catalog::{ResolvedServiceUrl, ServiceCatalog};
use hearth_core::shared::{shared_root, RemoteCatalog, SharedRegistry};
use hearth_core::state::{ActualServiceState, OperationStatus, ServiceOperationKind};
use hearth_core::workspaces::{
    default_workspace_file, display_path, folder_name, normalize_path, WorkspaceStore,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::client::{fetch_log, safe_message, FetchedLog, ManagerTuiClient, WatchEvent};
use crate::desk::{
    activation, classify_attachments, group_is_up, group_sections, group_targets,
    instance_shared_notice, join_names, key_command, link_overlay, metas_from_catalog,
    project_shared_notice, should_reload_catalog, start_all_targets, stop_all_targets, summary_of,
    unchecked_shared_notice, url_visible, Act, Command, Desk, Hit, ImpactFollow, InstanceLine,
    KnownRoot, Link, LinkSelection, Pane, Pending, RecipeLine, RowId, SharedTouch, WorkspaceLine,
};
use crate::schedule::{FetchCoalescer, FrameScheduler, LogAction, LogRefresh};
use crate::state::{Service, TuiFence, TuiState};

pub type SpawnHook = Arc<dyn Fn(&Path) + Send + Sync>;

pub struct ShellOptions {
    pub initial_root: PathBuf,
    pub spawn_daemon: SpawnHook,
    pub spawn_smp: SpawnHook,
    pub refresh_interval: Duration,
}

const EMPTY_CATALOG: &str = "No services yet. Add service blocks to hearth.yaml.";
const RELEASES_URL: &str = "https://github.com/ngosangns/hearth/releases";
// Button, drag, and wheel. All-motion (`?1003`) stays off. Any reporting mode stops the terminal
// from selecting text; Ghostty will not fall back while it is on, and Shift does nothing when
// `mouse-shift-capture` is off. A drag on a painted URL is selected in the shell instead.
// `m` drops reporting. Unchanged link cells are not rewritten, or that selection is cleared.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";

enum DiscoverOutcome {
    Live {
        catalog: ServiceCatalog,
        client: Box<ManagerTuiClient>,
    },
    Offline {
        catalog: Option<ServiceCatalog>,
        message: String,
    },
}

struct SharedSnapshot {
    recipes: Vec<RecipeLine>,
    instances: Vec<InstanceLine>,
    error: Option<String>,
}

enum Msg {
    Discover {
        gen: u64,
        /// Workspace the outcome belongs to. A `gen` made stale by a workspace switch still
        /// names its workspace, so a finished daemon job can settle the view it started from.
        workspace: String,
        outcome: Box<DiscoverOutcome>,
    },
    Watch {
        gen: u64,
        event: WatchEvent,
    },
    Note {
        gen: u64,
        text: String,
    },
    Shared {
        snapshot: SharedSnapshot,
    },
    JobFinished,
    LogReady {
        gen: u64,
        key: RowId,
        body: LogBody,
    },
    SnapshotReady {
        gen: u64,
        services: Result<Vec<hearth_core::state::ServiceLifecycleState>, String>,
    },
    UrlsReady {
        gen: u64,
        urls: Result<Vec<ResolvedServiceUrl>, String>,
    },
    PidReady {
        gen: u64,
        pid: Option<i64>,
    },
    SharedLogReady {
        gen: u64,
        cursor: usize,
        id: String,
        text: Result<String, String>,
    },
}

enum LogBody {
    Daemon(Result<String, String>),
    Service {
        id: String,
        fence: TuiFence,
        fetched: Result<FetchedLog, String>,
    },
}

/// A daemon start/restart still in flight, keyed by its `gen`: the workspace it serves and
/// the notice the view shows while that workspace is selected before the job lands.
struct DaemonJob {
    workspace: String,
    label: String,
}

struct Session {
    root: PathBuf,
    catalog: ServiceCatalog,
    client: Arc<ManagerTuiClient>,
    state: TuiState,
    cancel: CancellationToken,
    fence: TuiFence,
    urls: Vec<ResolvedServiceUrl>,
}

struct Restorer;

impl Drop for Restorer {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), DisableMouseCapture, Show);
        ratatui::restore();
    }
}

pub async fn run_shell(options: ShellOptions) -> i32 {
    let mut store = match WorkspaceStore::open(default_workspace_file()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("hearth tui: {error}");
            return 1;
        }
    };
    let adopted = store.adopt_project(&options.initial_root);
    let terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            ratatui::restore();
            eprintln!("hearth tui: {error}");
            return 1;
        }
    };
    // Drop the app (and its terminal) before restoring raw mode, the alternate screen, and the mouse.
    let _restorer = Restorer;
    let mut stdout = std::io::stdout();
    if let Err(error) = write!(stdout, "{MOUSE_ON}").and_then(|()| execute!(stdout, Hide)) {
        eprintln!("hearth tui: {error}");
        return 1;
    }
    let (tx, rx) = mpsc::channel(256);
    let mut app = App {
        desk: Desk::default(),
        store,
        spawn_daemon: options.spawn_daemon,
        spawn_smp: options.spawn_smp,
        session: None,
        gen: 0,
        stopped: HashSet::new(),
        starting: HashMap::new(),
        acting: false,
        jobs: 0,
        config_revision: None,
        tx,
        terminal,
        disposed: false,
        mouse_on: true,
        link_drag: None,
        link_paint: None,
        painted_size: None,
        input_at: None,
        input_pending_present: false,
        logs: LogRefresh::default(),
        log_wait: None,
        snap_fetch: FetchCoalescer::default(),
        url_fetch: FetchCoalescer::default(),
        pid_fetch: FetchCoalescer::default(),
        shared_log_fetch: FetchCoalescer::default(),
        frames: FrameScheduler::default(),
    };
    app.sync_workspaces();
    if let Some(id) = adopted {
        if let Some(index) = app
            .desk
            .workspaces
            .iter()
            .position(|workspace| workspace.id == id)
        {
            app.desk.workspace_index = index;
        }
    }

    app.open_selected();
    app.draw();
    let code = app.event_loop(rx, options.refresh_interval).await;
    if let Some(session) = app.session.take() {
        session.cancel.cancel();
    }
    code
}

struct App {
    desk: Desk,
    store: WorkspaceStore,
    spawn_daemon: SpawnHook,
    spawn_smp: SpawnHook,
    session: Option<Session>,
    gen: u64,
    stopped: HashSet<String>,
    starting: HashMap<u64, DaemonJob>,
    acting: bool,
    jobs: u32,
    config_revision: Option<u64>,
    tx: mpsc::Sender<Msg>,
    terminal: ratatui::DefaultTerminal,
    disposed: bool,
    mouse_on: bool,
    link_drag: Option<LinkDrag>,
    /// Last OSC 8 rewrite. Equal links are left untouched so a native selection survives refresh.
    link_paint: Option<(Vec<Link>, Option<LinkSelection>)>,
    painted_size: Option<(u16, u16)>,
    /// Set when a key or wheel is dequeued. Cleared by the next `draw`, which records latency.
    input_at: Option<(Instant, &'static str)>,
    /// Scroll and wheel paint once, after pending watch messages have been applied.
    input_pending_present: bool,
    logs: LogRefresh,
    log_wait: Option<Instant>,
    snap_fetch: FetchCoalescer,
    url_fetch: FetchCoalescer,
    pid_fetch: FetchCoalescer,
    shared_log_fetch: FetchCoalescer,
    frames: FrameScheduler,
}

struct LinkDrag {
    anchor: u16,
    column: u16,
    link: Link,
}

impl App {
    async fn event_loop(&mut self, mut rx: mpsc::Receiver<Msg>, refresh: Duration) -> i32 {
        let mut events = EventStream::new();
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + refresh, refresh);
        loop {
            let queued = self.msg_queued();
            let wake = self.next_wake();
            tokio::select! {
                biased;
                maybe = events.next() => {
                    let started = Instant::now();
                    if let Some(Ok(event)) = maybe {
                        self.input_pending_present = false;
                        self.on_terminal(event).await;
                        if self.input_pending_present {
                            while let Ok(message) = rx.try_recv() {
                                self.on_msg(message).await;
                            }
                            self.input_pending_present = false;
                            self.draw();
                        } else {
                            self.drop_armed_input();
                        }
                    }
                    crate::profile::branch("terminal", started.elapsed(), self.msg_queued());
                }
                _ = tokio::time::sleep_until(wake.unwrap_or_else(tokio::time::Instant::now)), if wake.is_some() => {
                    let started = Instant::now();
                    self.on_wake();
                    crate::profile::branch("wake", started.elapsed(), self.msg_queued());
                }
                _ = interval.tick() => {
                    let started = Instant::now();
                    self.refresh().await;
                    crate::profile::branch("refresh", started.elapsed(), self.msg_queued());
                }
                message = rx.recv() => {
                    let started = Instant::now();
                    let Some(message) = message else { break };
                    self.on_msg(message).await;
                    while let Ok(more) = rx.try_recv() {
                        self.on_msg(more).await;
                    }
                    crate::profile::branch("msg", started.elapsed(), queued.max(self.msg_queued()));
                }
            }
            if self.disposed {
                break;
            }
        }
        0
    }

    async fn on_terminal(&mut self, event: Event) {
        match event {
            Event::Resize(_, _) => {
                self.link_paint = None;
                self.draw();
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.arm_input("wheel");
                    self.on_wheel(mouse.column as usize, mouse.row as usize, -1);
                }
                MouseEventKind::ScrollDown => {
                    self.arm_input("wheel");
                    self.on_wheel(mouse.column as usize, mouse.row as usize, 1);
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    self.on_mouse_down(mouse.column, mouse.row).await
                }
                MouseEventKind::Drag(MouseButton::Left) => self.on_link_drag(mouse.column),
                MouseEventKind::Up(MouseButton::Left) => self.on_link_up(),
                _ => {}
            },
            Event::Key(key)
                if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat =>
            {
                let Some(command) = key_command(&self.desk, key) else {
                    if self.desk.pending.take().is_some() {
                        self.draw();
                    }
                    return;
                };
                self.arm_input("key");
                self.on_command(command).await;
            }
            _ => {}
        }
    }

    async fn on_command(&mut self, command: Command) {
        if self.acting && !command_allowed_while_acting(&self.desk, &command) {
            return;
        }
        match command {
            Command::Quit => self.shutdown(),
            Command::ToggleHelp => {
                self.desk.help = !self.desk.help;
                self.draw();
            }
            Command::CloseHelp => {
                self.desk.help = false;
                self.draw();
            }
            Command::ClearArm => {
                self.desk.clear_arm();
                self.draw();
            }
            Command::FocusNext => {
                let before = self.desk.selected_service();
                self.desk.focus_next();
                if self.desk.selected_service() != before {
                    self.request_log(true);
                }
                self.input_pending_present = true;
            }
            Command::Move(delta) => {
                let pane = self.desk.focus;
                let before = self.desk.selected_service();
                if self.desk.nudge(delta) && pane == Pane::Workspaces {
                    self.open_selected();
                } else if self.desk.selected_service() != before {
                    self.request_log(true);
                }
                self.input_pending_present = true;
            }
            Command::ScrollLog(delta) => {
                self.desk.scroll_log(delta);
                self.input_pending_present = true;
            }
            Command::ToggleShared => {
                self.desk.toggle_shared();
                if self.desk.shared_open {
                    self.load_shared();
                }
                self.draw();
            }
            Command::ToggleMouse => self.toggle_mouse(),
            Command::Activate => self.activate().await,
            Command::Stop => self.stop_focused().await,
            Command::Restart => self.restart_focused().await,
            Command::StartAll => self.start_all().await,
            Command::StopAll => self.stop_all().await,
            Command::Reclaim => self.reclaim_key().await,
            Command::Add => {
                self.desk.clear_arm();
                self.desk.composer = Some(String::new());
                self.draw();
            }
            Command::Forget => self.arm_workspace(Pending::Forget).await,
            Command::StopDaemon => self.arm_workspace(Pending::StopDaemon).await,
            Command::RestartDaemon => self.arm_workspace(Pending::RestartDaemon).await,
            Command::ReloadCatalog => self.reload_catalog().await,
            Command::CopyUrl => self.copy_url(),
            Command::OpenFolder => self.open_folder(),
            Command::Updates => self.open_updates(),
            Command::RemoveShared => self.arm_shared_remove().await,
            Command::CycleVersion(delta) => {
                if self.desk.cycle_version(delta) {
                    self.draw();
                }
            }
            Command::Type(ch) => {
                if let Some(text) = &mut self.desk.composer {
                    if text.len() < 512 {
                        text.push(ch);
                    }
                }
                self.draw();
            }
            Command::Backspace => {
                if let Some(text) = &mut self.desk.composer {
                    text.pop();
                }
                self.draw();
            }
            Command::CancelComposer => {
                self.desk.composer = None;
                self.draw();
            }
            Command::SubmitComposer => self.submit_folder().await,
        }
    }

    async fn activate(&mut self) {
        if self.confirm_pending().await {
            return;
        }
        match activation(&self.desk) {
            Act::Trust(id) => {
                let pending = Pending::Trust(id);
                if self.desk.arm(pending) {
                    self.trust_selected().await;
                } else {
                    self.draw();
                }
            }
            Act::StartDaemon(_) => self.start_daemon().await,
            Act::FocusServices => {
                self.desk.focus = Pane::Services;
                self.request_log(true);
                self.draw();
            }
            Act::StartService(id) => {
                self.service_action(ServiceOperationKind::Start, id, false)
                    .await
            }
            Act::StopService(id) => {
                self.service_action(ServiceOperationKind::Stop, id, false)
                    .await
            }
            Act::Reclaim(id) => self.arm_reclaim(id).await,
            Act::StartGroup(name) => self.group_action(&name, ServiceOperationKind::Start).await,
            Act::RestartGroup(name) => {
                self.group_action(&name, ServiceOperationKind::Restart)
                    .await
            }
            Act::Install(id) => self.install_shared(id).await,
            Act::StartInstance(id) => self.shared_action(id, "start").await,
            Act::StopInstance(id) => self.guarded_shared(id, "stop").await,
            Act::Disabled => {
                self.desk.notice = "service is disabled".to_string();
                self.draw();
            }
            Act::Idle => {}
        }
    }

    async fn confirm_pending(&mut self) -> bool {
        let Some(pending) = self.desk.pending.clone() else {
            return false;
        };
        if !self.desk.arm(pending.clone()) {
            self.draw();
            return true;
        }
        match pending {
            Pending::Trust(_) => self.trust_selected().await,
            Pending::Forget(id) => self.forget(&id).await,
            Pending::StopDaemon(id) => self.stop_daemon(&id).await,
            Pending::RestartDaemon(id) => self.restart_daemon(&id).await,
            Pending::Reclaim(id) => {
                self.service_action(ServiceOperationKind::Start, id, true)
                    .await
            }
            Pending::RemoveShared {
                id,
                affected,
                unchecked,
            } => {
                self.remove_shared(id, unchecked || !affected.is_empty())
                    .await
            }
            Pending::SharedImpact { follow, .. } => self.run_impact(follow).await,
        }
        true
    }

    async fn arm_workspace(&mut self, kind: impl Fn(String) -> Pending) {
        let Some(workspace) = self.desk.selected_workspace() else {
            return;
        };
        let pending = kind(workspace.id.clone());
        if !self.desk.arm(pending.clone()) {
            self.draw();
            return;
        }
        match pending {
            Pending::Forget(id) => self.forget(&id).await,
            Pending::StopDaemon(id) => self.stop_daemon(&id).await,
            Pending::RestartDaemon(id) => self.restart_daemon(&id).await,
            Pending::Trust(_)
            | Pending::Reclaim(_)
            | Pending::RemoveShared { .. }
            | Pending::SharedImpact { .. } => self.draw(),
        }
    }

    async fn arm_reclaim(&mut self, id: String) {
        if self.desk.arm(Pending::Reclaim(id.clone())) {
            self.service_action(ServiceOperationKind::Start, id, true)
                .await;
        } else {
            self.draw();
        }
    }

    async fn arm_shared_remove(&mut self) {
        let Some(RowId::Instance(id)) = self.desk.selected_shared() else {
            self.desk.notice = "select an installed shared service".to_string();
            self.draw();
            return;
        };
        let pending = self.remove_pending(&id);
        let force = remove_is_forced(&pending);
        if self.desk.arm(pending) {
            self.remove_shared(id, force).await;
        } else {
            self.draw();
        }
    }

    /// Second press runs the action. First press draws the alert and returns true.
    fn hold_for_shared(
        &mut self,
        ids: &[String],
        action: ServiceOperationKind,
        label: &str,
    ) -> bool {
        let Some(pending) = self.project_pending(ids, action, label) else {
            return false;
        };
        if self.desk.arm(pending) {
            return false;
        }
        self.draw();
        true
    }

    async fn guarded_shared(&mut self, id: String, action: &str) {
        if let Some(pending) = self.instance_pending(&id, action) {
            if !self.desk.arm(pending) {
                self.draw();
                return;
            }
        }
        self.shared_action(id, action).await;
    }

    async fn run_impact(&mut self, follow: ImpactFollow) {
        match follow {
            ImpactFollow::Instance { id, action } => self.shared_action(id, &action).await,
            ImpactFollow::Remove { id, force } => self.remove_shared(id, force).await,
            ImpactFollow::Service { id, action } => {
                self.run_service_action(action, id, false).await
            }
            ImpactFollow::Many { label, ids, action } => {
                self.run_group_action(label, action, ids).await
            }
        }
    }

    fn project_pending(
        &self,
        ids: &[String],
        action: ServiceOperationKind,
        label: &str,
    ) -> Option<Pending> {
        let verb = action.as_wire_str();
        let mut touches = Vec::new();
        let mut unknown = Vec::new();
        for id in ids {
            let Some(instance) = self
                .desk
                .service_line(id)
                .and_then(|service| service.shared_instance.clone())
            else {
                continue;
            };
            match attachment_roots(&instance) {
                Ok(roots) => {
                    let report = classify_attachments(
                        &roots,
                        self.current_root().as_deref(),
                        &self.known_roots(),
                    );
                    if !report.others.is_empty() {
                        touches.push(SharedTouch {
                            instance,
                            others: report.others,
                        });
                    }
                }
                Err(()) => unknown.push(instance),
            }
        }
        if touches.is_empty() && unknown.is_empty() {
            return None;
        }
        touches.sort_by(|left, right| left.instance.cmp(&right.instance));
        unknown.sort();
        let current = self
            .desk
            .selected_workspace()
            .map(|workspace| workspace.name.clone())
            .unwrap_or_else(|| "this workspace".to_string());
        let notice = if touches.is_empty() {
            unchecked_shared_notice(verb, &join_names(&unknown))
        } else {
            project_shared_notice(verb, &current, &touches, &unknown)
        };
        let follow = if ids.len() == 1 {
            ImpactFollow::Service {
                id: ids[0].clone(),
                action,
            }
        } else {
            ImpactFollow::Many {
                label: label.to_string(),
                ids: ids.to_vec(),
                action,
            }
        };
        Some(Pending::SharedImpact { notice, follow })
    }

    fn instance_pending(&self, id: &str, action: &str) -> Option<Pending> {
        if action == "start" {
            return None;
        }
        let follow = ImpactFollow::Instance {
            id: id.to_string(),
            action: action.to_string(),
        };
        match attachment_roots(id) {
            Ok(roots) => {
                let report = classify_attachments(
                    &roots,
                    self.current_root().as_deref(),
                    &self.known_roots(),
                );
                if report.others.is_empty() {
                    return None;
                }
                Some(Pending::SharedImpact {
                    notice: instance_shared_notice(action, id, &report.all),
                    follow,
                })
            }
            Err(()) => Some(Pending::SharedImpact {
                notice: unchecked_shared_notice(action, id),
                follow,
            }),
        }
    }

    fn remove_pending(&self, id: &str) -> Pending {
        match attachment_roots(id) {
            Ok(roots) => {
                let report = classify_attachments(&roots, None, &self.known_roots());
                Pending::RemoveShared {
                    id: id.to_string(),
                    affected: report.all,
                    unchecked: false,
                }
            }
            Err(()) => Pending::RemoveShared {
                id: id.to_string(),
                affected: Vec::new(),
                unchecked: true,
            },
        }
    }

    fn current_root(&self) -> Option<String> {
        let id = self.desk.selected_workspace()?.id.clone();
        let path = self.workspace_path(&id)?;
        Some(canon_root(path.to_string_lossy().as_ref()))
    }

    fn known_roots(&self) -> Vec<KnownRoot> {
        self.store
            .list()
            .iter()
            .map(|row| KnownRoot {
                root: canon_root(&row.path),
                name: folder_name(&row.path),
                path: display_path(&row.path),
            })
            .collect()
    }

    fn toggle_mouse(&mut self) {
        self.mouse_on = !self.mouse_on;
        let mut stdout = std::io::stdout();
        if self.mouse_on {
            let _ = write!(stdout, "{MOUSE_ON}");
        } else {
            let _ = execute!(stdout, DisableMouseCapture);
        }
        let _ = stdout.flush();
        self.link_drag = None;
        if !self.mouse_on {
            // Drop the in-app highlight and force one plain rewrite, then leave those cells alone
            // so the terminal's own drag can select the URL.
            self.desk.clear_link_selection();
            self.link_paint = None;
        }
        self.desk.notice = if self.mouse_on {
            "mouse on".to_string()
        } else {
            "mouse off — drag to select, m re-enables".to_string()
        };
        self.draw();
    }

    async fn on_mouse_down(&mut self, column: u16, row: u16) {
        if let Some(link) = self.desk.link_for_press(column as usize, row as usize) {
            self.link_drag = Some(LinkDrag {
                anchor: column,
                column,
                link: link.clone(),
            });
            self.desk.select_link(&link, column, column, false);
            self.draw();
            return;
        }
        self.link_drag = None;
        let had_link = self.desk.link_selection.is_some();
        self.desk.clear_link_selection();
        let hit = self.desk.hit(column as usize, row as usize);
        self.on_click(column as usize, row as usize).await;
        if hit.is_none() && had_link {
            self.draw();
        }
    }

    fn on_link_drag(&mut self, column: u16) {
        let Some(drag) = self.link_drag.as_mut() else {
            return;
        };
        drag.column = column;
        let anchor = drag.anchor;
        let link = drag.link.clone();
        self.desk
            .select_link(&link, anchor, column, anchor.abs_diff(column) >= 2);
        self.draw();
    }

    fn on_link_up(&mut self) {
        if self.link_drag.take().is_none() {
            return;
        }
        self.copy_link_selection();
        self.draw();
    }

    fn copy_link_selection(&mut self) {
        let Some(selection) = self.desk.link_selection.clone() else {
            return;
        };
        let text = selection.copied();
        if text.is_empty() {
            return;
        }
        let notice = if text == selection.target {
            "copied url"
        } else {
            "copied selection"
        };
        self.desk.notice = match copy_to_pasteboard(&text) {
            Ok(()) => notice.to_string(),
            Err(error) => error,
        };
    }

    fn on_wheel(&mut self, column: usize, row: usize, delta: i64) {
        let before = self
            .desk
            .selected_workspace()
            .map(|workspace| workspace.id.clone());
        let service = self.desk.service_cursor;
        let moved = self.desk.wheel(column, row, delta);
        let after = self
            .desk
            .selected_workspace()
            .map(|workspace| workspace.id.clone());
        if before != after {
            self.open_selected();
        } else if moved && service != self.desk.service_cursor {
            self.request_log(true);
        }
        self.input_pending_present = true;
    }

    async fn on_click(&mut self, column: usize, row: usize) {
        let Some(hit) = self.desk.hit(column, row) else {
            return;
        };
        self.desk.clear_arm();
        match hit {
            Hit::Workspace(index) => {
                let changed = self.desk.workspace_index != index;
                self.desk.focus = Pane::Workspaces;
                self.desk.workspace_index = index;
                if changed {
                    self.open_selected();
                }
            }
            Hit::Service(cursor) => {
                let changed =
                    self.desk.service_cursor != cursor || self.desk.focus != Pane::Services;
                self.desk.focus = Pane::Services;
                self.desk.service_cursor = cursor;
                if changed {
                    self.request_log(true);
                }
            }
            Hit::Shared(cursor) => {
                let changed = self.desk.shared_cursor != cursor || self.desk.focus != Pane::Shared;
                self.desk.focus = Pane::Shared;
                self.desk.shared_cursor = cursor;
                if changed {
                    self.request_shared_log();
                }
            }
        }
        self.input_pending_present = true;
    }

    async fn refresh(&mut self) {
        if self.disposed {
            return;
        }
        let before = self
            .desk
            .selected_workspace()
            .map(|workspace| workspace.id.clone());
        if self.store.reload().is_ok() {
            let changed = self.sync_workspaces();
            let after = self
                .desk
                .selected_workspace()
                .map(|workspace| workspace.id.clone());
            if changed || before != after {
                self.open_selected();
            }
        }
        if self.session.is_some() {
            self.spawn_pid();
            if !self.acting {
                self.spawn_snapshot();
                self.spawn_urls();
            }
        }
        if let Some(root) = self.session.as_ref().map(|session| session.root.clone()) {
            self.maybe_reload_catalog(&root).await;
        }
        if self.desk.shared_open {
            self.load_shared();
        }
        self.mark_frame();
    }

    async fn on_msg(&mut self, message: Msg) {
        match message {
            Msg::Discover {
                gen,
                workspace,
                outcome,
            } => {
                let finished = self.starting.remove(&gen).is_some();
                if gen != self.gen {
                    // A daemon job that landed after the view moved still settles its own
                    // workspace: re-attach it when that workspace is the one on screen.
                    if finished
                        && self
                            .desk
                            .selected_workspace()
                            .is_some_and(|selected| selected.id == workspace)
                    {
                        self.drop_session();
                        self.gen += 1;
                        let gen = self.gen;
                        self.attach_outcome(gen, *outcome).await;
                    }
                    return;
                }
                self.attach_outcome(gen, *outcome).await;
            }
            Msg::Watch { gen, event } => {
                if gen != self.gen {
                    return;
                }
                self.on_watch(event);
            }
            Msg::Note { gen, text } => {
                if gen == self.gen || self.desk.shared_open {
                    self.desk.notice = text;
                    self.draw();
                }
            }
            Msg::Shared { snapshot } => self.apply_shared(snapshot),
            Msg::JobFinished => {
                self.finish_job();
                if self.desk.shared_open {
                    self.load_shared();
                }
                self.draw();
            }
            Msg::LogReady { gen, key, body } => self.on_log_ready(gen, key, body),
            Msg::SnapshotReady { gen, services } => self.on_snapshot_ready(gen, services),
            Msg::UrlsReady { gen, urls } => self.on_urls_ready(gen, urls),
            Msg::PidReady { gen, pid } => self.on_pid_ready(gen, pid),
            Msg::SharedLogReady {
                gen,
                cursor,
                id,
                text,
            } => self.on_shared_log_ready(gen, cursor, id, text),
        }
    }

    fn open_selected(&mut self) {
        self.drop_session();
        self.gen += 1;
        let gen = self.gen;
        self.desk.urls.clear();
        self.desk.log_scroll = 0;
        let Some(workspace) = self.desk.selected_workspace().cloned() else {
            self.desk.set_sections(Vec::new(), Vec::new());
            self.desk.summary.clear();
            self.desk.log_title = "log".to_string();
            self.desk.log = "No workspaces yet. n adds a folder by its absolute path.".to_string();
            return;
        };
        if workspace.missing {
            self.desk.set_sections(Vec::new(), Vec::new());
            self.desk.notice = if workspace.path.is_empty() {
                format!("{} is missing", workspace.name)
            } else {
                format!("{} ({}) is missing", workspace.name, workspace.path)
            };
            self.desk.log = self.desk.notice.clone();
            return;
        }
        let Some(path) = self.workspace_path(&workspace.id) else {
            return;
        };
        self.config_revision = config_revision(&path);
        self.desk.notice = self
            .starting_label(&workspace.id)
            .map(str::to_string)
            .unwrap_or_else(|| format!("opening {}…", workspace.name));
        self.desk.log_title = workspace.name;
        self.desk.log = "Loading…".to_string();
        let tx = self.tx.clone();
        let workspace_id = workspace.id.clone();
        tokio::spawn(async move {
            let outcome = discover_project(path).await;
            let _ = tx
                .send(Msg::Discover {
                    gen,
                    workspace: workspace_id,
                    outcome: Box::new(outcome),
                })
                .await;
        });
    }

    async fn start_daemon(&mut self) {
        let Some(workspace) = self.desk.selected_workspace().cloned() else {
            return;
        };
        if !workspace.trusted || workspace.missing {
            return;
        }
        let Some(path) = self.workspace_path(&workspace.id) else {
            return;
        };
        self.stopped.remove(&workspace.id);
        self.drop_session();
        self.gen += 1;
        let gen = self.gen;
        self.config_revision = config_revision(&path);
        self.begin_job();
        self.desk.notice = format!("starting daemon for {}…", workspace.name);
        self.starting.insert(
            gen,
            DaemonJob {
                workspace: workspace.id.clone(),
                label: self.desk.notice.clone(),
            },
        );
        self.draw();
        let spawn = self.spawn_daemon.clone();
        let tx = self.tx.clone();
        let workspace_id = workspace.id.clone();
        tokio::spawn(async move {
            let outcome = ensure_project(path, spawn).await;
            let _ = tx
                .send(Msg::Discover {
                    gen,
                    workspace: workspace_id,
                    outcome: Box::new(outcome),
                })
                .await;
            let _ = tx.send(Msg::JobFinished).await;
        });
    }

    async fn trust_selected(&mut self) {
        let Some(id) = self
            .desk
            .selected_workspace()
            .map(|workspace| workspace.id.clone())
        else {
            return;
        };
        let _ = self.store.reload();
        if let Err(error) = self.store.trust(&id) {
            self.desk.notice = error;
            self.draw();
            return;
        }
        self.sync_workspaces();
        self.start_daemon().await;
    }

    async fn forget(&mut self, id: &str) {
        let _ = self.store.reload();
        if let Err(error) = self.store.remove(id) {
            self.desk.notice = error;
            self.draw();
            return;
        }
        self.stopped.remove(id);
        let changed = self.sync_workspaces();
        if changed {
            self.open_selected();
        }
        self.desk.notice = "forgot the workspace — its services keep running".to_string();
        self.draw();
    }

    async fn stop_daemon(&mut self, id: &str) {
        let Some(path) = self.workspace_path(id) else {
            return;
        };
        if let Some(catalog) = self.session.as_ref().map(|session| session.catalog.clone()) {
            self.show_catalog(&catalog, &[]);
        }
        self.stopped.insert(id.to_string());
        self.drop_session();
        self.desk.attached = false;
        self.desk.notice = "stopping daemon…".to_string();
        self.draw();
        let spawn = self.spawn_daemon.clone();
        let tx = self.tx.clone();
        let gen = self.gen;
        self.begin_job();
        tokio::spawn(async move {
            let text = match load_options(&path, &spawn) {
                Ok(options) => match stop_manager(&path, &options).await {
                    Ok(_) => "daemon stopped — enter starts it again".to_string(),
                    Err(error) => format!("stop daemon failed: {}", safe_message(&error.message)),
                },
                Err(error) => error,
            };
            let _ = tx.send(Msg::Note { gen, text }).await;
            let _ = tx.send(Msg::JobFinished).await;
        });
    }

    async fn restart_daemon(&mut self, id: &str) {
        let Some(path) = self.workspace_path(id) else {
            return;
        };
        self.stopped.remove(id);
        self.drop_session();
        self.config_revision = config_revision(&path);
        self.desk.notice = "restarting daemon…".to_string();
        self.draw();
        let spawn = self.spawn_daemon.clone();
        let tx = self.tx.clone();
        self.gen += 1;
        let gen = self.gen;
        self.begin_job();
        self.starting.insert(
            gen,
            DaemonJob {
                workspace: id.to_string(),
                label: self.desk.notice.clone(),
            },
        );
        let workspace_id = id.to_string();
        tokio::spawn(async move {
            let outcome = match load_options(&path, &spawn) {
                Ok(options) => match restart_manager(&path, &options).await {
                    Ok(_) => ensure_project(path, spawn).await,
                    Err(error) => DiscoverOutcome::Offline {
                        catalog: None,
                        message: format!("restart daemon failed: {}", safe_message(&error.message)),
                    },
                },
                Err(error) => DiscoverOutcome::Offline {
                    catalog: None,
                    message: error,
                },
            };
            let _ = tx
                .send(Msg::Discover {
                    gen,
                    workspace: workspace_id,
                    outcome: Box::new(outcome),
                })
                .await;
            let _ = tx.send(Msg::JobFinished).await;
        });
    }

    async fn attach_outcome(&mut self, gen: u64, outcome: DiscoverOutcome) {
        match outcome {
            DiscoverOutcome::Live { catalog, client } => {
                if self
                    .desk
                    .selected_workspace()
                    .is_some_and(|workspace| self.stopped.contains(&workspace.id))
                {
                    self.show_catalog(&catalog, &[]);
                    self.desk.attached = false;
                    self.desk.notice = "daemon is stopping — enter starts it again".to_string();
                    self.draw();
                    return;
                }
                let client: Arc<ManagerTuiClient> = Arc::new(*client);
                let lookup_catalog = catalog.clone();
                let lookup = Arc::new(move |id: &str| {
                    lookup_catalog
                        .services
                        .iter()
                        .find(|service| service.id == id)
                        .and_then(|service| service.kind)
                });
                let mut state = TuiState::new(lookup);
                let fence = state.begin_connection();
                let cancel = CancellationToken::new();
                let watch_client = Arc::clone(&client);
                let watch_cancel = cancel.clone();
                let tx = self.tx.clone();
                let (watch_tx, mut watch_rx) = mpsc::channel(256);
                tokio::spawn(async move {
                    watch_client.watch(watch_tx, watch_cancel).await;
                });
                tokio::spawn(async move {
                    while let Some(event) = watch_rx.recv().await {
                        if tx.send(Msg::Watch { gen, event }).await.is_err() {
                            break;
                        }
                    }
                });
                self.show_catalog(&catalog, &[]);
                self.desk.attached = true;
                self.desk.notice.clear();
                if catalog.services.is_empty() {
                    self.desk.notice = EMPTY_CATALOG.to_string();
                }
                self.session = Some(Session {
                    root: client_root(&client),
                    catalog,
                    client,
                    state,
                    cancel,
                    fence,
                    urls: Vec::new(),
                });
                self.draw();
            }
            DiscoverOutcome::Offline { catalog, message } => {
                if let Some(catalog) = catalog {
                    self.show_catalog(&catalog, &[]);
                }
                let workspace = self.desk.selected_workspace().cloned();
                let name = workspace
                    .as_ref()
                    .map(|workspace| workspace.name.clone())
                    .unwrap_or_default();
                let path = workspace
                    .as_ref()
                    .map(|workspace| workspace.path.clone())
                    .unwrap_or_default();
                let trusted = workspace
                    .as_ref()
                    .is_some_and(|workspace| workspace.trusted);
                let named = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{name} ({path})")
                };
                self.desk.attached = false;
                let in_flight = workspace
                    .as_ref()
                    .and_then(|workspace| self.starting_label(&workspace.id))
                    .map(str::to_string);
                self.desk.notice = if let Some(label) = in_flight {
                    label
                } else if message.is_empty() {
                    if trusted {
                        format!("{named} — enter starts the daemon")
                    } else {
                        format!("{named} is untrusted — enter trusts it and starts the daemon")
                    }
                } else if trusted && !message.contains("enter") {
                    format!("{message} — enter retries")
                } else {
                    message
                };
                self.desk.log_title = name;
                self.desk.log = self.desk.notice.clone();
                self.draw();
            }
        }
    }

    fn on_watch(&mut self, event: WatchEvent) {
        if self.session.is_none() {
            return;
        }
        match event {
            WatchEvent::BeginConnection => {
                if let Some(session) = &mut self.session {
                    session.fence = session.state.begin_connection();
                }
            }
            WatchEvent::Snapshot(services) => {
                let gen = self.gen;
                if self.apply_services(gen, &services) {
                    self.mark_frame();
                }
            }
            // Replay only advances the event cursor. The service list comes from the snapshot.
            WatchEvent::Replay(_) => {}
            WatchEvent::ManagerEvent(event) => {
                crate::profile::sse(&event.event_type, self.msg_queued());
                let fence = self
                    .session
                    .as_ref()
                    .map(|session| session.fence)
                    .unwrap_or_default();
                let service_id = event
                    .data
                    .get("serviceId")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                if event.event_type == "service.log" {
                    let connected = self.session.as_mut().is_some_and(|session| {
                        session
                            .state
                            .apply_event(fence, &event.event_type, &event.data)
                    });
                    if connected && service_id.as_deref() == self.selected_service_id().as_deref() {
                        self.request_log(false);
                    }
                } else if TuiState::event_requires_services_snapshot(&event.event_type) {
                    let connected = self.session.as_mut().is_some_and(|session| {
                        session
                            .state
                            .apply_event(fence, &event.event_type, &event.data)
                    });
                    if connected {
                        self.spawn_snapshot();
                    }
                }
            }
            WatchEvent::Unavailable(message) => {
                self.desk.attached = false;
                self.desk.notice = format!("Manager unavailable: {message}");
                self.draw();
            }
            WatchEvent::BulkStartFinished(_) => {}
        }
    }

    fn apply_services(
        &mut self,
        gen: u64,
        services: &[hearth_core::state::ServiceLifecycleState],
    ) -> bool {
        if gen != self.gen {
            return false;
        }
        let selected = self.selected_service_id();
        {
            let Some(session) = &mut self.session else {
                return false;
            };
            let fence = session.state.begin_request();
            if let Some(id) = selected.clone() {
                session.state.selection.selected_name = id;
            }
            if !session.state.apply_snapshot(fence, services) {
                return false;
            }
            if let Some(id) = selected {
                session.state.selection.selected_name = id;
            }
            session.fence = fence;
        }
        self.rebuild_from_state();
        self.sync_urls();
        self.repaint_selected_log();
        self.request_log(false);
        true
    }

    fn rebuild_from_state(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        let live = session.state.selection.services.clone();
        let catalog = session.catalog.clone();
        self.show_catalog(&catalog, &live);
    }

    fn show_catalog(&mut self, catalog: &ServiceCatalog, live: &[Service]) {
        let sections = group_sections(&metas_from_catalog(catalog), &catalog.group_tree, live);
        let start_all = start_all_targets(catalog, &sections);
        self.desk.summary = summary_of(&sections);
        self.desk.set_sections(sections, start_all);
        if !catalog.services.is_empty() && self.desk.notice == EMPTY_CATALOG {
            self.desk.notice.clear();
        }
    }

    fn request_log(&mut self, immediate: bool) {
        self.sync_urls();
        match self.desk.selected_service() {
            Some(RowId::Daemon) => self.request_daemon_log(immediate),
            Some(RowId::Group(name)) => {
                self.stage_group_log(&name);
                self.logs.cancel();
                self.log_wait = None;
            }
            Some(RowId::Service(id)) => self.request_service_log(id, immediate),
            _ => {
                self.logs.cancel();
                self.log_wait = None;
            }
        }
    }

    fn request_daemon_log(&mut self, immediate: bool) {
        if immediate {
            self.desk.log_title = "daemon log".to_string();
            if self.session.is_none() {
                self.desk.log = "Daemon log appears after the daemon is running.".to_string();
                self.logs.cancel();
                self.log_wait = None;
                return;
            }
            self.desk.log = "Loading…".to_string();
        } else if self.session.is_none() {
            return;
        }
        let action = self.logs.request(Instant::now(), immediate);
        self.note_log_action(action);
    }

    fn request_service_log(&mut self, id: String, immediate: bool) {
        if immediate {
            self.desk.log_title = id.clone();
            if self.session.is_none() {
                self.desk.log = "Daemon is not running.".to_string();
                self.logs.cancel();
                self.log_wait = None;
                return;
            }
            if let Some(session) = &mut self.session {
                session.state.selection.selected_name = id.clone();
                let detail = session.state.detail();
                let cached = session.state.cached_log(&id);
                self.desk.log = format!("{detail}\n{cached}");
            }
        } else if self.session.is_none() {
            return;
        }
        let action = self.logs.request(Instant::now(), immediate);
        self.note_log_action(action);
    }

    fn stage_group_log(&mut self, name: &str) {
        let verb = if group_is_up(&self.desk.sections, name) {
            "restarts"
        } else {
            "starts"
        };
        self.desk.log_title = name.to_string();
        self.desk.log = format!("enter {verb} this group · x stops it · r restarts it");
    }

    /// Keeps the state line current from the snapshot without waiting on a log fetch.
    fn repaint_selected_log(&mut self) {
        let Some(RowId::Service(id)) = self.desk.selected_service() else {
            return;
        };
        let Some(session) = &self.session else {
            return;
        };
        let detail = session.state.detail();
        let cached = session.state.cached_log(&id);
        self.desk.log_title = id;
        self.desk.log = format!("{detail}\n{cached}");
    }

    fn note_log_action(&mut self, action: LogAction) {
        match action {
            LogAction::Start => {
                self.log_wait = None;
                self.spawn_log();
            }
            LogAction::Wait(when) => self.log_wait = Some(when),
            LogAction::Idle => {
                if self.log_wait.is_some_and(|when| when <= Instant::now()) {
                    self.log_wait = None;
                }
            }
        }
    }

    fn spawn_log(&mut self) {
        match self.desk.selected_service() {
            Some(RowId::Daemon) if self.session.is_some() => self.spawn_daemon_log(),
            Some(RowId::Service(id)) if self.session.is_some() => self.spawn_service_log(id),
            _ => self.release_log_fetch(),
        }
    }

    /// `Start` already took the in-flight slot. Release it when this row has nothing to fetch.
    fn release_log_fetch(&mut self) {
        self.logs.cancel();
        let action = self.logs.finish(Instant::now());
        if matches!(action, LogAction::Start) {
            self.logs.cancel();
            let action = self.logs.finish(Instant::now());
            if matches!(action, LogAction::Start) {
                self.log_wait = None;
                return;
            }
            self.note_log_action(action);
            return;
        }
        self.note_log_action(action);
    }

    fn spawn_daemon_log(&mut self) {
        let Some(client) = self
            .session
            .as_ref()
            .map(|session| Arc::clone(&session.client))
        else {
            self.release_log_fetch();
            return;
        };
        let gen = self.gen;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let body = LogBody::Daemon(client.daemon_log().await.map_err(|error| error.message));
            let _ = tx
                .send(Msg::LogReady {
                    gen,
                    key: RowId::Daemon,
                    body,
                })
                .await;
        });
    }

    fn spawn_service_log(&mut self, id: String) {
        let Some(session) = &mut self.session else {
            self.release_log_fetch();
            return;
        };
        let fence = session.state.begin_request();
        let cursor = session.state.log_cursor(&id);
        let client = Arc::clone(&session.client);
        let gen = self.gen;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let fetched = fetch_log(&client, &id, cursor).await;
            let _ = tx
                .send(Msg::LogReady {
                    gen,
                    key: RowId::Service(id.clone()),
                    body: LogBody::Service { id, fence, fetched },
                })
                .await;
        });
    }

    fn on_log_ready(&mut self, gen: u64, key: RowId, body: LogBody) {
        if gen == self.gen && self.desk.selected_service().as_ref() == Some(&key) {
            self.paint_fetched_log(body);
        }
        let action = self.logs.finish(Instant::now());
        self.note_log_action(action);
    }

    fn paint_fetched_log(&mut self, body: LogBody) {
        match body {
            LogBody::Daemon(Ok(text)) => {
                self.desk.log_title = "daemon log".to_string();
                self.desk.log = if text.is_empty() {
                    "No daemon log yet.".to_string()
                } else {
                    text
                };
                self.mark_frame();
            }
            LogBody::Daemon(Err(error)) => {
                self.desk.notice = format!("Log unavailable: {}", safe_message(&error));
                self.mark_frame();
            }
            LogBody::Service { id, fence, fetched } => {
                let outcome = {
                    let Some(session) = &mut self.session else {
                        return;
                    };
                    if session.state.selection.selected_name != id {
                        return;
                    }
                    match fetched {
                        Ok(FetchedLog::Append(slice)) => {
                            Ok(session.state.apply_log(fence, &id, &slice))
                        }
                        Ok(FetchedLog::Replace(slice)) => {
                            Ok(session.state.replace_log(fence, &id, &slice))
                        }
                        Err(error) => Err(error),
                    }
                };
                match outcome {
                    Ok(true) => self.show_service_log(&id),
                    Ok(false) => {}
                    Err(error) => {
                        self.desk.notice = format!("Log unavailable: {}", safe_message(&error));
                        self.mark_frame();
                    }
                }
            }
        }
    }

    fn show_service_log(&mut self, id: &str) {
        let Some(session) = &self.session else {
            return;
        };
        let detail = session.state.detail();
        let body = session.state.log.clone();
        self.desk.log_title = id.to_string();
        self.desk.log = format!("{detail}\n{body}");
        self.mark_frame();
    }

    fn spawn_snapshot(&mut self) {
        let Some(client) = self
            .session
            .as_ref()
            .map(|session| Arc::clone(&session.client))
        else {
            return;
        };
        if !self.snap_fetch.try_begin() {
            return;
        }
        let gen = self.gen;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let services = client.snapshot().await.map_err(|error| error.message);
            let _ = tx.send(Msg::SnapshotReady { gen, services }).await;
        });
    }

    fn on_snapshot_ready(
        &mut self,
        gen: u64,
        services: Result<Vec<hearth_core::state::ServiceLifecycleState>, String>,
    ) {
        let again = self.snap_fetch.end();
        if gen == self.gen {
            match services {
                Ok(services) => {
                    if self.apply_services(gen, &services) {
                        self.mark_frame();
                    }
                }
                Err(error) => {
                    self.desk.notice =
                        format!("Failed to refresh services: {}", safe_message(&error));
                    self.mark_frame();
                }
            }
        }
        if again {
            self.spawn_snapshot();
        }
    }

    fn spawn_urls(&mut self) {
        if self.acting {
            return;
        }
        let Some(client) = self
            .session
            .as_ref()
            .map(|session| Arc::clone(&session.client))
        else {
            return;
        };
        if !self.url_fetch.try_begin() {
            return;
        }
        let gen = self.gen;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let urls = client.urls().await.map_err(|error| error.message);
            let _ = tx.send(Msg::UrlsReady { gen, urls }).await;
        });
    }

    fn on_urls_ready(&mut self, gen: u64, urls: Result<Vec<ResolvedServiceUrl>, String>) {
        let again = self.url_fetch.end();
        if gen == self.gen {
            if let Ok(urls) = urls {
                if let Some(session) = &mut self.session {
                    session.urls = urls;
                }
                self.sync_urls();
                self.mark_frame();
            }
        }
        if again {
            self.spawn_urls();
        }
    }

    fn spawn_pid(&mut self) {
        let Some(client) = self
            .session
            .as_ref()
            .map(|session| Arc::clone(&session.client))
        else {
            return;
        };
        if !self.pid_fetch.try_begin() {
            return;
        }
        let gen = self.gen;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let pid = client.daemon_pid().await.ok();
            let _ = tx.send(Msg::PidReady { gen, pid }).await;
        });
    }

    fn on_pid_ready(&mut self, gen: u64, pid: Option<i64>) {
        let again = self.pid_fetch.end();
        if gen == self.gen && self.session.is_some() {
            self.desk.daemon_pid = pid;
            self.mark_frame();
        }
        if again {
            self.spawn_pid();
        }
    }

    fn request_shared_log(&mut self) {
        let Some(RowId::Instance(id)) = self.desk.selected_shared() else {
            self.desk.log_title = "shared".to_string();
            return;
        };
        self.desk.log_title = id.clone();
        self.desk.log = "Loading…".to_string();
        if !self.shared_log_fetch.try_begin() {
            return;
        }
        let gen = self.gen;
        let cursor = self.desk.shared_cursor;
        let spawn = self.spawn_smp.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let text = fetch_shared_log(&spawn, &id).await;
            let _ = tx
                .send(Msg::SharedLogReady {
                    gen,
                    cursor,
                    id,
                    text,
                })
                .await;
        });
    }

    fn on_shared_log_ready(
        &mut self,
        gen: u64,
        cursor: usize,
        id: String,
        text: Result<String, String>,
    ) {
        let again = self.shared_log_fetch.end();
        let selected = self.desk.selected_shared();
        let same = gen == self.gen
            && self.desk.shared_cursor == cursor
            && matches!(selected, Some(RowId::Instance(current)) if current == id);
        if same {
            self.desk.log_title = id;
            self.desk.log = match text {
                Ok(text) => text,
                Err(error) => error,
            };
            self.mark_frame();
        }
        if again {
            self.request_shared_log();
        }
    }

    fn sync_urls(&mut self) {
        let Some(RowId::Service(id)) = self.desk.selected_service() else {
            self.desk.urls.clear();
            return;
        };
        let state = self
            .desk
            .service_line(&id)
            .map(|service| service.state)
            .unwrap_or(ActualServiceState::Stopped);
        let urls = self
            .session
            .as_ref()
            .map(|session| session.urls.clone())
            .unwrap_or_default();
        self.desk.urls = urls
            .into_iter()
            .filter(|url| url.service_id == id && url_visible(url.requires_running, state))
            .map(|url| {
                format!(
                    "{}  {}",
                    url.label.unwrap_or_else(|| "-".to_string()),
                    url.url
                )
            })
            .collect();
    }

    async fn service_action(
        &mut self,
        action: ServiceOperationKind,
        service: String,
        kill_unowned: bool,
    ) {
        if self
            .desk
            .service_line(&service)
            .is_some_and(|row| row.disabled)
        {
            self.desk.notice = "service is disabled".to_string();
            self.draw();
            return;
        }
        if !kill_unowned
            && matches!(
                action,
                ServiceOperationKind::Stop | ServiceOperationKind::Restart
            )
            && self.hold_for_shared(std::slice::from_ref(&service), action, &service)
        {
            return;
        }
        self.run_service_action(action, service, kill_unowned).await;
    }

    async fn run_service_action(
        &mut self,
        action: ServiceOperationKind,
        service: String,
        kill_unowned: bool,
    ) {
        let Some(session) = &self.session else {
            self.desk.notice = "start the daemon first".to_string();
            self.draw();
            return;
        };
        if self
            .desk
            .service_line(&service)
            .is_some_and(|row| row.disabled)
        {
            self.desk.notice = "service is disabled".to_string();
            self.draw();
            return;
        }
        let client = Arc::clone(&session.client);
        self.desk.notice = format!("{} {service}…", action.as_wire_str());
        self.draw();
        self.begin_job();
        let tx = self.tx.clone();
        let gen = self.gen;
        tokio::spawn(async move {
            let text = match client.action(&service, action, kill_unowned).await {
                Ok(operation) => format!(
                    "{} {service}: {}",
                    action.as_wire_str(),
                    operation.status.as_wire_str()
                ),
                Err(error) => format!(
                    "{} {service} failed: {}",
                    action.as_wire_str(),
                    safe_message(&error.message)
                ),
            };
            let _ = tx.send(Msg::Note { gen, text }).await;
            let _ = tx.send(Msg::JobFinished).await;
        });
    }

    async fn group_action(&mut self, name: &str, action: ServiceOperationKind) {
        let targets = group_targets(&self.desk.sections, name);
        if targets.is_empty() {
            self.desk.notice = format!(
                "{name} has nothing to {action}",
                action = action.as_wire_str()
            );
            self.draw();
            return;
        }
        if action == ServiceOperationKind::Start {
            self.bulk(targets, &format!("start {name}")).await;
            return;
        }
        if self.hold_for_shared(&targets, action, name) {
            return;
        }
        self.run_group_action(name.to_string(), action, targets)
            .await;
    }

    async fn run_group_action(
        &mut self,
        label: String,
        action: ServiceOperationKind,
        targets: Vec<String>,
    ) {
        let Some(session) = &self.session else {
            self.desk.notice = "start the daemon first".to_string();
            self.draw();
            return;
        };
        let client = Arc::clone(&session.client);
        self.begin_job();
        self.desk.notice = format!("{} {label}…", action.as_wire_str());
        self.draw();
        let tx = self.tx.clone();
        let gen = self.gen;
        tokio::spawn(async move {
            let mut failed = Vec::new();
            let mut accepted = Vec::new();
            for service in &targets {
                match client.action(service, action, false).await {
                    Ok(operation) => accepted.push((service.clone(), operation.id)),
                    Err(error) => {
                        failed.push(format!("{service}: {}", safe_message(&error.message)))
                    }
                }
            }
            let waits = accepted.into_iter().map(|(service, id)| {
                let client = Arc::clone(&client);
                async move {
                    match client.wait_operation(&id).await {
                        Ok(done) if done.status == OperationStatus::Failed => {
                            let message = done
                                .error
                                .map(|error| error.message)
                                .unwrap_or_else(|| "failed".to_string());
                            Err(format!("{service}: {}", safe_message(&message)))
                        }
                        Ok(_) => Ok(()),
                        Err(error) => Err(format!("{service}: {}", safe_message(&error.message))),
                    }
                }
            });
            for result in futures_util::future::join_all(waits).await {
                if let Err(message) = result {
                    failed.push(message);
                }
            }
            let text = match failed.len() {
                0 => format!("{} {label}", action.as_wire_str()),
                1 => format!("{} {label} failed: {}", action.as_wire_str(), failed[0]),
                n => format!(
                    "{} {label}: {n} failed — {}",
                    action.as_wire_str(),
                    failed[0]
                ),
            };
            let _ = tx.send(Msg::Note { gen, text }).await;
            let _ = tx.send(Msg::JobFinished).await;
        });
    }

    async fn start_all(&mut self) {
        if self.desk.focus != Pane::Services {
            return;
        }
        let targets = self.desk.start_all.clone();
        if targets.is_empty() {
            self.desk.notice = "nothing to start".to_string();
            self.draw();
            return;
        }
        self.bulk(targets, "start all").await;
    }

    async fn stop_all(&mut self) {
        if self.desk.focus != Pane::Services {
            return;
        }
        let targets = stop_all_targets(&self.desk.sections);
        if targets.is_empty() {
            self.desk.notice = "nothing to stop".to_string();
            self.draw();
            return;
        }
        if self.hold_for_shared(&targets, ServiceOperationKind::Stop, "all") {
            return;
        }
        self.run_group_action("all".to_string(), ServiceOperationKind::Stop, targets)
            .await;
    }

    async fn bulk(&mut self, targets: Vec<String>, label: &str) {
        let Some(session) = &self.session else {
            self.desk.notice = "start the daemon first".to_string();
            self.draw();
            return;
        };
        let client = Arc::clone(&session.client);
        self.begin_job();
        self.desk.notice = format!("{label}…");
        self.draw();
        let tx = self.tx.clone();
        let gen = self.gen;
        let label = label.to_string();
        tokio::spawn(async move {
            let text = match client.bulk_start(&targets).await {
                Ok(operation) => match client.wait_operation(&operation.id).await {
                    Ok(done) if done.status == OperationStatus::Failed => format!(
                        "{label} failed: {}",
                        done.error
                            .map(|error| error.message)
                            .unwrap_or_else(|| "start failed".to_string())
                    ),
                    Ok(_) => format!("{label}: succeeded"),
                    Err(error) => format!("{label} failed: {}", safe_message(&error.message)),
                },
                Err(error) => format!("{label} failed: {}", safe_message(&error.message)),
            };
            let _ = tx.send(Msg::Note { gen, text }).await;
            let _ = tx.send(Msg::JobFinished).await;
        });
    }

    async fn stop_focused(&mut self) {
        match self.desk.focus {
            Pane::Services => match self.desk.selected_service() {
                Some(RowId::Service(id)) => {
                    if self
                        .desk
                        .service_line(&id)
                        .is_some_and(|service| service.state == ActualServiceState::Stopping)
                    {
                        return;
                    }
                    self.service_action(ServiceOperationKind::Stop, id, false)
                        .await;
                }
                Some(RowId::Group(name)) => {
                    self.group_action(&name, ServiceOperationKind::Stop).await
                }
                _ => {}
            },
            Pane::Shared => {
                if let Some(RowId::Instance(id)) = self.desk.selected_shared() {
                    self.guarded_shared(id, "stop").await;
                }
            }
            Pane::Workspaces => {}
        }
    }

    async fn restart_focused(&mut self) {
        match self.desk.focus {
            Pane::Workspaces => self.arm_workspace(Pending::RestartDaemon).await,
            Pane::Services => match self.desk.selected_service() {
                Some(RowId::Service(id)) => {
                    self.service_action(ServiceOperationKind::Restart, id, false)
                        .await
                }
                Some(RowId::Group(name)) => {
                    self.group_action(&name, ServiceOperationKind::Restart)
                        .await
                }
                _ => {}
            },
            Pane::Shared => {
                if let Some(RowId::Instance(id)) = self.desk.selected_shared() {
                    self.guarded_shared(id, "restart").await;
                }
            }
        }
    }

    async fn reclaim_key(&mut self) {
        let Some(RowId::Service(id)) = self.desk.selected_service() else {
            return;
        };
        if self
            .desk
            .service_line(&id)
            .is_some_and(|service| service.state == ActualServiceState::ExternallyOwned)
        {
            self.arm_reclaim(id).await;
        }
    }

    async fn reload_catalog(&mut self) {
        let Some(session) = &self.session else {
            self.desk.notice = "start the daemon first".to_string();
            self.draw();
            return;
        };
        let root = session.root.clone();
        let client = Arc::clone(&session.client);
        self.begin_job();
        self.desk.notice = "reloading catalog…".to_string();
        self.draw();
        let tx = self.tx.clone();
        let gen = self.gen;
        tokio::spawn(async move {
            let text = match hearth_core::config_file::load_catalog(&root) {
                Err(error) => error.errors.join("; "),
                Ok(loaded) => {
                    let body = serde_json::json!({ "requestId": uuid::Uuid::new_v4().to_string(), "catalog": loaded.catalog });
                    match client
                        .request("/v1/manager/reload", reqwest::Method::POST, Some(&body))
                        .await
                    {
                        Ok(_) => "catalog reloaded".to_string(),
                        Err(error) => format!("reload failed: {}", safe_message(&error.message)),
                    }
                }
            };
            let _ = tx.send(Msg::Note { gen, text }).await;
            let _ = tx.send(Msg::JobFinished).await;
        });
    }

    fn copy_url(&mut self) {
        if self.desk.link_selection.is_some() {
            self.copy_link_selection();
            self.draw();
            return;
        }
        let urls = match self.desk.selected_service() {
            Some(RowId::Service(id)) => self.visible_urls(&id),
            _ => Vec::new(),
        };
        let (text, notice) = if let Some(first) = urls.first() {
            let notice = if urls.len() == 1 {
                "copied url".to_string()
            } else {
                format!("copied 1 of {}", urls.len())
            };
            (first.url.clone(), notice)
        } else if !self.desk.log.is_empty() {
            (self.desk.log.clone(), "copied log".to_string())
        } else {
            self.desk.notice = "nothing to copy".to_string();
            self.draw();
            return;
        };
        self.desk.notice = match copy_to_pasteboard(&text) {
            Ok(()) => notice,
            Err(error) => error,
        };
        self.draw();
    }

    fn open_folder(&mut self) {
        let Some(id) = self
            .desk
            .selected_workspace()
            .map(|workspace| workspace.id.clone())
        else {
            return;
        };
        let Some(path) = self.workspace_path(&id) else {
            return;
        };
        self.desk.notice = match std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .status()
        {
            Ok(status) if status.success() => format!("revealed {}", path.display()),
            Ok(status) => format!("reveal failed ({status}): {}", path.display()),
            Err(error) => format!("reveal failed: {error}"),
        };
        self.draw();
    }

    fn open_updates(&mut self) {
        self.desk.notice = match std::process::Command::new("open")
            .arg(RELEASES_URL)
            .status()
        {
            Ok(status) if status.success() => {
                "opened the releases page — hearth update installs the latest binary".to_string()
            }
            _ => format!("{RELEASES_URL} — hearth update installs the latest binary"),
        };
        self.draw();
    }

    async fn submit_folder(&mut self) {
        let Some(text) = self.desk.composer.clone() else {
            return;
        };
        self.desk.composer = None;
        let path = match normalize_path(text.trim()) {
            Ok(path) => path,
            Err(error) => {
                self.desk.notice = error;
                self.draw();
                return;
            }
        };
        let _ = self.store.reload();
        match self.store.add(&path) {
            Ok(added) => {
                let id = added.record.id;
                self.sync_workspaces();
                if let Some(index) = self
                    .desk
                    .workspaces
                    .iter()
                    .position(|workspace| workspace.id == id)
                {
                    self.desk.workspace_index = index;
                }
                self.desk.focus = Pane::Workspaces;
                self.open_selected();
                self.desk.notice = if added.created {
                    "added — enter trusts it".to_string()
                } else {
                    "already in the list".to_string()
                };
            }
            Err(error) => self.desk.notice = error,
        }
        self.draw();
    }

    fn load_shared(&mut self) {
        if self.acting {
            return;
        }
        let tx = self.tx.clone();
        let spawn = self.spawn_smp.clone();
        tokio::spawn(async move {
            let snapshot = fetch_shared(&spawn).await;
            let _ = tx.send(Msg::Shared { snapshot }).await;
        });
    }

    fn apply_shared(&mut self, snapshot: SharedSnapshot) {
        let chosen: std::collections::HashMap<String, usize> = self
            .desk
            .recipes
            .iter()
            .map(|recipe| (recipe.name.clone(), recipe.version_index))
            .collect();
        self.desk.recipes = snapshot
            .recipes
            .into_iter()
            .map(|mut recipe| {
                if let Some(index) = chosen.get(&recipe.name) {
                    if *index < recipe.versions.len() {
                        recipe.version_index = *index;
                    }
                }
                recipe
            })
            .collect();
        let keep = self.desk.selected_shared();
        self.desk.instances = snapshot.instances;
        let ids = self.desk.shared_ids();
        self.desk.shared_cursor = keep
            .and_then(|id| ids.iter().position(|row| row == &id))
            .unwrap_or(0);
        if let Some(error) = snapshot.error {
            if self.desk.pending.is_none() {
                self.desk.notice = error;
            }
        }
        self.draw();
    }

    async fn install_shared(&mut self, id: String) {
        if self.desk.instances.iter().any(|instance| instance.id == id) {
            self.desk.notice = format!("{id} is already installed");
            self.draw();
            return;
        }
        self.begin_job();
        self.desk.notice = format!("installing {id}…");
        self.draw();
        let spawn = self.spawn_smp.clone();
        let tx = self.tx.clone();
        let gen = self.gen;
        tokio::spawn(async move {
            let text = match shared_client(&spawn).await {
                Ok(client) => match request_with_timeout(
                    &client,
                    "/v1/shared/install",
                    reqwest::Method::POST,
                    Some(&serde_json::json!({ "service": id })),
                    None,
                    None,
                )
                .await
                {
                    Ok(_) => format!("installed {id}"),
                    Err(error) => format!("install failed: {}", safe_message(&error)),
                },
                Err(error) => error,
            };
            let _ = tx.send(Msg::Note { gen, text }).await;
            let _ = tx.send(Msg::JobFinished).await;
        });
    }

    async fn shared_action(&mut self, id: String, action: &str) {
        self.begin_job();
        self.desk.notice = format!("{action} {id}…");
        self.draw();
        let spawn = self.spawn_smp.clone();
        let tx = self.tx.clone();
        let gen = self.gen;
        let action = action.to_string();
        tokio::spawn(async move {
            let text = match shared_client(&spawn).await {
                Ok(client) => submit_shared(&client, &id, &action).await,
                Err(error) => error,
            };
            let _ = tx.send(Msg::Note { gen, text }).await;
            let _ = tx.send(Msg::JobFinished).await;
        });
    }

    async fn remove_shared(&mut self, id: String, force: bool) {
        self.begin_job();
        self.desk.notice = format!("removing {id}…");
        self.draw();
        let spawn = self.spawn_smp.clone();
        let tx = self.tx.clone();
        let gen = self.gen;
        tokio::spawn(async move {
            let text = match shared_client(&spawn).await {
                Ok(client) => match request(
                    &client,
                    "/v1/shared/remove",
                    reqwest::Method::POST,
                    Some(&serde_json::json!({ "service": id, "force": force })),
                    None,
                )
                .await
                {
                    Ok(_) => format!("removed {id}"),
                    Err(error) => format!("remove failed: {}", safe_message(&error)),
                },
                Err(error) => error,
            };
            let _ = tx.send(Msg::Note { gen, text }).await;
            let _ = tx.send(Msg::JobFinished).await;
        });
    }

    fn sync_workspaces(&mut self) -> bool {
        let selected = self
            .desk
            .selected_workspace()
            .map(|workspace| workspace.id.clone());
        self.desk.workspaces = self
            .store
            .list()
            .iter()
            .map(|row| WorkspaceLine {
                id: row.id.clone(),
                name: folder_name(&row.path),
                path: display_path(&row.path),
                trusted: row.trusted,
                missing: !Path::new(&row.path).is_dir(),
            })
            .collect();
        if let Some(id) = &selected {
            if let Some(index) = self
                .desk
                .workspaces
                .iter()
                .position(|workspace| &workspace.id == id)
            {
                self.desk.workspace_index = index;
            }
        }
        if self.desk.workspaces.is_empty() {
            self.desk.workspace_index = 0;
        } else if self.desk.workspace_index >= self.desk.workspaces.len() {
            self.desk.workspace_index = self.desk.workspaces.len() - 1;
        }
        self.desk
            .selected_workspace()
            .map(|workspace| workspace.id.clone())
            != selected
    }

    /// The notice an in-flight daemon job shows for `id`, when one is running for it.
    fn starting_label(&self, id: &str) -> Option<&str> {
        self.starting
            .values()
            .find(|job| job.workspace == id)
            .map(|job| job.label.as_str())
    }

    fn workspace_path(&self, id: &str) -> Option<PathBuf> {
        self.store.get(id).map(|row| PathBuf::from(&row.path))
    }

    fn selected_service_id(&self) -> Option<String> {
        match self.desk.selected_service() {
            Some(RowId::Service(id)) => Some(id),
            _ => None,
        }
    }

    fn drop_session(&mut self) {
        if let Some(session) = self.session.take() {
            session.cancel.cancel();
        }
        self.desk.attached = false;
        self.desk.daemon_pid = None;
        self.config_revision = None;
        self.logs.cancel();
        self.log_wait = None;
    }

    fn begin_job(&mut self) {
        self.jobs = self.jobs.saturating_add(1);
        self.acting = true;
    }

    fn finish_job(&mut self) {
        self.jobs = self.jobs.saturating_sub(1);
        self.acting = self.jobs > 0;
    }

    async fn maybe_reload_catalog(&mut self, root: &Path) {
        let next = config_revision(root);
        if !should_reload_catalog(self.config_revision, next) {
            self.config_revision = next.or(self.config_revision);
            return;
        }
        if self.acting {
            return;
        }
        self.config_revision = next;
        self.reload_catalog().await;
    }

    fn visible_urls(&self, id: &str) -> Vec<ResolvedServiceUrl> {
        let state = self
            .desk
            .service_line(id)
            .map(|service| service.state)
            .unwrap_or(ActualServiceState::Stopped);
        self.session
            .as_ref()
            .map(|session| session.urls.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|url| url.service_id == id && url_visible(url.requires_running, state))
            .collect()
    }

    fn shutdown(&mut self) {
        self.disposed = true;
        self.drop_session();
    }

    fn msg_queued(&self) -> usize {
        self.tx.max_capacity().saturating_sub(self.tx.capacity())
    }

    fn arm_input(&mut self, kind: &'static str) {
        self.input_at = Some((Instant::now(), kind));
    }

    fn drop_armed_input(&mut self) {
        if let Some((_, kind)) = self.input_at.take() {
            crate::profile::input_dropped(kind);
        }
    }

    fn next_wake(&self) -> Option<tokio::time::Instant> {
        let when = match (self.frames.due(), self.log_wait) {
            (Some(frame), Some(log)) => Some(frame.min(log)),
            (Some(frame), None) => Some(frame),
            (None, Some(log)) => Some(log),
            (None, None) => None,
        }?;
        Some(wake_at(when))
    }

    fn on_wake(&mut self) {
        let now = Instant::now();
        let action = self.logs.poll(now);
        self.note_log_action(action);
        if self.frames.poll(now) {
            self.draw();
        }
    }

    fn mark_frame(&mut self) {
        let _ = self.frames.mark(Instant::now());
    }

    fn draw(&mut self) {
        self.frames.input(Instant::now());
        let started = Instant::now();
        if let Ok(size) = self.terminal.size() {
            let next = (size.width, size.height);
            if self.painted_size != Some(next) {
                self.link_paint = None;
                self.painted_size = Some(next);
            }
        }
        // `CompletedFrame` is the buffer that was just flushed. The current buffer was already
        // swapped and cleared, so link colours have to be read from the completed frame.
        let Ok(completed) = self.terminal.draw(|frame| self.desk.draw(frame)) else {
            self.finish_draw(started);
            return;
        };
        if self.desk.help || self.desk.composer.is_some() || self.desk.pending.is_some() {
            self.link_paint = None;
            self.finish_draw(started);
            return;
        }
        let signature = (self.desk.links.clone(), self.desk.link_selection.clone());
        if self.link_paint.as_ref() == Some(&signature) {
            self.finish_draw(started);
            return;
        }
        self.link_paint = Some(signature);
        if self.desk.links.is_empty() {
            self.finish_draw(started);
            return;
        }
        let selection = self.desk.link_selection.clone();
        let runs: Vec<(u16, u16, String)> = self
            .desk
            .links
            .iter()
            .map(|link| {
                let fg = completed
                    .buffer
                    .cell((link.x, link.y))
                    .map(|cell| cell.fg)
                    .unwrap_or(ratatui::style::Color::DarkGray);
                (link.x, link.y, link_overlay(link, selection.as_ref(), fg))
            })
            .collect();
        let mut stdout = std::io::stdout();
        for (x, y, text) in runs {
            let _ = execute!(stdout, MoveTo(x, y));
            let _ = write!(stdout, "{text}");
        }
        let _ = stdout.flush();
        self.finish_draw(started);
    }

    fn finish_draw(&mut self, started: Instant) {
        crate::profile::draw(started.elapsed());
        if let Some((at, kind)) = self.input_at.take() {
            crate::profile::input_latency(kind, at.elapsed());
        }
    }
}

fn wake_at(when: Instant) -> tokio::time::Instant {
    let now = Instant::now();
    let tokio_now = tokio::time::Instant::now();
    match when.checked_duration_since(now) {
        Some(delay) => tokio_now + delay,
        None => tokio_now,
    }
}

fn command_allowed_while_acting(desk: &Desk, command: &Command) -> bool {
    match command {
        Command::Quit
        | Command::Move(_)
        | Command::FocusNext
        | Command::ScrollLog(_)
        | Command::ToggleHelp
        | Command::CloseHelp
        | Command::ToggleShared
        | Command::ToggleMouse
        | Command::ClearArm
        | Command::Stop
        | Command::CopyUrl => true,
        Command::Activate => matches!(activation(desk), Act::StopService(_) | Act::StopInstance(_)),
        _ => false,
    }
}

fn config_revision(root: &Path) -> Option<u64> {
    let path = hearth_core::config_file::find_config_file(root)?;
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|age| age.as_secs())
}

fn client_root(client: &ManagerTuiClient) -> PathBuf {
    client.root().to_path_buf()
}

async fn discover_project(path: PathBuf) -> DiscoverOutcome {
    let loaded = match hearth_core::config_file::load_catalog(&path) {
        Ok(loaded) => loaded,
        Err(error) => {
            return DiscoverOutcome::Offline {
                catalog: None,
                message: error.errors.join("; "),
            }
        }
    };
    let catalog = loaded.catalog;
    match discover(&path, &catalog).await {
        Discovery::Live { .. } => DiscoverOutcome::Live {
            client: Box::new(ManagerTuiClient::new(path, catalog.clone())),
            catalog,
        },
        Discovery::Incompatible { .. } => DiscoverOutcome::Offline {
            catalog: Some(catalog),
            message: "hearth manager protocol is incompatible".to_string(),
        },
        Discovery::Stale { .. } => DiscoverOutcome::Offline {
            catalog: Some(catalog),
            message: "hearth manager is unavailable".to_string(),
        },
        Discovery::Malformed => DiscoverOutcome::Offline {
            catalog: Some(catalog),
            message: "hearth manager lock is unreadable — enter replaces it".to_string(),
        },
        Discovery::Absent => DiscoverOutcome::Offline {
            catalog: Some(catalog),
            message: String::new(),
        },
    }
}

async fn ensure_project(path: PathBuf, spawn: SpawnHook) -> DiscoverOutcome {
    let loaded = match hearth_core::config_file::load_catalog(&path) {
        Ok(loaded) => loaded,
        Err(error) => {
            return DiscoverOutcome::Offline {
                catalog: None,
                message: error.errors.join("; "),
            }
        }
    };
    let catalog = loaded.catalog;
    let options = hook_options(catalog.clone(), &spawn);
    match ensure(&path, &options).await {
        Ok(_) => DiscoverOutcome::Live {
            client: Box::new(ManagerTuiClient::new(path.clone(), catalog.clone())),
            catalog,
        },
        Err(error) => DiscoverOutcome::Offline {
            catalog: Some(catalog),
            message: safe_message(&error.message),
        },
    }
}

fn hook_options(catalog: ServiceCatalog, spawn: &SpawnHook) -> LocalctlOptions {
    let spawn = Arc::clone(spawn);
    LocalctlOptions {
        catalog,
        spawn_daemon: Box::new(move |path| spawn(path)),
    }
}

fn load_options(path: &Path, spawn: &SpawnHook) -> Result<LocalctlOptions, String> {
    let loaded =
        hearth_core::config_file::load_catalog(path).map_err(|error| error.errors.join("; "))?;
    Ok(hook_options(loaded.catalog, spawn))
}

fn copy_to_pasteboard(text: &str) -> Result<(), String> {
    let mut child = std::process::Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("pbcopy: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|error| error.to_string())?;
    }
    let status = child.wait().map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("pbcopy exited {status}"))
    }
}

fn remove_is_forced(pending: &Pending) -> bool {
    match pending {
        Pending::RemoveShared {
            affected,
            unchecked,
            ..
        } => *unchecked || !affected.is_empty(),
        _ => false,
    }
}

/// Project roots attached to `id`, canonicalized. `Err` means the registry could not be read,
/// which is not the same as an instance with no attachments.
fn attachment_roots(id: &str) -> Result<Vec<String>, ()> {
    let io = hearth_core::file_io::create_file_io(false);
    let registry = SharedRegistry::load(Arc::from(io), &shared_root()).map_err(|_| ())?;
    let Some(instance) = registry.get(id) else {
        return Ok(Vec::new());
    };
    Ok(instance
        .attachments
        .values()
        .map(|attachment| canon_root(&attachment.project_root))
        .collect())
}

fn canon_root(path: &str) -> String {
    let path = Path::new(path);
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

async fn fetch_shared(spawn: &SpawnHook) -> SharedSnapshot {
    let remote = RemoteCatalog::new(
        &shared_root(),
        std::env::var("HEARTH_SHARED_CATALOG_URL").ok(),
    );
    let mut error = None;
    let recipes = match remote.load(false).await {
        Ok(doc) => {
            let mut names: Vec<String> = doc.services.keys().cloned().collect();
            names.sort();
            names
                .into_iter()
                .map(|name| {
                    let mut versions: Vec<String> = doc
                        .services
                        .get(&name)
                        .map(|family| family.versions.keys().cloned().collect())
                        .unwrap_or_default();
                    versions.sort();
                    RecipeLine {
                        name,
                        versions,
                        version_index: 0,
                    }
                })
                .collect()
        }
        Err(failure) => {
            error = Some(failure.0);
            Vec::new()
        }
    };
    let instances = match hearth_cli::shared::discover_smp().await {
        Discovery::Live { client } => {
            match request(&client, "/v1/shared", reqwest::Method::GET, None, None).await {
                Ok(body) => parse_instances(&body),
                Err(failure) => {
                    error.get_or_insert(failure);
                    local_instances()
                }
            }
        }
        _ => {
            error.get_or_insert_with(|| {
                "smp is not running — enter on a recipe installs it".to_string()
            });
            local_instances()
        }
    };
    let _ = spawn;
    SharedSnapshot {
        recipes,
        instances,
        error,
    }
}

fn parse_instances(body: &serde_json::Value) -> Vec<InstanceLine> {
    body.get("instances")
        .and_then(|value| value.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let id = row.get("id")?.as_str()?.to_string();
                    Some(InstanceLine {
                        id,
                        install_state: row
                            .get("installState")
                            .and_then(|value| value.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        actual_state: row
                            .get("state")
                            .and_then(|value| value.get("actualState"))
                            .and_then(|value| value.as_str())
                            .unwrap_or("stopped")
                            .to_string(),
                        port: row
                            .get("port")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0) as u16,
                        attachments: row
                            .get("attachments")
                            .and_then(|value| value.as_array())
                            .map(|rows| rows.len())
                            .unwrap_or(0),
                        install_error: row
                            .get("installError")
                            .and_then(|value| value.as_str())
                            .filter(|text| !text.is_empty())
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn local_instances() -> Vec<InstanceLine> {
    let io = hearth_core::file_io::create_file_io(false);
    let Ok(registry) = SharedRegistry::load(Arc::from(io), &shared_root()) else {
        return Vec::new();
    };
    registry
        .list()
        .into_iter()
        .map(|instance| InstanceLine {
            id: instance.id(),
            install_state: instance.install_state.as_wire_str().to_string(),
            actual_state: "stopped".to_string(),
            port: instance.port,
            attachments: instance.attachments.len(),
            install_error: instance.install_error.clone(),
        })
        .collect()
}

async fn fetch_shared_log(spawn: &SpawnHook, id: &str) -> Result<String, String> {
    let client = shared_client(spawn).await?;
    let path = format!(
        "/v1/logs/{}?limit=16384",
        hearth_cli::encode_path_segment(id)
    );
    let body = request(&client, &path, reqwest::Method::GET, None, None)
        .await
        .map_err(|error| safe_message(&error))?;
    let data = body
        .get("data")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if data.is_empty() {
        Ok("No log yet.".to_string())
    } else {
        Ok(data.to_string())
    }
}

async fn shared_client(spawn: &SpawnHook) -> Result<Client, String> {
    hearth_cli::shared::ensure_smp(spawn)
        .await
        .map_err(|error| safe_message(&error.message))
}

async fn submit_shared(client: &Client, id: &str, action: &str) -> String {
    let body = serde_json::json!({ "requestId": uuid::Uuid::new_v4().to_string(), "serviceId": id, "action": action });
    let response = match request(
        client,
        "/v1/operations",
        reqwest::Method::POST,
        Some(&body),
        None,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => return format!("{action} {id} failed: {}", safe_message(&error)),
    };
    let Some(operation_id) = response
        .get("operation")
        .and_then(|value| value.get("id"))
        .and_then(|value| value.as_str())
    else {
        return format!("{action} {id}");
    };
    match client
        .wait(
            operation_id,
            Some(tokio::time::Instant::now() + Duration::from_secs(180)),
        )
        .await
    {
        Ok(operation) if operation.status == OperationStatus::Failed => format!(
            "{action} {id} failed: {}",
            operation
                .error
                .map(|error| error.message)
                .unwrap_or_else(|| "failed".to_string())
        ),
        Ok(operation) => format!("{action} {id}: {}", operation.status.as_wire_str()),
        Err(error) => format!("{action} {id} failed: {}", safe_message(&error.message)),
    }
}
