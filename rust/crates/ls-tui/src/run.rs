//! Port of `src/tui/index.ts` — boots a terminal app that is just another HTTP+SSE client of the
//! daemon; it has no direct access to processes.
//!
//! **Deliberate deviations from the TS source** (this module cannot be meaningfully unit-tested —
//! it owns the real terminal and a real reconnect loop — so these are judgment calls made while
//! porting, not something a test caught):
//! - No diffing renderer. `pi-tui`'s `TUI`/`ProcessTerminal` do incremental terminal diffing; this
//!   port does a full clear + redraw every frame via plain ANSI. More flicker-prone under a very
//!   fast event stream, functionally equivalent otherwise.
//! - Action dispatch (`start`/`stop`/`restart`/`start all`) runs to completion inline in the same
//!   event loop that also reads input, instead of the TS source's fire-and-forget
//!   (`void runAction(...)`) that lets input keep being processed while the request is in flight.
//!   Rust's single-owner `TuiApp` makes true concurrent mutation awkward without message-passing
//!   the entire action back in, which isn't worth the complexity for what is already a `busy`-gated,
//!   typically sub-second HTTP round trip.
//! - Input comes from `crossterm`'s structured `KeyEvent`/`MouseEvent`, not raw terminal bytes (see
//!   `actions.rs`'s module doc for why).
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind, MouseButton, MouseEventKind};
use futures_util::StreamExt;
use ls_cli::{ensure, LocalctlOptions, SpawnDaemon};
use ls_core::catalog::ServiceCatalog;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::actions::{keyboard_action, TuiAction};
use crate::client::{refresh_selected_log, ManagerTuiClient, WatchEvent};
use crate::screen::{ServiceScreen, ScreenUpdate, Viewport};
use crate::state::{ActionKind, ServiceKindLookup, TuiFence, TuiState};

const MOUSE_TRACKING_ON: &str = "\x1b[?1000h\x1b[?1006h";
const MOUSE_TRACKING_OFF: &str = "\x1b[?1006l\x1b[?1000l";

pub struct RunTuiOptions {
    pub root: PathBuf,
    pub catalog: ServiceCatalog,
    /// Same shape as `LocalctlOptions.spawn_daemon` — spawns the project's own daemon entry,
    /// detached.
    pub spawn_daemon: SpawnDaemon,
    /// Background reconciliation poll interval; the SSE event stream is the primary update path.
    pub refresh_interval: Duration,
    /// Looks up a service's `kind` for display. Defaults to a catalog lookup.
    pub service_kind: Option<ServiceKindLookup>,
}

fn message(error: &dyn std::error::Error) -> String {
    error.to_string().chars().map(|c| if c == '\r' || c == '\n' || (c as u32) < 0x20 || c as u32 == 0x7f { ' ' } else { c }).collect()
}

/// Boots a terminal app that is just another HTTP+SSE client of the daemon. Resolves with an exit
/// code once the user quits (`q`/Ctrl-C).
pub async fn run_tui(options: RunTuiOptions) -> i32 {
    let RunTuiOptions { root, catalog, spawn_daemon, refresh_interval, service_kind } = options;
    let localctl_options = LocalctlOptions { catalog: catalog.clone(), spawn_daemon, doctor_checks: None };
    if let Err(error) = ensure(&root, &localctl_options).await {
        eprintln!("{}", error.message);
        return error.exit_code;
    }

    let service_kind_lookup: ServiceKindLookup = service_kind.unwrap_or_else(|| {
        let lookup_catalog = catalog.clone();
        Arc::new(move |id: &str| lookup_catalog.services.iter().find(|s| s.id == id).and_then(|s| s.kind))
    });
    let all_targets: Vec<String> = catalog.groups.get("all").cloned().unwrap_or_else(|| catalog.services.iter().map(|s| s.id.clone()).collect());

    let client = ManagerTuiClient::new(root, &localctl_options);
    let (columns, rows) = crossterm::terminal::size().map(|(c, r)| (c as usize, r as usize)).unwrap_or((80, 24));

    let mut app = TuiApp { state: TuiState::new(service_kind_lookup), screen: ServiceScreen::new(Viewport { columns, rows }), all_targets, busy: false, disposed: false, columns, rows, urls: Vec::new() };

    let _ = crossterm::terminal::enable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "{MOUSE_TRACKING_ON}");
    let _ = stdout.flush();

    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel::<WatchEvent>(256);

    let watch_future = client.watch(tx, cancel.clone());
    let loop_future = app.event_loop(&client, rx, refresh_interval, cancel.clone());
    let (_, exit_code) = tokio::join!(watch_future, loop_future);

    let _ = write!(stdout, "{MOUSE_TRACKING_OFF}");
    let _ = stdout.flush();
    let _ = crossterm::terminal::disable_raw_mode();
    exit_code
}

struct TuiApp {
    state: TuiState,
    screen: ServiceScreen,
    all_targets: Vec<String>,
    busy: bool,
    disposed: bool,
    columns: usize,
    rows: usize,
    /// Every service's resolved URLs, refreshed alongside each snapshot.
    urls: Vec<ls_core::catalog::ResolvedServiceUrl>,
}

impl TuiApp {
    async fn event_loop(&mut self, client: &ManagerTuiClient<'_>, mut rx: mpsc::Receiver<WatchEvent>, refresh_interval: Duration, cancel: CancellationToken) -> i32 {
        let mut current_fence = TuiFence::default();
        let mut events = EventStream::new();
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + refresh_interval, refresh_interval);
        self.draw();

        loop {
            tokio::select! {
                maybe_event = events.next() => {
                    if let Some(Ok(event)) = maybe_event {
                        self.handle_terminal_event(event, client).await;
                    }
                }
                _ = interval.tick() => {
                    self.reconcile(client).await;
                }
                message = rx.recv() => {
                    let Some(message) = message else { break };
                    self.handle_watch_event(message, client, &mut current_fence).await;
                }
            }
            if self.disposed {
                break;
            }
        }
        cancel.cancel();
        0
    }

    async fn handle_watch_event(&mut self, message: WatchEvent, client: &ManagerTuiClient<'_>, current_fence: &mut TuiFence) {
        match message {
            WatchEvent::BeginConnection => {
                *current_fence = self.state.begin_connection();
            }
            WatchEvent::Snapshot(services) => {
                if !self.state.apply_snapshot(*current_fence, &services) {
                    return;
                }
                self.state.notice.clear();
                self.draw();
                let fence = self.state.begin_request();
                self.refresh_selected(client, fence).await;
            }
            WatchEvent::Replay(replay) => {
                if !self.state.connected(*current_fence) {
                    return;
                }
                if replay.reset {
                    self.state.notice = "Manager restarted; state resynchronized.".to_string();
                }
                self.draw();
            }
            WatchEvent::ManagerEvent(event) => {
                let service_id = event.data.get("serviceId").and_then(|v| v.as_str()).map(str::to_string);
                if !self.state.apply_event(*current_fence, &event.event_type, &event.data) {
                    return;
                }
                self.draw();
                if event.event_type == "service.log" && service_id.as_deref() == Some(self.state.selection.selected_name.as_str()) {
                    let fence = self.state.begin_request();
                    self.refresh_selected(client, fence).await;
                }
                if event.event_type == "operation.updated" && service_id.as_deref() == Some(self.state.selection.selected_name.as_str()) {
                    let fence = self.state.begin_request();
                    self.refresh_operation(client, fence).await;
                }
            }
            WatchEvent::Unavailable(msg) => {
                if !self.state.connected(*current_fence) {
                    return;
                }
                self.state.notice = format!("Manager unavailable: {msg}");
                self.draw();
            }
        }
    }

    async fn handle_terminal_event(&mut self, event: Event, client: &ManagerTuiClient<'_>) {
        match event {
            Event::Resize(columns, rows) => {
                self.columns = columns as usize;
                self.rows = rows as usize;
                self.screen.invalidate();
                self.draw();
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.handle_wheel(mouse.row as usize, -1),
                MouseEventKind::ScrollDown => self.handle_wheel(mouse.row as usize, 1),
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(service) = self.screen.service_at(mouse.row as usize, Some(self.rows)) {
                        self.select_service(&service, client).await;
                    }
                }
                _ => {}
            },
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let selected = self.state.selection.selected().cloned();
                let Some(action) = keyboard_action(key, selected.as_ref()) else { return };
                match action {
                    TuiAction::Quit => self.shutdown(),
                    TuiAction::Up => self.move_selection(-1, client).await,
                    TuiAction::Down => self.move_selection(1, client).await,
                    TuiAction::StartAll => self.run_all(ActionKind::Start, client).await,
                    TuiAction::StopAll => self.run_all(ActionKind::Stop, client).await,
                    TuiAction::Start => self.run_action(ActionKind::Start, self.state.selection.selected_name.clone(), client).await,
                    TuiAction::Stop => self.run_action(ActionKind::Stop, self.state.selection.selected_name.clone(), client).await,
                    TuiAction::Restart => self.run_action(ActionKind::Restart, self.state.selection.selected_name.clone(), client).await,
                }
            }
            _ => {}
        }
    }

    fn handle_wheel(&mut self, row: usize, delta: i64) {
        let rows = self.rows;
        if let Some(service) = self.screen.handle_wheel(row, delta, Some(rows)) {
            self.state.selection.select(&service);
            self.draw();
        }
    }

    async fn select_service(&mut self, name: &str, client: &ManagerTuiClient<'_>) {
        if !self.state.selection.select(name) {
            return;
        }
        let fence = self.state.begin_selection();
        self.draw();
        self.refresh_selected(client, fence).await;
    }

    async fn move_selection(&mut self, delta: i64, client: &ManagerTuiClient<'_>) {
        if !self.state.selection.move_by(delta) {
            return;
        }
        let fence = self.state.begin_selection();
        self.draw();
        self.refresh_selected(client, fence).await;
    }

    async fn reconcile(&mut self, client: &ManagerTuiClient<'_>) {
        if self.disposed || self.busy {
            return;
        }
        let fence = self.state.begin_request();
        match client.snapshot().await {
            Ok(services) => {
                if !self.state.apply_snapshot(fence, &services) {
                    return;
                }
                // Best-effort: a daemon from before `/v1/urls` existed simply has no URL rows.
                if let Ok(urls) = client.urls().await {
                    self.urls = urls;
                }
                self.state.notice.clear();
                self.draw();
                let fence = self.state.begin_request();
                self.refresh_selected(client, fence).await;
            }
            Err(error) => {
                if !self.state.current(fence) {
                    return;
                }
                self.state.notice = format!("Manager unavailable: {}", message(&error));
                self.draw();
            }
        }
    }

    async fn refresh_selected(&mut self, client: &ManagerTuiClient<'_>, fence: TuiFence) {
        if self.state.selection.selected_name.is_empty() {
            if self.state.current(fence) {
                self.state.log = "No services.".to_string();
                self.draw();
            }
            return;
        }
        match refresh_selected_log(client, &mut self.state, fence).await {
            Ok(changed) => {
                if changed {
                    self.draw();
                }
                self.refresh_operation(client, fence).await;
            }
            Err(error) => {
                if self.state.current(fence) {
                    self.state.notice = format!("Log unavailable: {error}");
                    self.draw();
                }
            }
        }
    }

    async fn refresh_operation(&mut self, client: &ManagerTuiClient<'_>, fence: TuiFence) {
        let Some(operation_id) = self.state.selection.selected().and_then(|s| s.current_operation_id.clone()) else { return };
        match client.operation(&operation_id).await {
            Ok(operation) => {
                if self.state.apply_operation(fence, operation) {
                    self.draw();
                }
            }
            Err(error) => {
                if self.state.current(fence) {
                    self.state.notice = format!("Operation unavailable: {}", error.message);
                    self.draw();
                }
            }
        }
    }

    async fn run_all(&mut self, action: ActionKind, client: &ManagerTuiClient<'_>) {
        if action == ActionKind::Start {
            if self.busy {
                return;
            }
            self.busy = true;
            self.state.notice = "start all…".to_string();
            self.draw();
            let outcome: Result<(), String> = async {
                let operation = client.bulk_start(&self.all_targets).await.map_err(|e| e.message)?;
                let completed = client.wait_operation(&operation.id).await.map_err(|e| e.message)?;
                if completed.status == ls_core::state::OperationStatus::Failed {
                    return Err(completed.error.map(|e| e.message).unwrap_or_else(|| "start all failed".to_string()));
                }
                Ok(())
            }
            .await;
            match outcome {
                Ok(()) => {
                    self.state.notice = "start all: succeeded".to_string();
                    self.draw();
                    self.reconcile(client).await;
                }
                Err(error) => {
                    self.state.notice = format!("start all failed: {error}");
                    self.draw();
                }
            }
            self.busy = false;
            return;
        }
        let services: Vec<String> = self.state.selection.services.iter().map(|s| s.name.clone()).collect();
        for service in services {
            if self.disposed {
                return;
            }
            self.run_action(action, service, client).await;
        }
    }

    async fn run_action(&mut self, action: ActionKind, service: String, client: &ManagerTuiClient<'_>) {
        if self.busy || service.is_empty() {
            return;
        }
        let fence = self.state.begin_action(&service, action);
        self.busy = true;
        self.state.notice = format!("{} {service}…", action_verb(action));
        self.draw();
        match client.action(&service, action).await {
            Ok(operation) => {
                if self.state.apply_action(&fence, operation) {
                    self.draw();
                    self.reconcile(client).await;
                }
            }
            Err(error) => {
                if self.state.apply_action_failure(&fence, &error.message) {
                    self.draw();
                }
            }
        }
        self.busy = false;
    }

    fn shutdown(&mut self) {
        if self.disposed {
            return;
        }
        self.disposed = true;
        self.state.begin_connection();
    }

    /// The focused service's URLs as screen rows, flagging the ones that need the service running
    /// while it is not — the same rule as `lsd urls`.
    fn focused_urls(&self) -> Vec<String> {
        let selected = &self.state.selection.selected_name;
        let running = self.state.selection.services.iter().find(|s| &s.name == selected).is_some_and(|s| matches!(s.state.as_str(), "ready" | "running" | "degraded" | "starting" | "preparing"));
        self.urls
            .iter()
            .filter(|u| &u.service_id == selected)
            .map(|u| format!("{}  {}{}", u.label.as_deref().unwrap_or("-"), u.url, if u.requires_running && !running { "  (not running)" } else { "" }))
            .collect()
    }

    fn draw(&mut self) {
        self.screen.update(ScreenUpdate {
            services: Some(self.state.selection.services.clone()),
            selected_name: Some(self.state.selection.selected_name.clone()),
            log_service: Some(self.state.selection.selected_name.clone()),
            log: Some(format!("{}\n{}", self.state.detail(), self.state.log)),
            notice: Some(self.state.notice.clone()),
            urls: Some(self.focused_urls()),
        });
        let lines = self.screen.render(self.columns, Some(self.rows)).to_vec();
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "\x1b[H");
        for line in &lines {
            let _ = write!(stdout, "{line}\x1b[K\r\n");
        }
        let _ = stdout.flush();
    }
}

fn action_verb(action: ActionKind) -> &'static str {
    match action {
        ActionKind::Start => "start",
        ActionKind::Stop => "stop",
        ActionKind::Restart => "restart",
    }
}
