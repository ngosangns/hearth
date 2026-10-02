//! Boots a terminal app that is just another HTTP+SSE client of the daemon; it has no direct access
//! to processes. This module owns the real terminal and has no automated coverage.
//!
//! - Every frame is a full redraw via plain ANSI — no diffing renderer.
//! - Action requests (a single POST each) run inline in the event loop, `busy`-gated. Anything
//!   that can take minutes must not: "start all" only POSTs inline and waits for the bulk
//!   operation in a spawned task that reports back over the watch channel, so input (`q`,
//!   Ctrl-C — raw mode disables ISIG) is never blocked behind a build or a readiness timeout.
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind, MouseButton, MouseEventKind};
use futures_util::StreamExt;
use hearth_cli::{ensure, LocalctlOptions, SpawnDaemon};
use hearth_core::catalog::ServiceCatalog;
use hearth_core::state::{ActualServiceState, OperationStatus, ServiceOperationKind};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::actions::{keyboard_action, TuiAction};
use crate::client::{refresh_selected_log, safe_message, ManagerTuiClient, WatchEvent};
use crate::screen::{ServiceScreen, ScreenUpdate, Viewport};
use crate::state::{ServiceKindLookup, TuiFence, TuiState};

const MOUSE_TRACKING_ON: &str = "\x1b[?1000h\x1b[?1006h";
const MOUSE_TRACKING_OFF: &str = "\x1b[?1006l\x1b[?1000l";

#[deprecated(
    since = "0.18.0",
    note = "use hearth_tui::run_shell / `hearth tui` (Ratatui workspace shell); this ANSI single-project path is legacy"
)]
pub struct RunTuiOptions {
    pub root: PathBuf,
    pub catalog: ServiceCatalog,
    /// Same shape as `LocalctlOptions.spawn_daemon` — spawns the project's own daemon entry,
    /// detached.
    pub spawn_daemon: SpawnDaemon,
    /// Background reconciliation poll interval; the SSE event stream is the primary update path.
    pub refresh_interval: Duration,
}

/// Boots a terminal app that is just another HTTP+SSE client of the daemon. Resolves with an exit
/// code once the user quits (`q`/Ctrl-C).
#[deprecated(
    since = "0.18.0",
    note = "use hearth_tui::run_shell / `hearth tui` (Ratatui workspace shell); this ANSI single-project path is legacy"
)]
#[allow(deprecated)]
pub async fn run_tui(options: RunTuiOptions) -> i32 {
    let RunTuiOptions { root, catalog, spawn_daemon, refresh_interval } = options;
    let localctl_options = LocalctlOptions { catalog: catalog.clone(), spawn_daemon };
    if let Err(error) = ensure(&root, &localctl_options).await {
        eprintln!("{}", error.message);
        return error.exit_code;
    }

    let lookup_catalog = catalog.clone();
    let service_kind_lookup: ServiceKindLookup = Arc::new(move |id: &str| lookup_catalog.services.iter().find(|s| s.id == id).and_then(|s| s.kind));
    // Fallback "all" = every service the catalog knows — minus disabled ones, matching how a
    // declared `all` group expands past them at load.
    let all_targets: Vec<String> = catalog.groups.get("all").cloned().unwrap_or_else(|| catalog.services.iter().filter(|s| !s.disabled).map(|s| s.id.clone()).collect());

    let client = ManagerTuiClient::new(root, catalog);
    let (columns, rows) = crossterm::terminal::size().map(|(c, r)| (c as usize, r as usize)).unwrap_or((80, 24));

    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel::<WatchEvent>(256);

    let mut app = TuiApp {
        state: TuiState::new(service_kind_lookup),
        screen: ServiceScreen::new(Viewport { columns, rows }),
        all_targets,
        busy: false,
        start_all_pending: false,
        disposed: false,
        columns,
        rows,
        urls: Vec::new(),
        pending_reclaim: None,
        events: tx.clone(),
    };

    let _ = crossterm::terminal::enable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "{MOUSE_TRACKING_ON}");
    let _ = stdout.flush();

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
    /// A "start all" is waiting in its background task; a second one is refused until it reports.
    start_all_pending: bool,
    disposed: bool,
    columns: usize,
    rows: usize,
    /// Every service's resolved URLs, refreshed alongside each snapshot.
    urls: Vec<hearth_core::catalog::ResolvedServiceUrl>,
    /// The service name armed by a first Reclaim keypress — a second press on the same service is
    /// the user's confirmation to kill the port-holder. Any other key or a selection move clears
    /// it, so a stray Enter can never turn into a kill.
    pending_reclaim: Option<String>,
    /// The watch channel's sender, for background work (the "start all" wait) to report back on.
    events: mpsc::Sender<WatchEvent>,
}

impl TuiApp {
    async fn event_loop(&mut self, client: &ManagerTuiClient, mut rx: mpsc::Receiver<WatchEvent>, refresh_interval: Duration, cancel: CancellationToken) -> i32 {
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

    async fn handle_watch_event(&mut self, message: WatchEvent, client: &ManagerTuiClient, current_fence: &mut TuiFence) {
        match message {
            WatchEvent::BeginConnection => {
                *current_fence = self.state.begin_connection();
            }
            WatchEvent::Snapshot(services) => {
                if !self.state.apply_snapshot(*current_fence, &services) {
                    return;
                }
                // A pending reclaim's notice is the confirm prompt. A snapshot must not wipe it
                // or the second press has nothing left on screen telling the user what it will kill.
                if self.pending_reclaim.is_none() {
                    self.state.notice.clear();
                }
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
                if event.event_type == "service.log" {
                    self.draw();
                    if service_id.as_deref() == Some(self.state.selection.selected_name.as_str()) {
                        let fence = self.state.begin_request();
                        self.refresh_selected(client, fence).await;
                    }
                    return;
                }
                if TuiState::event_requires_services_snapshot(&event.event_type) {
                    match client.snapshot().await {
                        Ok(services) => {
                            if self.state.apply_snapshot(*current_fence, &services) {
                                if self.pending_reclaim.is_none() {
                                    self.state.notice.clear();
                                }
                                self.draw();
                                let fence = self.state.begin_request();
                                self.refresh_selected(client, fence).await;
                                if event.event_type == "operation.updated"
                                    && service_id.as_deref() == Some(self.state.selection.selected_name.as_str())
                                {
                                    let fence = self.state.begin_request();
                                    self.refresh_operation(client, fence).await;
                                }
                            }
                        }
                        Err(error) => {
                            self.state.notice = format!("Failed to refresh services: {}", safe_message(&error.message));
                            self.draw();
                        }
                    }
                    return;
                }
                self.draw();
            }
            WatchEvent::Unavailable(msg) => {
                if !self.state.connected(*current_fence) {
                    return;
                }
                self.state.notice = format!("Manager unavailable: {msg}");
                self.draw();
            }
            WatchEvent::BulkStartFinished(outcome) => {
                self.start_all_pending = false;
                self.state.notice = match outcome {
                    Ok(operation) if operation.status == OperationStatus::Failed => format!("start all failed: {}", safe_message(&operation.error.map(|e| e.message).unwrap_or_else(|| "start all failed".to_string()))),
                    Ok(_) => "start all: succeeded".to_string(),
                    Err(error) => format!("start all failed: {}", safe_message(&error)),
                };
                self.draw();
                self.reconcile(client).await;
            }
        }
    }

    async fn handle_terminal_event(&mut self, event: Event, client: &ManagerTuiClient) {
        match event {
            Event::Resize(columns, rows) => {
                self.columns = columns as usize;
                self.rows = rows as usize;
                self.screen.resize(Viewport { columns: self.columns, rows: self.rows });
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
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let selected = self.state.selection.selected().cloned();
                let Some(action) = keyboard_action(key, selected.as_ref()) else {
                    // Esc and any other unmapped key disarms a pending reclaim. Repeat is ignored
                    // above, so holding the confirm key cannot fire the kill on its own.
                    if self.pending_reclaim.take().is_some() {
                        self.state.notice.clear();
                        self.draw();
                    }
                    return;
                };
                if action != TuiAction::Reclaim {
                    self.pending_reclaim = None;
                }
                match action {
                    TuiAction::Quit => self.shutdown(),
                    TuiAction::Up => self.move_selection(-1, client).await,
                    TuiAction::Down => self.move_selection(1, client).await,
                    TuiAction::StartAll => self.start_all(client).await,
                    TuiAction::StopAll => self.stop_all(client).await,
                    TuiAction::Start => self.run_action(ServiceOperationKind::Start, self.state.selection.selected_name.clone(), client, false).await,
                    TuiAction::Stop => self.run_action(ServiceOperationKind::Stop, self.state.selection.selected_name.clone(), client, false).await,
                    TuiAction::Restart => self.run_action(ServiceOperationKind::Restart, self.state.selection.selected_name.clone(), client, false).await,
                    TuiAction::Reclaim => self.reclaim(selected.as_ref(), client).await,
                }
            }
            _ => {}
        }
    }

    fn handle_wheel(&mut self, row: usize, delta: i64) {
        let rows = self.rows;
        if let Some(service) = self.screen.handle_wheel(row, delta, Some(rows)) {
            // A selection move disarms a pending reclaim, wheel included.
            if self.state.selection.select(&service) {
                self.pending_reclaim = None;
            }
            self.draw();
        }
    }

    async fn select_service(&mut self, name: &str, client: &ManagerTuiClient) {
        self.pending_reclaim = None;
        if !self.state.selection.select(name) {
            return;
        }
        let fence = self.state.begin_selection();
        self.draw();
        self.refresh_selected(client, fence).await;
    }

    async fn move_selection(&mut self, delta: i64, client: &ManagerTuiClient) {
        if !self.state.selection.move_by(delta) {
            return;
        }
        let fence = self.state.begin_selection();
        self.draw();
        self.refresh_selected(client, fence).await;
    }

    async fn reconcile(&mut self, client: &ManagerTuiClient) {
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
                if self.pending_reclaim.is_none() {
                    self.state.notice.clear();
                }
                self.draw();
                let fence = self.state.begin_request();
                self.refresh_selected(client, fence).await;
            }
            Err(error) => {
                if !self.state.current(fence) {
                    return;
                }
                self.state.notice = format!("Manager unavailable: {}", safe_message(&error.message));
                self.draw();
            }
        }
    }

    async fn refresh_selected(&mut self, client: &ManagerTuiClient, fence: TuiFence) {
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

    async fn refresh_operation(&mut self, client: &ManagerTuiClient, fence: TuiFence) {
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

    /// POSTs the bulk start inline, then waits for it in a spawned task that reports back as
    /// `WatchEvent::BulkStartFinished` — the wait can take minutes and must never block input.
    async fn start_all(&mut self, client: &ManagerTuiClient) {
        if self.busy || self.start_all_pending {
            return;
        }
        self.busy = true;
        self.state.notice = "start all…".to_string();
        self.draw();
        let accepted = match client.bulk_start(&self.all_targets).await {
            Ok(operation) => client.connection().await.map(|connection| (operation, connection)),
            Err(error) => Err(error),
        };
        self.busy = false;
        match accepted {
            Ok((operation, connection)) => {
                self.start_all_pending = true;
                self.state.notice = format!("start all: {}", operation.id);
                self.draw();
                let events = self.events.clone();
                tokio::spawn(async move {
                    let outcome = connection.wait(&operation.id, None).await.map_err(|e| e.message);
                    let _ = events.send(WatchEvent::BulkStartFinished(outcome)).await;
                });
            }
            Err(error) => {
                self.state.notice = format!("start all failed: {}", safe_message(&error.message));
                self.draw();
            }
        }
    }

    async fn stop_all(&mut self, client: &ManagerTuiClient) {
        let services: Vec<String> = self.state.selection.services.iter().map(|s| s.name.clone()).collect();
        for service in services {
            if self.disposed {
                return;
            }
            self.run_action(ServiceOperationKind::Stop, service, client, false).await;
        }
    }

    /// First press arms the reclaim (the notice names the holder); a second press on the same
    /// service sends `killUnowned`. Arming is per-service — switching rows re-asks.
    async fn reclaim(&mut self, selected: Option<&crate::state::Service>, client: &ManagerTuiClient) {
        let Some(selected) = selected else { return };
        let service = selected.name.clone();
        if self.pending_reclaim.as_deref() != Some(service.as_str()) {
            // Lifecycle events don't carry `error`, so refresh the row before naming the holder.
            self.reconcile(client).await;
            let Some(refreshed) = self.state.selection.services.iter().find(|s| s.name == service && s.state == ActualServiceState::ExternallyOwned) else {
                self.state.notice = format!("{service} is no longer externally owned");
                self.draw();
                return;
            };
            let holder = refreshed.error.clone().unwrap_or_else(|| format!("{service} is externally owned"));
            self.pending_reclaim = Some(service.clone());
            self.state.notice = format!("{holder} — press again to kill it and start {service}");
            self.draw();
            return;
        }
        self.pending_reclaim = None;
        self.run_action(ServiceOperationKind::Start, service, client, true).await;
    }

    async fn run_action(&mut self, action: ServiceOperationKind, service: String, client: &ManagerTuiClient, kill_unowned: bool) {
        if self.busy || service.is_empty() {
            return;
        }
        let fence = self.state.begin_action(&service, action);
        self.busy = true;
        self.state.notice = format!("{} {service}…", action.as_wire_str());
        self.draw();
        let result = client.action(&service, action, kill_unowned).await;
        // Cleared before the reconcile below, which is a no-op while `busy` is set.
        self.busy = false;
        match result {
            Ok(operation) => {
                if self.state.apply_action(&fence, operation) {
                    self.draw();
                    self.reconcile(client).await;
                }
            }
            Err(error) => {
                if self.state.apply_action_failure(&fence, &safe_message(&error.message)) {
                    self.draw();
                }
            }
        }
    }

    fn shutdown(&mut self) {
        if self.disposed {
            return;
        }
        self.disposed = true;
        self.state.begin_connection();
    }

    /// The focused service's URLs as screen rows, flagging the ones that need the service running
    /// while it is not — the same rule as `hearth urls`.
    fn focused_urls(&self) -> Vec<String> {
        let selected = &self.state.selection.selected_name;
        let running = self.state.selection.services.iter().find(|s| &s.name == selected).is_some_and(|s| {
            matches!(s.state, ActualServiceState::Ready | ActualServiceState::Running | ActualServiceState::RunningUnready | ActualServiceState::Starting | ActualServiceState::Preparing)
        });
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
