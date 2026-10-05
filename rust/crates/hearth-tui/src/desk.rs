//! Layout, key map, and Ratatui paint for the workspace shell. No network: `shell` applies
//! [`Command`]s and draws a [`Frame`]. The command table is the key map and the footer.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hearth_core::catalog::{
    CatalogGroup, CommandSpec, ReadinessSpec, ServiceCatalog, ServiceDefinition, ServiceKind,
    ServiceRunProfile,
};
use hearth_core::state::{ActualServiceState, ServiceOperationKind};
use hearth_core::workspaces::{display_path, folder_name};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

use crate::state::Service;
use crate::text_utils::{sgr_to_line, visible_width};

pub const DAEMON_ID: &str = "$daemon";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    Workspaces,
    Services,
    Shared,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowId {
    Daemon,
    Group(String),
    Service(String),
    Recipe(String),
    Instance(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    Trust(String),
    Forget(String),
    StopDaemon(String),
    RestartDaemon(String),
    Reclaim(String),
    /// Second press removes the instance. `affected` names every attached workspace.
    /// `unchecked` means the attachment list could not be read, so the press is an explicit
    /// override rather than a known blast radius.
    RemoveShared {
        id: String,
        affected: Vec<String>,
        unchecked: bool,
    },
    /// Second press runs `follow`. The notice already names the other workspaces.
    SharedImpact {
        notice: String,
        follow: ImpactFollow,
    },
}

/// What a confirmed shared-impact alert does. Project actions only detach or reattach this
/// workspace. Instance actions stop, restart, or delete the singleton for every attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImpactFollow {
    Instance {
        id: String,
        action: String,
    },
    Remove {
        id: String,
        force: bool,
    },
    Service {
        id: String,
        action: ServiceOperationKind,
    },
    Many {
        label: String,
        ids: Vec<String>,
        action: ServiceOperationKind,
    },
}

/// One shared instance another workspace is attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedTouch {
    pub instance: String,
    pub others: Vec<String>,
}

/// A workspace the TUI already knows, used to turn an attachment root into a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownRoot {
    pub root: String,
    pub name: String,
    pub path: String,
}

/// Attachment roots split into every user and the users that are not the selected workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentReport {
    pub all: Vec<String>,
    pub others: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Quit,
    ToggleHelp,
    CloseHelp,
    ClearArm,
    FocusNext,
    Move(i64),
    ScrollLog(i64),
    ToggleShared,
    ToggleMouse,
    Activate,
    Stop,
    Restart,
    StartAll,
    StopAll,
    Reclaim,
    Add,
    Forget,
    StopDaemon,
    RestartDaemon,
    ReloadCatalog,
    CopyUrl,
    OpenFolder,
    Updates,
    RemoveShared,
    CycleVersion(i64),
    Type(char),
    Backspace,
    SubmitComposer,
    CancelComposer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Workspace(usize),
    Service(usize),
    Shared(usize),
}

/// What Enter does to the focused row, before the two-press arm. Destructive confirms are separate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    Trust(String),
    StartDaemon(String),
    FocusServices,
    StartService(String),
    StopService(String),
    Reclaim(String),
    StartGroup(String),
    RestartGroup(String),
    Install(String),
    StartInstance(String),
    StopInstance(String),
    Disabled,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceMeta {
    pub id: String,
    pub label: String,
    pub ports: String,
    pub disabled: bool,
    pub finite: bool,
    pub infra: bool,
    /// Synthesized from a `shared:` entry. `shared_instance` is `name@version`.
    pub shared: bool,
    pub shared_instance: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceLine {
    pub id: String,
    pub label: String,
    pub ports: String,
    pub state: ActualServiceState,
    pub disabled: bool,
    pub finite: bool,
    pub infra: bool,
    pub shared: bool,
    pub shared_instance: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub name: Option<String>,
    pub services: Vec<ServiceLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLine {
    pub id: String,
    pub name: String,
    /// `~/`-shortened path. Empty in tests that only care about the name.
    pub path: String,
    pub trusted: bool,
    pub missing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeLine {
    pub name: String,
    pub versions: Vec<String>,
    pub version_index: usize,
}

impl RecipeLine {
    pub fn chosen(&self) -> String {
        let version = self
            .versions
            .get(self.version_index)
            .map(String::as_str)
            .unwrap_or("");
        format!("{}@{version}", self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceLine {
    pub id: String,
    pub install_state: String,
    pub actual_state: String,
    pub port: u16,
    pub attachments: usize,
    pub install_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visual {
    Label,
    Item,
}

#[derive(Debug, Clone, Copy, Default)]
struct Geometry {
    workspace_items: Rect,
    workspace_offset: usize,
    main: Pane,
    main_items: Rect,
    main_offset: usize,
    log_items: Rect,
    modal: Rect,
}

/// An http(s) URL that was painted this frame, with the cells it occupies. The shell re-writes
/// those cells as an OSC 8 hyperlink after `terminal.draw`, and a drag on them is selected here:
/// mouse reporting swallows the terminal's own selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub x: u16,
    pub y: u16,
    /// The painted text the sequence wraps, clipped to the cells that were drawn.
    pub text: String,
    /// The full URL. Copied when the whole visible run is selected, including a clipped tail.
    pub target: String,
    /// Inner pane origin of the line, so a press on the `URL ` prefix still hits this link.
    pub row_x: u16,
    pub row_width: u16,
    /// Service URL rows accept a press anywhere on the line. Log lines only on the URL itself.
    pub row_press: bool,
}

impl Link {
    pub fn end_x(&self) -> u16 {
        self.x.saturating_add(visible_width(&self.text) as u16)
    }

    fn contains_cell(&self, column: usize, row: usize) -> bool {
        row == self.y as usize && column >= self.x as usize && column < self.end_x() as usize
    }

    fn on_row(&self, column: usize, row: usize) -> bool {
        row == self.y as usize
            && column >= self.row_x as usize
            && column < self.row_x as usize + self.row_width as usize
    }
}

/// The link run the pointer is selecting. Columns are screen columns; `end` is exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSelection {
    pub y: u16,
    pub x: u16,
    pub start: u16,
    pub end: u16,
    pub text: String,
    pub target: String,
}

impl LinkSelection {
    /// The whole visible run copies `target`, so a clipped URL still yields the full address.
    pub fn copied(&self) -> String {
        let start = self.start.saturating_sub(self.x) as usize;
        let end = self.end.saturating_sub(self.x) as usize;
        let visible = slice_columns(&self.text, start, end);
        if visible == self.text {
            self.target.clone()
        } else {
            visible
        }
    }
}

pub struct Desk {
    pub focus: Pane,
    pub shared_open: bool,
    pub help: bool,
    pub composer: Option<String>,
    pub pending: Option<Pending>,
    pub notice: String,
    pub workspaces: Vec<WorkspaceLine>,
    pub workspace_index: usize,
    pub sections: Vec<Section>,
    pub service_cursor: usize,
    pub urls: Vec<String>,
    pub log_title: String,
    pub log: String,
    pub summary: String,
    pub recipes: Vec<RecipeLine>,
    pub instances: Vec<InstanceLine>,
    pub shared_cursor: usize,
    pub start_all: Vec<String>,
    pub attached: bool,
    pub daemon_pid: Option<i64>,
    pub log_scroll: usize,
    workspace_offset: usize,
    service_offset: usize,
    shared_offset: usize,
    geometry: Geometry,
    pub links: Vec<Link>,
    pub link_selection: Option<LinkSelection>,
}

impl Default for Desk {
    fn default() -> Self {
        Self {
            focus: Pane::Workspaces,
            shared_open: false,
            help: false,
            composer: None,
            pending: None,
            notice: String::new(),
            workspaces: Vec::new(),
            workspace_index: 0,
            sections: Vec::new(),
            service_cursor: 0,
            urls: Vec::new(),
            log_title: "log".to_string(),
            log: String::new(),
            summary: String::new(),
            recipes: Vec::new(),
            instances: Vec::new(),
            shared_cursor: 0,
            start_all: Vec::new(),
            attached: false,
            daemon_pid: None,
            log_scroll: 0,
            workspace_offset: 0,
            service_offset: 0,
            shared_offset: 0,
            geometry: Geometry::default(),
            links: Vec::new(),
            link_selection: None,
        }
    }
}

impl Desk {
    pub fn selected_workspace(&self) -> Option<&WorkspaceLine> {
        self.workspaces.get(self.workspace_index)
    }

    pub fn service_ids(&self) -> Vec<RowId> {
        service_ids(&self.sections, self.selected_workspace().is_some())
    }

    pub fn shared_ids(&self) -> Vec<RowId> {
        let mut rows = Vec::new();
        for recipe in &self.recipes {
            rows.push(RowId::Recipe(recipe.name.clone()));
        }
        for instance in &self.instances {
            rows.push(RowId::Instance(instance.id.clone()));
        }
        rows
    }

    pub fn selected_service(&self) -> Option<RowId> {
        self.service_ids().get(self.service_cursor).cloned()
    }

    pub fn selected_shared(&self) -> Option<RowId> {
        self.shared_ids().get(self.shared_cursor).cloned()
    }

    pub fn service_line(&self, id: &str) -> Option<&ServiceLine> {
        self.sections
            .iter()
            .flat_map(|section| section.services.iter())
            .find(|service| service.id == id)
    }

    /// Replaces the service tree and keeps the cursor on the same row when it still exists.
    pub fn set_sections(&mut self, sections: Vec<Section>, start_all: Vec<String>) {
        let keep = self.selected_service();
        self.sections = sections;
        self.start_all = start_all;
        let ids = self.service_ids();
        self.service_cursor = keep
            .and_then(|id| ids.iter().position(|row| row == &id))
            .unwrap_or(0);
    }

    /// First call arms `pending` and returns false. The same call again returns true and clears it.
    pub fn arm(&mut self, pending: Pending) -> bool {
        if self.pending.as_ref() == Some(&pending) {
            self.pending = None;
            true
        } else {
            self.pending = Some(pending);
            false
        }
    }

    pub fn clear_arm(&mut self) {
        self.pending = None;
    }

    pub fn nudge(&mut self, delta: i64) -> bool {
        self.pending = None;
        self.log_scroll = 0;
        match self.focus {
            Pane::Workspaces => slide(&mut self.workspace_index, self.workspaces.len(), delta),
            Pane::Services => {
                let len = self.service_ids().len();
                slide(&mut self.service_cursor, len, delta)
            }
            Pane::Shared => {
                let len = self.shared_ids().len();
                slide(&mut self.shared_cursor, len, delta)
            }
        }
    }

    pub fn focus_next(&mut self) {
        self.pending = None;
        self.focus = match self.focus {
            Pane::Workspaces if self.shared_open => Pane::Shared,
            Pane::Workspaces => Pane::Services,
            Pane::Services | Pane::Shared => Pane::Workspaces,
        };
    }

    pub fn toggle_shared(&mut self) {
        self.pending = None;
        self.shared_open = !self.shared_open;
        self.focus = if self.shared_open {
            Pane::Shared
        } else {
            Pane::Services
        };
    }

    pub fn cycle_version(&mut self, delta: i64) -> bool {
        if self.focus != Pane::Shared {
            return false;
        }
        let RowId::Recipe(name) = self.selected_shared().unwrap_or(RowId::Daemon) else {
            return false;
        };
        let Some(recipe) = self.recipes.iter_mut().find(|recipe| recipe.name == name) else {
            return false;
        };
        let len = recipe.versions.len() as i64;
        if len == 0 {
            return false;
        }
        let next = (recipe.version_index as i64 + delta).rem_euclid(len) as usize;
        if next == recipe.version_index {
            return false;
        }
        recipe.version_index = next;
        true
    }

    pub fn scroll_log(&mut self, delta: i64) {
        if delta > 0 {
            self.log_scroll = self.log_scroll.saturating_add(delta as usize);
        } else {
            self.log_scroll = self.log_scroll.saturating_sub((-delta) as usize);
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.geometry = Geometry::default();
        self.links.clear();
        if area.width == 0 || area.height == 0 {
            self.link_selection = None;
            return;
        }
        let (header_h, footer_h) = chrome_heights(area.height);
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(header_h),
            Constraint::Min(0),
            Constraint::Length(footer_h),
        ])
        .areas(area);
        if header_h > 0 {
            frame.render_widget(
                Paragraph::new(title_line(&self.summary, self.daemon_pid)).style(theme::title()),
                header,
            );
        }
        if footer_h > 0 {
            frame.render_widget(
                Paragraph::new(footer_line(self, footer.width as usize)),
                footer,
            );
        }
        if self.help {
            self.draw_help(frame, body);
        } else if let Some(sidebar) = split_at(body.width as usize) {
            self.draw_split(frame, body, sidebar as u16);
        } else {
            self.draw_narrow(frame, body);
        }
        self.reconcile_link_selection();
        self.paint_link_chrome(frame.buffer_mut());
        if self.composer.is_some() || self.pending.is_some() {
            self.draw_modal(frame, area);
        }
    }

    /// A press on a URL, or on the only URL of that row (the `URL ` label included).
    pub fn link_for_press(&self, column: usize, row: usize) -> Option<Link> {
        if self.help || self.composer.is_some() {
            return None;
        }
        if self.pending.is_some() && contains_cell(self.geometry.modal, column, row) {
            return None;
        }
        let hits: Vec<&Link> = self
            .links
            .iter()
            .filter(|link| link.on_row(column, row))
            .collect();
        if let Some(link) = hits.iter().find(|link| link.contains_cell(column, row)) {
            return Some((*link).clone());
        }
        if hits.len() == 1 && hits[0].row_press {
            Some(hits[0].clone())
        } else {
            None
        }
    }

    /// `moved` is a drag that travelled at least two columns. A click, or a one-cell jitter,
    /// keeps the whole URL so the copy is the address and not a single character.
    pub fn select_link(&mut self, link: &Link, anchor: u16, current: u16, moved: bool) {
        let origin = link.x;
        let end = link.end_x();
        let (start, stop) = link_range(origin, end, anchor, current, moved);
        self.link_selection = Some(LinkSelection {
            y: link.y,
            x: origin,
            start,
            end: stop,
            text: link.text.clone(),
            target: link.target.clone(),
        });
    }

    pub fn clear_link_selection(&mut self) {
        self.link_selection = None;
    }

    fn reconcile_link_selection(&mut self) {
        let keep = self.link_selection.as_ref().is_some_and(|selection| {
            self.links.iter().any(|link| {
                link.y == selection.y && link.x == selection.x && link.target == selection.target
            })
        });
        if !keep {
            self.link_selection = None;
        }
    }

    fn paint_link_chrome(&self, buf: &mut Buffer) {
        for link in &self.links {
            for x in link.x..link.end_x() {
                if let Some(cell) = buf.cell_mut((x, link.y)) {
                    cell.modifier.insert(Modifier::UNDERLINED);
                }
            }
        }
        let Some(selection) = &self.link_selection else {
            return;
        };
        for x in selection.start..selection.end {
            if let Some(cell) = buf.cell_mut((x, selection.y)) {
                cell.modifier.insert(Modifier::UNDERLINED);
                cell.modifier.insert(Modifier::REVERSED);
            }
        }
    }

    pub fn hit(&self, column: usize, row: usize) -> Option<Hit> {
        if self.help || self.composer.is_some() {
            return None;
        }
        if self.pending.is_some() && contains_cell(self.geometry.modal, column, row) {
            return None;
        }
        let geo = &self.geometry;
        if contains_cell(geo.workspace_items, column, row) {
            let index = geo.workspace_offset + row - geo.workspace_items.y as usize;
            return self.workspaces.get(index).map(|_| Hit::Workspace(index));
        }
        if contains_cell(geo.main_items, column, row) {
            let slot = geo.main_offset + row - geo.main_items.y as usize;
            let visuals = match geo.main {
                Pane::Shared => shared_visuals(self),
                Pane::Services => service_visuals(self),
                Pane::Workspaces => return None,
            };
            let painted = visuals.get(slot)?;
            if painted.kind != Visual::Item {
                return None;
            }
            return Some(match geo.main {
                Pane::Shared => Hit::Shared(painted.selectable),
                _ => Hit::Service(painted.selectable),
            });
        }
        None
    }

    /// Scrolls the list under the pointer. Returns whether the workspace or service cursor moved.
    pub fn wheel(&mut self, column: usize, row: usize, delta: i64) -> bool {
        if delta == 0 || self.help || self.composer.is_some() {
            return false;
        }
        if self.pending.is_some() && contains_cell(self.geometry.modal, column, row) {
            return false;
        }
        let geo = self.geometry;
        if contains_cell(geo.workspace_items, column, row) {
            let before = self.workspace_index;
            scroll_window(
                &mut self.workspace_offset,
                &mut self.workspace_index,
                self.workspaces.len(),
                geo.workspace_items.height as usize,
                delta,
            );
            return self.workspace_index != before;
        }
        if contains_cell(geo.log_items, column, row) {
            self.scroll_log(-delta);
            return false;
        }
        if !contains_cell(geo.main_items, column, row) {
            return false;
        }
        match geo.main {
            Pane::Shared => {
                let before = self.shared_cursor;
                let len = self.shared_ids().len();
                let _ = slide(&mut self.shared_cursor, len, delta);
                self.shared_cursor != before
            }
            Pane::Services => {
                let before = self.service_cursor;
                let len = service_visuals(self).len();
                let height = geo.main_items.height as usize;
                if height == 0 || len <= height {
                    return false;
                }
                let max = len - height;
                let next = (self.service_offset as i64 + delta).clamp(0, max as i64) as usize;
                if next == self.service_offset {
                    return false;
                }
                self.service_offset = next;
                let visuals = service_visuals(self);
                let current = visual_index_of_item(&visuals, self.service_cursor).unwrap_or(0);
                if current < self.service_offset {
                    if let Some(item) = visuals
                        .iter()
                        .skip(self.service_offset)
                        .find(|row| row.kind == Visual::Item)
                    {
                        self.service_cursor = item.selectable;
                    }
                } else if current >= self.service_offset + height {
                    if let Some(item) = visuals
                        .iter()
                        .take(self.service_offset + height)
                        .rev()
                        .find(|row| row.kind == Visual::Item)
                    {
                        self.service_cursor = item.selectable;
                    }
                }
                self.service_cursor != before
            }
            Pane::Workspaces => false,
        }
    }

    fn header_notice(&self) -> String {
        if let Some(pending) = &self.pending {
            return arm_notice(pending, self);
        }
        self.notice.clone()
    }

    fn status_notice(&self) -> String {
        if self.composer.is_some() || self.pending.is_some() || self.help {
            return String::new();
        }
        plain_cell(&self.notice)
    }

    fn draw_help(&mut self, frame: &mut Frame, area: Rect) {
        // A short frame skips the border so the "untrusted" line still fits in 16 rows.
        let bordered = area.width >= 3 && area.height as usize >= HELP.len() + 2;
        let target = if bordered {
            let block = pane_block("help", true);
            let inner = block.inner(area);
            frame.render_widget(block, area);
            inner
        } else {
            area
        };
        let lines: Vec<Line> = HELP
            .iter()
            .take(target.height as usize)
            .map(|line| Line::from(*line))
            .collect();
        frame.render_widget(Paragraph::new(lines), target);
    }

    fn draw_split(&mut self, frame: &mut Frame, body: Rect, sidebar: u16) {
        let [left, right] =
            Layout::horizontal([Constraint::Length(sidebar), Constraint::Min(0)]).areas(body);
        let (height, offset) = self.fit_workspace(block_inner_height(left), self.workspaces.len());
        self.paint_workspaces(frame, left, height, offset, false);
        let main = if self.shared_open {
            Pane::Shared
        } else {
            Pane::Services
        };
        self.paint_main(frame, right, main);
    }

    fn draw_narrow(&mut self, frame: &mut Frame, body: Rect) {
        match self.focus {
            Pane::Workspaces => {
                let notice_rows = self.notice_rows();
                let (height, offset) = self.fit_workspace(
                    block_inner_height(body).saturating_sub(notice_rows),
                    self.workspaces.len(),
                );
                self.paint_workspaces(frame, body, height, offset, true);
                self.geometry.main = Pane::Workspaces;
            }
            Pane::Services | Pane::Shared => {
                let main = if self.focus == Pane::Shared || self.shared_open {
                    Pane::Shared
                } else {
                    Pane::Services
                };
                self.paint_main(frame, body, main);
            }
        }
    }

    fn fit_workspace(&mut self, height: usize, len: usize) -> (usize, usize) {
        let height = height.min(len);
        reveal(
            &mut self.workspace_offset,
            self.workspace_index,
            height,
            len,
        );
        (height, self.workspace_offset)
    }

    /// The status notice is one row at the top of the visible pane, not a banner over it.
    fn notice_rows(&self) -> usize {
        usize::from(!self.status_notice().is_empty())
    }

    fn paint_notice(&self, frame: &mut Frame, inner: Rect, show: bool) -> usize {
        let notice = if show {
            self.status_notice()
        } else {
            String::new()
        };
        if notice.is_empty() || inner.width == 0 || inner.height == 0 {
            return 0;
        }
        let line = Line::styled(notice, theme::notice());
        frame
            .buffer_mut()
            .set_line(inner.x, inner.y, &line, inner.width);
        1
    }

    fn paint_workspaces(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        item_slots: usize,
        offset: usize,
        show_notice: bool,
    ) {
        let focused = self.focus == Pane::Workspaces;
        let block = pane_block("WORKSPACES", focused);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let notice_rows = self.paint_notice(frame, inner, show_notice);
        let rows = item_slots.min((inner.height as usize).saturating_sub(notice_rows));
        self.geometry.workspace_items = Rect {
            x: inner.x,
            y: inner.y + notice_rows as u16,
            width: inner.width,
            height: rows as u16,
        };
        self.geometry.workspace_offset = offset;
        let buf = frame.buffer_mut();
        for slot in 0..rows {
            let index = offset + slot;
            let current = index == self.workspace_index;
            let summary = if current { self.summary.as_str() } else { "" };
            let line = self
                .workspaces
                .get(index)
                .map(|workspace| workspace_line(workspace, focused && current, summary))
                .unwrap_or_else(|| Line::from(""));
            paint_line(
                buf,
                inner.x,
                inner.y + notice_rows as u16 + slot as u16,
                inner.width,
                &line,
                focused && current,
            );
        }
    }

    fn paint_main(&mut self, frame: &mut Frame, area: Rect, pane: Pane) {
        let (list_area, log_area) = split_main(area);
        let visuals = if pane == Pane::Shared {
            shared_visuals(self)
        } else {
            service_visuals(self)
        };
        let cursor = if pane == Pane::Shared {
            self.shared_cursor
        } else {
            self.service_cursor
        };
        let focused = self.focus == pane;
        let title = pane_title(self, pane);
        let block = pane_block(&title, focused);
        let inner = block.inner(list_area);
        frame.render_widget(block, list_area);
        let notice_rows = self.paint_notice(frame, inner, true);
        let inner = Rect {
            y: inner.y + notice_rows as u16,
            height: inner.height.saturating_sub(notice_rows as u16),
            ..inner
        };
        let url_rows = if pane == Pane::Services {
            self.urls.len().min(3).min(inner.height as usize)
        } else {
            0
        };
        let url_rows = if (inner.height as usize) > url_rows {
            url_rows
        } else {
            0
        };
        let item_capacity = (inner.height as usize).saturating_sub(url_rows);
        let item_height = item_capacity.min(visuals.len());
        let visual_cursor = visual_index_of_item(&visuals, cursor).unwrap_or(0);
        let offset = {
            let slot = if pane == Pane::Shared {
                &mut self.shared_offset
            } else {
                &mut self.service_offset
            };
            reveal(slot, visual_cursor, item_height, visuals.len());
            *slot
        };
        self.geometry.main = pane;
        self.geometry.main_offset = offset;
        self.geometry.main_items = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: item_height as u16,
        };
        {
            let buf = frame.buffer_mut();
            for slot in 0..item_height {
                let painted = visuals.get(offset + slot);
                let selected = painted.is_some_and(|row| {
                    focused && row.kind == Visual::Item && row.selectable == cursor
                });
                let line = painted
                    .map(|row| row.line.clone())
                    .unwrap_or_else(|| Line::from(""));
                paint_line(
                    buf,
                    inner.x,
                    inner.y + slot as u16,
                    inner.width,
                    &line,
                    selected,
                );
            }
            if pane == Pane::Services {
                for (index, url) in self.urls.iter().take(url_rows).enumerate() {
                    let line = Line::styled(format!("URL {url}"), theme::dim());
                    let y = inner.y + item_height as u16 + index as u16;
                    buf.set_line(inner.x, y, &line, inner.width);
                    collect_links(&line, inner.x, y, inner.width, true, &mut self.links);
                }
            }
        }
        if log_area.height == 0 {
            return;
        }
        let log_title = format!("LOG — {}", self.log_title);
        let log_block = pane_block(&log_title, false);
        let log_inner = log_block.inner(log_area);
        frame.render_widget(log_block, log_area);
        let rows = log_window(&self.log, log_inner.height as usize, &mut self.log_scroll);
        self.geometry.log_items = log_inner;
        let buf = frame.buffer_mut();
        for (index, row) in rows.iter().enumerate() {
            let line = sgr_to_line(row);
            let y = log_inner.y + index as u16;
            buf.set_line(log_inner.x, y, &line, log_inner.width);
            collect_links(
                &line,
                log_inner.x,
                y,
                log_inner.width,
                false,
                &mut self.links,
            );
        }
    }

    fn draw_modal(&mut self, frame: &mut Frame, area: Rect) {
        let composing = self.composer.clone();
        if composing.is_none() && self.pending.is_none() {
            return;
        }
        let title = if composing.is_some() {
            "add folder"
        } else {
            "confirm"
        };
        let rect = centered(area, modal_width(area), modal_height(area));
        self.geometry.modal = rect;
        frame.render_widget(Clear, rect);
        let block = Block::bordered()
            .title(title)
            .border_style(theme::modal())
            .title_style(theme::title());
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let keymap = footer_line(self, inner.width as usize);
        let body_rows = inner.height.saturating_sub(1) as usize;
        let body = if let Some(text) = composing {
            composer_lines(&text, inner.width as usize, body_rows)
        } else {
            fitted_lines(&self.header_notice(), inner.width as usize, body_rows)
                .into_iter()
                .map(Line::from)
                .collect()
        };
        let buf = frame.buffer_mut();
        for (index, line) in body.iter().enumerate().take(body_rows) {
            buf.set_line(inner.x, inner.y + index as u16, line, inner.width);
        }
        if inner.height > 0 {
            buf.set_line(inner.x, inner.y + inner.height - 1, &keymap, inner.width);
        }
    }
}

struct Painted {
    line: Line<'static>,
    kind: Visual,
    selectable: usize,
}

fn service_visuals(desk: &Desk) -> Vec<Painted> {
    let mut rows = Vec::new();
    let mut selectable = 0;
    let focused = desk.focus == Pane::Services;
    if desk.selected_workspace().is_some() {
        let selected = focused && desk.service_cursor == selectable;
        rows.push(Painted {
            line: item_line(selected, "daemon log"),
            kind: Visual::Item,
            selectable,
        });
        selectable += 1;
    }
    let named = desk.sections.iter().any(|section| section.name.is_some());
    for section in &desk.sections {
        if let Some(name) = &section.name {
            let selected = focused && desk.service_cursor == selectable;
            rows.push(Painted {
                line: item_line(selected, &name.to_uppercase()),
                kind: Visual::Item,
                selectable,
            });
            selectable += 1;
        } else if named {
            rows.push(Painted {
                line: Line::styled("OTHER", theme::dim()),
                kind: Visual::Label,
                selectable,
            });
        }
        for service in &section.services {
            let selected = focused && desk.service_cursor == selectable;
            rows.push(Painted {
                line: service_line(service, selected),
                kind: Visual::Item,
                selectable,
            });
            selectable += 1;
        }
    }
    rows
}

fn shared_visuals(desk: &Desk) -> Vec<Painted> {
    let mut rows = Vec::new();
    let mut selectable = 0;
    let focused = desk.focus == Pane::Shared;
    if !desk.recipes.is_empty() {
        rows.push(Painted {
            line: Line::styled("RECIPES", theme::dim()),
            kind: Visual::Label,
            selectable,
        });
    }
    for recipe in &desk.recipes {
        let selected = focused && desk.shared_cursor == selectable;
        let installed = desk
            .instances
            .iter()
            .any(|instance| instance.id == recipe.chosen());
        let version = recipe
            .versions
            .get(recipe.version_index)
            .map(String::as_str)
            .unwrap_or("");
        rows.push(Painted {
            line: recipe_line(selected, &recipe.name, version, installed),
            kind: Visual::Item,
            selectable,
        });
        selectable += 1;
    }
    if !desk.instances.is_empty() {
        rows.push(Painted {
            line: Line::styled("INSTALLED", theme::dim()),
            kind: Visual::Label,
            selectable,
        });
    }
    for instance in &desk.instances {
        let selected = focused && desk.shared_cursor == selectable;
        let projects = match instance.attachments {
            0 => String::new(),
            1 => "  1 project".to_string(),
            n => format!("  {n} projects"),
        };
        let failure = instance
            .install_error
            .as_deref()
            .filter(|text| !text.is_empty())
            .map(|text| format!("  {text}"))
            .unwrap_or_default();
        rows.push(Painted {
            line: instance_line(instance, selected, &projects, &failure),
            kind: Visual::Item,
            selectable,
        });
        selectable += 1;
    }
    rows
}

fn pane_title(desk: &Desk, pane: Pane) -> String {
    match pane {
        Pane::Shared => "SHARED".to_string(),
        Pane::Services | Pane::Workspaces => {
            let workspace = desk.selected_workspace();
            let name = workspace
                .map(|workspace| workspace.name.as_str())
                .unwrap_or("no workspace");
            let path = workspace
                .and_then(|workspace| {
                    (!workspace.path.is_empty()).then(|| format!("  {}", workspace.path))
                })
                .unwrap_or_default();
            let summary = if desk.summary.is_empty() {
                String::new()
            } else {
                format!("  {}", desk.summary)
            };
            format!("SERVICES  {name}{path}{summary}")
        }
    }
}

fn workspace_line(workspace: &WorkspaceLine, selected: bool, summary: &str) -> Line<'static> {
    let badge = if workspace.missing {
        "  missing"
    } else if !workspace.trusted {
        "  untrusted"
    } else {
        ""
    };
    let mark = if selected { ">" } else { " " };
    let mut spans = vec![Span::raw(format!("{mark} {}{badge}", workspace.name))];
    if !workspace.path.is_empty() {
        spans.push(Span::styled(format!(" {}", workspace.path), theme::dim()));
    }
    if !summary.is_empty() {
        spans.push(Span::raw(format!("  {summary}")));
    }
    Line::from(spans)
}

fn service_line(service: &ServiceLine, selected: bool) -> Line<'static> {
    let mark = if selected { ">" } else { " " };
    let (label, style) = if service.finite && service.state == ActualServiceState::Stopped {
        ("not run", theme::dim())
    } else {
        (compact_state(service.state), state_style(service.state))
    };
    let mut flags = String::new();
    if service.infra {
        flags.push_str(" infra");
    }
    if service.disabled {
        flags.push_str(" disabled");
    }
    if service.finite {
        flags.push_str(" job");
    }
    let name = if service.label.is_empty() || service.label == service.id {
        service.id.clone()
    } else {
        format!("{}  {}", service.label, service.id)
    };
    let ports = if service.ports.is_empty() {
        String::new()
    } else {
        format!("  {}", service.ports)
    };
    let detail = service
        .error
        .as_deref()
        .filter(|text| !text.is_empty())
        .map(|text| format!("  {text}"))
        .unwrap_or_default();
    let name_style = if service.disabled {
        theme::dim()
    } else {
        theme::title()
    };
    let tail_style = if service.disabled {
        theme::dim()
    } else {
        Style::default()
    };
    let mut spans = vec![
        Span::raw(format!("{mark} ")),
        Span::styled(format!("{label:<9}"), style),
    ];
    push_service_name(&mut spans, &name, name_style, service.shared);
    spans.push(Span::styled(format!("{ports}{flags}{detail}"), tail_style));
    Line::from(spans)
}

/// A project `shared:` row keeps the normal state colour. The word `shared` in the label is blue.
fn push_service_name(spans: &mut Vec<Span<'static>>, name: &str, style: Style, shared: bool) {
    if !shared {
        spans.push(Span::styled(format!(" {name}"), style));
        return;
    }
    let Some(index) = name.find("shared") else {
        spans.push(Span::styled(" shared ".to_string(), shared_word()));
        spans.push(Span::styled(name.to_string(), style));
        return;
    };
    let (before, after) = name.split_at(index);
    let after = &after["shared".len()..];
    if before.is_empty() {
        spans.push(Span::raw(" ".to_string()));
    } else {
        spans.push(Span::styled(format!(" {before}"), style));
    }
    spans.push(Span::styled("shared".to_string(), shared_word()));
    if !after.is_empty() {
        spans.push(Span::styled(after.to_string(), style));
    }
}

fn shared_word() -> Style {
    Style::default()
        .fg(Color::Blue)
        .add_modifier(Modifier::BOLD)
}

fn item_line(selected: bool, text: &str) -> Line<'static> {
    let mark = if selected { ">" } else { " " };
    Line::from(format!("{mark} {text}"))
}

fn recipe_line(selected: bool, name: &str, version: &str, installed: bool) -> Line<'static> {
    let mark = if selected { ">" } else { " " };
    let action = if installed { "installed" } else { "install" };
    Line::from(format!("{mark} {name}  {version}  {action}"))
}

fn instance_line(
    instance: &InstanceLine,
    selected: bool,
    projects: &str,
    failure: &str,
) -> Line<'static> {
    let mark = if selected { ">" } else { " " };
    let state = compact_wire(&instance.actual_state);
    Line::from(vec![
        Span::raw(format!(
            "{mark} {}  {}  ",
            instance.id, instance.install_state
        )),
        Span::styled(state.to_string(), wire_style(state)),
        Span::raw(format!("  :{}{projects}{failure}", instance.port)),
    ])
}

fn compact_state(state: ActualServiceState) -> &'static str {
    match state {
        ActualServiceState::Ready => "ready",
        ActualServiceState::QueuedStart => "queued",
        ActualServiceState::Running | ActualServiceState::RunningUnready => "running",
        ActualServiceState::Starting | ActualServiceState::Preparing => "starting",
        ActualServiceState::Stopping => "stopping",
        ActualServiceState::Succeeded => "succeeded",
        ActualServiceState::Failed => "failed",
        ActualServiceState::Orphaned => "orphaned",
        ActualServiceState::ExternallyOwned => "external",
        ActualServiceState::Stopped => "stopped",
    }
}

fn state_style(state: ActualServiceState) -> Style {
    Style::default().fg(match state {
        ActualServiceState::Ready | ActualServiceState::Succeeded => Color::Green,
        ActualServiceState::Running | ActualServiceState::RunningUnready => Color::Cyan,
        ActualServiceState::Preparing
        | ActualServiceState::QueuedStart
        | ActualServiceState::Starting
        | ActualServiceState::Stopping => Color::Yellow,
        ActualServiceState::Stopped => Color::DarkGray,
        ActualServiceState::Failed
        | ActualServiceState::Orphaned
        | ActualServiceState::ExternallyOwned => Color::Red,
    })
}

/// `name@version` carried by a project service synthesized from `shared:`.
pub fn shared_instance_of(service: &ServiceDefinition) -> Option<String> {
    let ServiceRunProfile::Verified { command, .. } = &service.profiles.run else {
        return None;
    };
    let CommandSpec::Argv { argv } = &command.command else {
        return None;
    };
    let index = argv
        .windows(2)
        .position(|pair| pair[0] == "shared" && pair[1] == "attach")?;
    let id = argv.get(index + 2)?;
    if id.contains('@') && !id.starts_with('-') {
        Some(id.clone())
    } else {
        None
    }
}

pub fn classify_attachments(
    roots: &[String],
    current: Option<&str>,
    known: &[KnownRoot],
) -> AttachmentReport {
    let others: Vec<String> = roots
        .iter()
        .filter(|root| current.is_none_or(|current| !same_root(root, current)))
        .cloned()
        .collect();
    AttachmentReport {
        all: labels_for(roots, known),
        others: labels_for(&others, known),
    }
}

pub fn instance_shared_notice(verb: &str, instance: &str, affected: &[String]) -> String {
    let who = join_names(affected);
    let (doing, consequence) = match verb {
        "restart" => (
            "Restarting",
            format!("takes the shared service down for {who}"),
        ),
        "remove" => (
            "Removing",
            format!("deletes its data and takes it down for {who}"),
        ),
        _ => (
            "Stopping",
            format!("takes the shared service down for {who}"),
        ),
    };
    format!("{doing} {instance} {consequence}. Press again to {verb}.")
}

pub fn project_shared_notice(
    verb: &str,
    current: &str,
    touches: &[SharedTouch],
    unknown: &[String],
) -> String {
    let mut sentences = Vec::new();
    if !touches.is_empty() {
        let parts: Vec<String> = touches
            .iter()
            .map(|touch| format!("{} ({})", touch.instance, join_names(&touch.others)))
            .collect();
        let listed = join_names(&parts);
        let (be, them) = if touches.len() == 1 {
            ("is", "it")
        } else {
            ("are", "them")
        };
        let doing = if verb == "restart" {
            "Restarting"
        } else {
            "Stopping"
        };
        let effect = match verb {
            "restart" => format!("only detaches and reattaches {current}"),
            _ => format!("only detaches {current}"),
        };
        sentences.push(format!(
            "{listed} {be} also used by other workspaces. {doing} {them} here {effect} — those workspaces keep {them}"
        ));
    }
    if !unknown.is_empty() {
        sentences.push(format!(
            "Could not check which workspaces use {}",
            join_names(unknown)
        ));
    }
    format!("{}. Press again to {verb}.", sentences.join(". "))
}

pub fn unchecked_shared_notice(verb: &str, instance: &str) -> String {
    format!("could not check which workspaces use {instance}. Press again to {verb} anyway.")
}

fn labels_for(roots: &[String], known: &[KnownRoot]) -> Vec<String> {
    let rows: Vec<(String, String)> = roots
        .iter()
        .map(|root| {
            let found = known.iter().find(|item| same_root(&item.root, root));
            let name = found
                .map(|item| item.name.clone())
                .unwrap_or_else(|| folder_name(root));
            let path = found
                .map(|item| item.path.clone())
                .unwrap_or_else(|| display_path(root));
            (name, path)
        })
        .collect();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for (name, _) in &rows {
        *counts.entry(name.clone()).or_insert(0) += 1;
    }
    let mut labels: Vec<String> = rows
        .into_iter()
        .map(|(name, path)| {
            if counts.get(&name).copied().unwrap_or(0) > 1 {
                format!("{name} ({path})")
            } else {
                name
            }
        })
        .collect();
    labels.sort();
    labels.dedup();
    labels
}

fn same_root(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
}

pub fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [one, two] => format!("{one} and {two}"),
        many => {
            let (last, rest) = many.split_last().unwrap();
            format!("{}, and {last}", rest.join(", "))
        }
    }
}

fn wire_style(state: &str) -> Style {
    let color = match state {
        "ready" | "succeeded" => Color::Green,
        "running" => Color::Cyan,
        "starting" | "queued" | "stopping" | "preparing" => Color::Yellow,
        "failed" | "orphaned" | "external" | "externally-owned" => Color::Red,
        _ => Color::DarkGray,
    };
    Style::default().fg(color)
}

fn title_line(summary: &str, pid: Option<i64>) -> Line<'static> {
    let mut text = if summary.is_empty() {
        "Hearth".to_string()
    } else {
        format!("Hearth  {summary}")
    };
    if let Some(pid) = pid {
        text.push_str(&format!("  pid {pid}"));
    }
    Line::styled(text, theme::title())
}

fn plain_cell(text: &str) -> String {
    sgr_to_line(text)
        .spans
        .into_iter()
        .map(|span| span.content.into_owned())
        .collect()
}

fn chrome_heights(height: u16) -> (u16, u16) {
    match height {
        0 => (0, 0),
        1 => (1, 0),
        _ => (1, 1),
    }
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        theme::focus()
    } else {
        theme::idle()
    };
    Block::bordered()
        .title(title)
        .border_style(style)
        .title_style(style)
}

fn block_inner_height(area: Rect) -> usize {
    pane_block("", false).inner(area).height as usize
}

fn split_main(area: Rect) -> (Rect, Rect) {
    if area.height < 6 {
        let log = Rect {
            x: area.x,
            y: area.bottom(),
            width: area.width,
            height: 0,
        };
        return (area, log);
    }
    let log_h = ((area.height as usize) / 3).clamp(3, area.height as usize - 3) as u16;
    let [list, log] =
        Layout::vertical([Constraint::Length(area.height - log_h), Constraint::Min(0)]).areas(area);
    (list, log)
}

fn paint_line(buf: &mut Buffer, x: u16, y: u16, width: u16, line: &Line, selected: bool) {
    if width == 0 {
        return;
    }
    if selected {
        buf.set_style(
            Rect {
                x,
                y,
                width,
                height: 1,
            },
            Style::default()
                .bg(Color::Indexed(238))
                .add_modifier(Modifier::BOLD),
        );
    }
    buf.set_line(x, y, line, width);
}

/// Finds every http(s) URL in a painted line and records where its text sits so the shell can
/// wrap those cells in an OSC 8 hyperlink after the draw. Anything past `width` was clipped off
/// screen and is ignored; a clipped tail links the visible prefix to the full target.
fn collect_links(line: &Line, x: u16, y: u16, width: u16, row_press: bool, links: &mut Vec<Link>) {
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let mut starts: Vec<usize> = text
        .match_indices("https://")
        .map(|(index, _)| index)
        .collect();
    starts.extend(text.match_indices("http://").map(|(index, _)| index));
    starts.sort_unstable();
    for start in starts {
        let column = visible_width(&text[..start]);
        if column >= width as usize {
            continue;
        }
        let tail = &text[start..];
        let end = tail
            .find(char::is_whitespace)
            .map(|offset| start + offset)
            .unwrap_or(text.len());
        let target = text[start..end]
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'']);
        if target.is_empty() {
            continue;
        }
        let text = clip_width(target, width as usize - column);
        if !text.is_empty() {
            links.push(Link {
                x: x + column as u16,
                y,
                text,
                target: target.to_string(),
                row_x: x,
                row_width: width,
                row_press,
            });
        }
    }
}

/// A drag shorter than two columns is a click: copy the whole URL, not the cell under the pointer.
fn link_range(origin: u16, end: u16, anchor: u16, current: u16, moved: bool) -> (u16, u16) {
    if end <= origin || !moved || anchor.abs_diff(current) < 2 {
        return (origin, end);
    }
    let inside = |column: u16| column >= origin && column < end;
    if !inside(anchor) && !inside(current) {
        return (origin, end);
    }
    let last = end - 1;
    let clamp = |column: u16| {
        if inside(column) {
            column
        } else if column < origin {
            origin
        } else {
            last
        }
    };
    let (start, stop) = (clamp(anchor), clamp(current));
    if start <= stop {
        (start, stop.saturating_add(1))
    } else {
        (stop, start.saturating_add(1))
    }
}

fn slice_columns(text: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut column = 0usize;
    for ch in text.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if column >= end {
            break;
        }
        if column >= start {
            out.push(ch);
        }
        column += width;
    }
    out
}

/// Bytes written at the link's first cell: OSC 8 around the visible text, underlined, with the
/// selected columns reversed. The escape stays out of the Ratatui buffer.
pub fn link_overlay(link: &Link, selection: Option<&LinkSelection>, fg: Color) -> String {
    let target: String = link.target.chars().filter(|ch| !ch.is_control()).collect();
    let selected = selection.filter(|selection| {
        selection.y == link.y && selection.x == link.x && selection.target == link.target
    });
    let (sel_start, sel_end) = selected
        .map(|selection| {
            (
                (selection.start.saturating_sub(link.x)) as usize,
                (selection.end.saturating_sub(link.x)) as usize,
            )
        })
        .unwrap_or((0, 0));
    let mut body = String::new();
    body.push_str("\x1b[0m");
    body.push_str(&sgr_fg(fg));
    body.push_str("\x1b[4m");
    let mut column = 0usize;
    let mut reversed = false;
    for ch in link.text.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        let in_sel =
            sel_end > sel_start && width > 0 && column < sel_end && column + width > sel_start;
        if in_sel != reversed {
            body.push_str(if in_sel { "\x1b[7m" } else { "\x1b[27m" });
            reversed = in_sel;
        }
        body.push(ch);
        column += width;
    }
    if reversed {
        body.push_str("\x1b[27m");
    }
    body.push_str("\x1b[0m");
    format!("\x1b]8;;{target}\x1b\\{body}\x1b]8;;\x1b\\")
}

fn sgr_fg(color: Color) -> String {
    match color {
        Color::Reset => "\x1b[39m".to_string(),
        Color::Black => "\x1b[30m".to_string(),
        Color::Red => "\x1b[31m".to_string(),
        Color::Green => "\x1b[32m".to_string(),
        Color::Yellow => "\x1b[33m".to_string(),
        Color::Blue => "\x1b[34m".to_string(),
        Color::Magenta => "\x1b[35m".to_string(),
        Color::Cyan => "\x1b[36m".to_string(),
        Color::Gray => "\x1b[37m".to_string(),
        Color::DarkGray => "\x1b[90m".to_string(),
        Color::LightRed => "\x1b[91m".to_string(),
        Color::LightGreen => "\x1b[92m".to_string(),
        Color::LightYellow => "\x1b[93m".to_string(),
        Color::LightBlue => "\x1b[94m".to_string(),
        Color::LightMagenta => "\x1b[95m".to_string(),
        Color::LightCyan => "\x1b[96m".to_string(),
        Color::White => "\x1b[97m".to_string(),
        Color::Indexed(index) => format!("\x1b[38;5;{index}m"),
        Color::Rgb(red, green, blue) => format!("\x1b[38;2;{red};{green};{blue}m"),
    }
}

fn contains_cell(rect: Rect, column: usize, row: usize) -> bool {
    let Ok(x) = u16::try_from(column) else {
        return false;
    };
    let Ok(y) = u16::try_from(row) else {
        return false;
    };
    rect.contains(Position { x, y })
}

fn modal_width(area: Rect) -> u16 {
    if area.width <= 24 {
        area.width
    } else {
        (area.width - 4).clamp(24, 68)
    }
}

fn modal_height(area: Rect) -> u16 {
    9.min(area.height)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn composer_lines(text: &str, width: usize, rows: usize) -> Vec<Line<'static>> {
    if rows == 0 || width == 0 {
        return Vec::new();
    }
    let cursor = Span::styled("█", theme::focus());
    let line = if text.is_empty() {
        Line::from(vec![Span::styled("absolute path", theme::dim()), cursor])
    } else {
        Line::from(vec![Span::raw(text.to_string()), cursor])
    };
    vec![line]
}

fn fitted_lines(text: &str, width: usize, rows: usize) -> Vec<String> {
    if width == 0 || rows == 0 {
        return Vec::new();
    }
    let wrapped = wrap_text(text, width);
    if wrapped.len() <= rows {
        wrapped
    } else {
        wrapped[wrapped.len() - rows..].to_vec()
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split_whitespace() {
        let word_width = visible_width(word);
        if word_width > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            let mut chunk = String::new();
            let mut chunk_width = 0usize;
            for ch in word.chars() {
                let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if chunk_width + ch_width > width && !chunk.is_empty() {
                    lines.push(std::mem::take(&mut chunk));
                    chunk_width = 0;
                }
                chunk.push(ch);
                chunk_width += ch_width;
            }
            current = chunk;
            current_width = chunk_width;
            continue;
        }
        let gap = usize::from(!current.is_empty());
        if current_width + gap + word_width > width {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if !current.is_empty() {
            current.push(' ');
            current_width += 1;
        }
        current.push_str(word);
        current_width += word_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Audience {
    Always,
    Workspace,
    Services,
    Shared,
    Main,
}

#[derive(Clone, Copy)]
enum Gate {
    Always,
    Focus(Pane),
}

struct Binding {
    code: KeyCode,
    command: Command,
    hint: Option<&'static str>,
    show: Audience,
    gate: Gate,
}

const fn bind(
    code: KeyCode,
    command: Command,
    hint: Option<&'static str>,
    show: Audience,
    gate: Gate,
) -> Binding {
    Binding {
        code,
        command,
        hint,
        show,
        gate,
    }
}

const COMPOSER_HINTS: &[&str] = &["esc cancel", "enter add", "bksp erase"];
const HELP_HINTS: &[&str] = &["q quit", "esc close", "? close"];
const CONFIRM_HINTS: &[&str] = &["enter confirm", "esc cancel"];

/// Dispatch order is first match. Hint order is this order, with aliases (`hint: None`) omitted.
const BINDINGS: &[Binding] = &[
    bind(
        KeyCode::Char('j'),
        Command::Move(1),
        Some("j/k move"),
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('k'),
        Command::Move(-1),
        Some("j/k move"),
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Enter,
        Command::Activate,
        Some("enter act"),
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('x'),
        Command::Stop,
        Some("x stop"),
        Audience::Main,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('a'),
        Command::StartAll,
        Some("a all"),
        Audience::Services,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('s'),
        Command::StopAll,
        Some("s stop all"),
        Audience::Services,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('K'),
        Command::Reclaim,
        Some("K reclaim"),
        Audience::Services,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('c'),
        Command::CopyUrl,
        Some("c copy"),
        Audience::Main,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('l'),
        Command::ReloadCatalog,
        Some("l reload"),
        Audience::Main,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('['),
        Command::CycleVersion(-1),
        Some("[ ] version"),
        Audience::Shared,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('X'),
        Command::RemoveShared,
        Some("X remove"),
        Audience::Shared,
        Gate::Focus(Pane::Shared),
    ),
    bind(
        KeyCode::Char('o'),
        Command::OpenFolder,
        Some("o reveal"),
        Audience::Workspace,
        Gate::Focus(Pane::Workspaces),
    ),
    bind(
        KeyCode::Backspace,
        Command::Forget,
        Some("bksp forget"),
        Audience::Workspace,
        Gate::Focus(Pane::Workspaces),
    ),
    bind(
        KeyCode::Char('D'),
        Command::StopDaemon,
        Some("D stop daemon"),
        Audience::Workspace,
        Gate::Focus(Pane::Workspaces),
    ),
    bind(
        KeyCode::Char('n'),
        Command::Add,
        Some("n add"),
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('r'),
        Command::Restart,
        Some("r restart daemon"),
        Audience::Workspace,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('r'),
        Command::Restart,
        Some("r restart"),
        Audience::Main,
        Gate::Always,
    ),
    bind(
        KeyCode::PageUp,
        Command::ScrollLog(1),
        Some("pgup/pgdn log"),
        Audience::Main,
        Gate::Always,
    ),
    bind(
        KeyCode::Tab,
        Command::FocusNext,
        Some("tab pane"),
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('S'),
        Command::ToggleShared,
        Some("S shared"),
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('m'),
        Command::ToggleMouse,
        Some("m mouse"),
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('?'),
        Command::ToggleHelp,
        Some("? help"),
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('q'),
        Command::Quit,
        Some("q quit"),
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Up,
        Command::Move(-1),
        None,
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Down,
        Command::Move(1),
        None,
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Char(' '),
        Command::Activate,
        None,
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('R'),
        Command::Restart,
        None,
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::PageDown,
        Command::ScrollLog(-1),
        None,
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Char(']'),
        Command::CycleVersion(1),
        None,
        Audience::Always,
        Gate::Always,
    ),
    bind(
        KeyCode::Char('u'),
        Command::Updates,
        None,
        Audience::Always,
        Gate::Always,
    ),
];

fn shows(audience: Audience, desk: &Desk) -> bool {
    match audience {
        Audience::Always => true,
        Audience::Workspace => desk.focus == Pane::Workspaces,
        Audience::Services => desk.focus == Pane::Services,
        Audience::Shared => desk.focus == Pane::Shared,
        Audience::Main => matches!(desk.focus, Pane::Services | Pane::Shared),
    }
}

fn gate_open(gate: Gate, desk: &Desk) -> bool {
    match gate {
        Gate::Always => true,
        Gate::Focus(pane) => desk.focus == pane,
    }
}

fn footer_line(desk: &Desk, width: usize) -> Line<'static> {
    let hints: Vec<&str> = if desk.composer.is_some() {
        COMPOSER_HINTS.to_vec()
    } else if desk.help {
        HELP_HINTS.to_vec()
    } else if desk.pending.is_some() {
        CONFIRM_HINTS.to_vec()
    } else {
        let mut hints = Vec::new();
        for binding in BINDINGS {
            let Some(hint) = binding.hint else {
                continue;
            };
            if !shows(binding.show, desk) {
                continue;
            }
            if hints.last() == Some(&hint) {
                continue;
            }
            if hint == "tab pane" && desk.shared_open {
                hints.push("esc close");
            }
            hints.push(hint);
        }
        hints
    };
    Line::styled(fit_hints(&hints, width), theme::footer())
}

fn fit_hints(hints: &[&str], width: usize) -> String {
    if width == 0 || hints.is_empty() {
        return String::new();
    }
    let mut kept = hints.to_vec();
    while kept.len() > 1 && visible_width(&kept.join("  ")) > width {
        kept.remove(kept.len() - 2);
    }
    let joined = kept.join("  ");
    if visible_width(&joined) <= width {
        joined
    } else {
        clip_width(&joined, width)
    }
}

fn clip_width(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > width {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out
}

mod theme {
    use ratatui::style::{Color, Modifier, Style};

    pub fn title() -> Style {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    }

    pub fn notice() -> Style {
        Style::default().fg(Color::Yellow)
    }

    pub fn dim() -> Style {
        Style::default().fg(Color::DarkGray)
    }

    pub fn focus() -> Style {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    }

    pub fn idle() -> Style {
        Style::default().fg(Color::DarkGray)
    }

    pub fn modal() -> Style {
        Style::default().fg(Color::Yellow)
    }

    pub fn footer() -> Style {
        Style::default().fg(Color::DarkGray)
    }
}

pub fn key_command(desk: &Desk, key: KeyEvent) -> Option<Command> {
    if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
        return None;
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Some(Command::Quit);
    }
    if desk.composer.is_some() {
        return composer_key(key);
    }
    if desk.help {
        return help_key(key);
    }
    if key.code == KeyCode::Esc {
        return if desk.pending.is_some() {
            Some(Command::ClearArm)
        } else if desk.shared_open {
            Some(Command::ToggleShared)
        } else {
            None
        };
    }
    BINDINGS
        .iter()
        .find(|binding| binding.code == key.code && gate_open(binding.gate, desk))
        .map(|binding| binding.command)
}

fn composer_key(key: KeyEvent) -> Option<Command> {
    match key.code {
        KeyCode::Esc => Some(Command::CancelComposer),
        KeyCode::Enter => Some(Command::SubmitComposer),
        KeyCode::Backspace => Some(Command::Backspace),
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) && !ch.is_control() => {
            Some(Command::Type(ch))
        }
        _ => None,
    }
}

fn help_key(key: KeyEvent) -> Option<Command> {
    match key.code {
        KeyCode::Char('q') => Some(Command::Quit),
        KeyCode::Esc | KeyCode::Char('?') => Some(Command::CloseHelp),
        _ => None,
    }
}

pub fn activation(desk: &Desk) -> Act {
    match desk.focus {
        Pane::Workspaces => {
            let Some(workspace) = desk.selected_workspace() else {
                return Act::Idle;
            };
            if workspace.missing {
                Act::Idle
            } else if !workspace.trusted {
                Act::Trust(workspace.id.clone())
            } else if !desk.attached {
                Act::StartDaemon(workspace.id.clone())
            } else {
                Act::FocusServices
            }
        }
        Pane::Services => match desk.selected_service() {
            Some(RowId::Daemon) | None => Act::Idle,
            Some(RowId::Group(name)) => {
                if group_is_up(&desk.sections, &name) {
                    Act::RestartGroup(name)
                } else {
                    Act::StartGroup(name)
                }
            }
            Some(RowId::Service(id)) => match desk.service_line(&id) {
                Some(service) if service.disabled => Act::Disabled,
                Some(service) if service.state == ActualServiceState::ExternallyOwned => {
                    Act::Reclaim(id)
                }
                Some(service) if service.state == ActualServiceState::Stopping => Act::Idle,
                Some(service)
                    if service.state == ActualServiceState::QueuedStart || is_up(service.state) =>
                {
                    Act::StopService(id)
                }
                Some(_) => Act::StartService(id),
                None => Act::Idle,
            },
            Some(RowId::Recipe(_) | RowId::Instance(_)) => Act::Idle,
        },
        Pane::Shared => match desk.selected_shared() {
            Some(RowId::Recipe(name)) => desk
                .recipes
                .iter()
                .find(|recipe| recipe.name == name)
                .map(|recipe| Act::Install(recipe.chosen()))
                .unwrap_or(Act::Idle),
            Some(RowId::Instance(id)) => {
                let up = desk
                    .instances
                    .iter()
                    .find(|instance| instance.id == id)
                    .is_some_and(|instance| {
                        matches!(compact_wire(&instance.actual_state), "ready" | "running")
                    });
                if up {
                    Act::StopInstance(id)
                } else {
                    Act::StartInstance(id)
                }
            }
            _ => Act::Idle,
        },
    }
}

/// Long-lived members are all ready or running, so the primary action is restart.
pub fn group_is_up(sections: &[Section], name: &str) -> bool {
    let long_lived: Vec<&ServiceLine> = sections
        .iter()
        .find(|section| section.name.as_deref() == Some(name))
        .map(|section| {
            section
                .services
                .iter()
                .filter(|service| !service.disabled && !service.finite)
                .collect()
        })
        .unwrap_or_default();
    !long_lived.is_empty() && long_lived.iter().all(|service| is_up(service.state))
}

/// Stop-all skips disabled rows and rows that are already stopped or finished.
pub fn stop_all_targets(sections: &[Section]) -> Vec<String> {
    sections
        .iter()
        .flat_map(|section| section.services.iter())
        .filter(|service| {
            !service.disabled
                && !matches!(
                    service.state,
                    ActualServiceState::Stopped | ActualServiceState::Succeeded
                )
        })
        .map(|service| service.id.clone())
        .collect()
}

/// A URL that requires the process is hidden until the row is ready or running. A finished
/// `readiness: exit` row (`succeeded`) shows its URLs too: a one-shot build/export has no process
/// left by design, and what it produced is what the link points at.
pub fn url_visible(requires_running: bool, state: ActualServiceState) -> bool {
    !requires_running || is_up(state) || state == ActualServiceState::Succeeded
}

/// The first observed catalog mtime is recorded. A later change reloads once.
pub fn should_reload_catalog(previous: Option<u64>, next: Option<u64>) -> bool {
    matches!((previous, next), (Some(before), Some(after)) if before != after)
}

pub fn group_targets(sections: &[Section], name: &str) -> Vec<String> {
    sections
        .iter()
        .find(|section| section.name.as_deref() == Some(name))
        .map(|section| {
            section
                .services
                .iter()
                .filter(|service| !service.disabled)
                .map(|service| service.id.clone())
                .collect()
        })
        .unwrap_or_default()
}

pub fn summary_of(sections: &[Section]) -> String {
    let mut ready = 0;
    let mut failed = 0;
    let mut total = 0;
    for service in sections.iter().flat_map(|section| section.services.iter()) {
        if service.finite && service.state != ActualServiceState::Failed {
            continue;
        }
        total += 1;
        if matches!(
            service.state,
            ActualServiceState::Ready
                | ActualServiceState::Running
                | ActualServiceState::RunningUnready
        ) {
            ready += 1;
        } else if service.state == ActualServiceState::Failed {
            failed += 1;
        }
    }
    if total == 0 {
        return String::new();
    }
    if failed > 0 {
        format!("{ready}/{total} ready  {failed} failed")
    } else {
        format!("{ready}/{total} ready")
    }
}

fn compact_wire(state: &str) -> &str {
    match state {
        "running-unready" => "running",
        "preparing" => "starting",
        "queued-start" => "queued",
        "externally-owned" => "external",
        other => other,
    }
}

/// Document order, then any live service the catalog does not list. Named groups take the first
/// declared membership. Anything left is an unlabeled section, which the shell shows as Other.
pub fn group_sections(
    metas: &[ServiceMeta],
    tree: &[CatalogGroup],
    live: &[Service],
) -> Vec<Section> {
    let mut order: Vec<ServiceMeta> = metas.to_vec();
    for service in live {
        if !order.iter().any(|meta| meta.id == service.name) {
            order.push(ServiceMeta {
                id: service.name.clone(),
                label: service.name.clone(),
                ports: String::new(),
                disabled: false,
                finite: false,
                infra: service.kind == Some(ServiceKind::Infrastructure),
                shared: false,
                shared_instance: None,
            });
        }
    }
    let line = |meta: &ServiceMeta| -> ServiceLine {
        let found = live.iter().find(|service| service.name == meta.id);
        let state = found
            .map(|service| service.state)
            .unwrap_or(ActualServiceState::Stopped);
        let error = found
            .and_then(|service| service.error.clone())
            .filter(|text| !text.is_empty())
            .or_else(|| {
                if state == ActualServiceState::Failed {
                    found
                        .and_then(|service| service.readiness_detail.clone())
                        .filter(|text| !text.is_empty())
                } else {
                    None
                }
            });
        ServiceLine {
            id: meta.id.clone(),
            label: if meta.label.is_empty() {
                meta.id.clone()
            } else {
                meta.label.clone()
            },
            ports: meta.ports.clone(),
            state,
            disabled: meta.disabled,
            finite: meta.finite,
            infra: meta.infra
                || found.and_then(|service| service.kind) == Some(ServiceKind::Infrastructure),
            shared: meta.shared,
            shared_instance: meta.shared_instance.clone(),
            error,
        }
    };
    if tree.is_empty() {
        return vec![Section {
            name: None,
            services: order.iter().map(line).collect(),
        }];
    }
    let mut first = HashMap::new();
    for group in tree {
        for member in &group.members {
            first
                .entry(member.clone())
                .or_insert_with(|| group.name.clone());
        }
    }
    let mut built: Vec<Section> = tree
        .iter()
        .map(|group| Section {
            name: Some(group.name.clone()),
            services: Vec::new(),
        })
        .collect();
    let mut rest = Vec::new();
    for meta in &order {
        let service = line(meta);
        if let Some(name) = first.get(&meta.id) {
            if let Some(section) = built
                .iter_mut()
                .find(|section| section.name.as_deref() == Some(name.as_str()))
            {
                section.services.push(service);
                continue;
            }
        }
        rest.push(service);
    }
    built.retain(|section| !section.services.is_empty());
    if !rest.is_empty() {
        built.push(Section {
            name: None,
            services: rest,
        });
    }
    if built.is_empty() {
        vec![Section {
            name: None,
            services: Vec::new(),
        }]
    } else {
        built
    }
}

pub fn metas_from_catalog(catalog: &ServiceCatalog) -> Vec<ServiceMeta> {
    catalog
        .services
        .iter()
        .map(|service| {
            let shared_instance = shared_instance_of(service);
            ServiceMeta {
                id: service.id.clone(),
                label: service.label.clone().unwrap_or_else(|| service.id.clone()),
                ports: service
                    .ports
                    .as_ref()
                    .map(|ports| {
                        ports
                            .iter()
                            .map(|port| port.port.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default(),
                disabled: service.disabled,
                finite: matches!(service.profiles.run.readiness(), ReadinessSpec::Exit),
                infra: service.kind == Some(ServiceKind::Infrastructure),
                shared: shared_instance.is_some(),
                shared_instance,
            }
        })
        .collect()
}

pub fn start_all_targets(catalog: &ServiceCatalog, sections: &[Section]) -> Vec<String> {
    catalog
        .groups
        .get("all")
        .cloned()
        .filter(|ids| !ids.is_empty())
        .unwrap_or_else(|| {
            sections
                .iter()
                .flat_map(|section| section.services.iter())
                .filter(|service| !service.disabled)
                .map(|service| service.id.clone())
                .collect()
        })
}

fn is_up(state: ActualServiceState) -> bool {
    matches!(
        state,
        ActualServiceState::Ready
            | ActualServiceState::Running
            | ActualServiceState::RunningUnready
    )
}

fn service_ids(sections: &[Section], have_workspace: bool) -> Vec<RowId> {
    let mut rows = Vec::new();
    if have_workspace {
        rows.push(RowId::Daemon);
    }
    for section in sections {
        if let Some(name) = &section.name {
            rows.push(RowId::Group(name.clone()));
        }
        for service in &section.services {
            rows.push(RowId::Service(service.id.clone()));
        }
    }
    rows
}

fn arm_notice(pending: &Pending, desk: &Desk) -> String {
    let workspace = |id: &str| -> String {
        desk.workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .map(|workspace| workspace.name.clone())
            .unwrap_or_else(|| id.to_string())
    };
    match pending {
        Pending::Trust(id) => {
            let name = workspace(id);
            let path = desk
                .workspaces
                .iter()
                .find(|row| row.id == *id)
                .map(|row| row.path.as_str())
                .unwrap_or("");
            if path.is_empty() {
                format!("press again to trust {name} and start its daemon")
            } else {
                format!("press again to trust {name} ({path}) and start its daemon")
            }
        }
        Pending::Forget(id) => format!(
            "press again to forget {} — its services keep running",
            workspace(id)
        ),
        Pending::StopDaemon(id) => format!(
            "press again to stop the daemon for {} — its services stop",
            workspace(id)
        ),
        Pending::RestartDaemon(id) => format!(
            "press again to restart the daemon for {} — services keep running",
            workspace(id)
        ),
        Pending::Reclaim(id) => {
            let holder = desk
                .service_line(id)
                .and_then(|service| service.error.clone())
                .unwrap_or_else(|| format!("{id} is externally owned"));
            format!("{holder} — press again to kill it and start {id}")
        }
        Pending::RemoveShared {
            id,
            affected,
            unchecked,
        } => {
            if *unchecked {
                unchecked_shared_notice("remove", id)
            } else if affected.is_empty() {
                format!("press again to remove {id}")
            } else {
                instance_shared_notice("remove", id, affected)
            }
        }
        Pending::SharedImpact { notice, .. } => notice.clone(),
    }
}

fn scroll_window(
    offset: &mut usize,
    index: &mut usize,
    len: usize,
    height: usize,
    delta: i64,
) -> bool {
    if height == 0 || len <= height {
        return false;
    }
    let max = len - height;
    let next = (*offset as i64 + delta).clamp(0, max as i64) as usize;
    if next == *offset {
        return false;
    }
    *offset = next;
    if *index < *offset {
        *index = *offset;
    } else if *index >= *offset + height {
        *index = *offset + height - 1;
    }
    true
}

fn slide(index: &mut usize, len: usize, delta: i64) -> bool {
    if len == 0 {
        return false;
    }
    let next = *index as i64 + delta;
    if next < 0 || next >= len as i64 {
        return false;
    }
    *index = next as usize;
    true
}

fn split_at(columns: usize) -> Option<usize> {
    if columns < 96 {
        return None;
    }
    let sidebar = (columns / 4).clamp(24, 36);
    if columns - sidebar < 48 {
        None
    } else {
        Some(sidebar)
    }
}

fn reveal(offset: &mut usize, cursor: usize, height: usize, len: usize) {
    if height == 0 || len == 0 {
        *offset = 0;
        return;
    }
    if cursor < *offset {
        *offset = cursor;
    } else if cursor >= *offset + height {
        *offset = cursor + 1 - height;
    }
    *offset = (*offset).min(len.saturating_sub(height));
}

fn visual_index_of_item(visuals: &[Painted], item: usize) -> Option<usize> {
    visuals
        .iter()
        .position(|row| row.kind == Visual::Item && row.selectable == item)
}

fn log_window<'a>(log: &'a str, height: usize, scroll: &mut usize) -> Vec<&'a str> {
    if height == 0 {
        return Vec::new();
    }
    let all: Vec<&str> = if log.is_empty() {
        vec![" "]
    } else {
        log.split('\n').collect()
    };
    let max_scroll = all.len().saturating_sub(height);
    *scroll = (*scroll).min(max_scroll);
    let end = all.len().saturating_sub(*scroll);
    let start = end.saturating_sub(height);
    all[start..end].to_vec()
}

const HELP: &[&str] = &[
    "Workspaces   j/k move · enter trusts (twice) or starts the daemon · tab moves to services",
    "              o reveals the folder in Finder · backspace forget (twice; services keep running)",
    "              D stop daemon (twice) · r restart daemon (twice; services stay up)",
    "Services     enter starts, stops a running row, or cancels a queued start · x stop · r restart",
    "              a start all · s stops rows that are not already stopped or finished",
    "              enter on a group starts it, or restarts it when every long-lived member is up",
    "              K on an external row kills the port holder and starts (twice)",
    "              c copies a visible URL, or the log · l reloads · editing hearth.yaml reloads too",
    "              x still stops while a start is running · the first row is the daemon log",
    "Shared       S opens it · [ ] change version · enter installs, or starts and stops an instance",
    "              the word shared is blue · x stops · r restarts · X removes · another workspace is named first",
    "Log          page up and page down · u releases page · hearth update · esc cancels · q quits",
    "Mouse        clicks and the wheel act on rows · drag a URL to copy it · m frees the mouse",
    "A folder stays untrusted until the second enter, and that does not spawn a daemon before then.",
    "Stop daemon stays stopped until you press enter on that workspace again.",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    fn paint(desk: &mut Desk, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| desk.draw(frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut lines = Vec::new();
        for y in 0..buffer.area.height {
            let mut line = String::new();
            for x in 0..buffer.area.width {
                line.push_str(buffer[(x, y)].symbol());
            }
            lines.push(line);
        }
        lines.join("\n")
    }

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn live(name: &str, state: &str) -> Service {
        Service {
            name: name.to_string(),
            kind: None,
            state: serde_json::from_value(serde_json::json!(state)).unwrap(),
            generation: None,
            current_operation_id: None,
            error: None,
            readiness_detail: None,
        }
    }

    fn meta(id: &str, disabled: bool, finite: bool) -> ServiceMeta {
        ServiceMeta {
            id: id.to_string(),
            label: id.to_string(),
            ports: String::new(),
            disabled,
            finite,
            infra: id == "postgres",
            shared: false,
            shared_instance: None,
        }
    }

    fn row(id: &str, state: ActualServiceState, finite: bool) -> ServiceLine {
        ServiceLine {
            id: id.into(),
            label: id.into(),
            ports: String::new(),
            state,
            disabled: false,
            finite,
            infra: false,
            shared: false,
            shared_instance: None,
            error: None,
        }
    }

    fn workspace(id: &str, name: &str, trusted: bool) -> WorkspaceLine {
        WorkspaceLine {
            id: id.into(),
            name: name.into(),
            path: String::new(),
            trusted,
            missing: false,
        }
    }

    #[test]
    fn groups_by_first_declared_membership_and_keeps_the_rest() {
        let tree = vec![
            CatalogGroup {
                name: "infra".to_string(),
                members: vec!["postgres".to_string(), "not-a-service".to_string()],
            },
            CatalogGroup {
                name: "app".to_string(),
                members: vec!["api".to_string(), "postgres".to_string()],
            },
        ];
        let sections = group_sections(
            &[
                meta("api", false, false),
                meta("postgres", false, false),
                meta("worker", true, true),
            ],
            &tree,
            &[live("api", "ready"), live("postgres", "stopped")],
        );
        assert_eq!(sections[0].name.as_deref(), Some("infra"));
        assert_eq!(sections[0].services[0].id, "postgres");
        assert_eq!(sections[1].name.as_deref(), Some("app"));
        assert_eq!(sections[1].services[0].id, "api");
        assert_eq!(sections[1].services[0].state, ActualServiceState::Ready);
        assert!(sections[2].name.is_none());
        assert!(sections[2].services[0].disabled);
        assert!(sections[2].services[0].finite);
    }

    #[test]
    fn summary_skips_finished_jobs_and_counts_ready_and_running() {
        let sections = vec![Section {
            name: None,
            services: vec![
                row("api", ActualServiceState::Ready, false),
                row("web", ActualServiceState::RunningUnready, false),
                row("job", ActualServiceState::Succeeded, true),
                row("bad", ActualServiceState::Failed, true),
            ],
        }];
        assert_eq!(summary_of(&sections), "2/3 ready  1 failed");
    }

    #[test]
    fn enter_trusts_an_untrusted_folder_and_does_not_spawn_before_that() {
        let mut desk = Desk::default();
        desk.workspaces.push(workspace("ID", "viclass", false));
        assert_eq!(activation(&desk), Act::Trust("ID".into()));
        assert!(!desk.arm(Pending::Trust("ID".into())));
        assert!(desk
            .header_notice()
            .contains("press again to trust viclass"));
        assert!(desk.arm(Pending::Trust("ID".into())));
    }

    #[test]
    fn enter_on_a_live_workspace_moves_to_services_and_external_rows_reclaim() {
        let mut desk = Desk::default();
        desk.workspaces.push(workspace("ID", "viclass", true));
        desk.attached = true;
        assert_eq!(activation(&desk), Act::FocusServices);
        desk.focus = Pane::Services;
        desk.sections = group_sections(
            &[meta("api", false, false)],
            &[],
            &[live("api", "externally-owned")],
        );
        desk.service_cursor = 1;
        assert_eq!(activation(&desk), Act::Reclaim("api".into()));
        desk.sections[0].services[0].disabled = true;
        desk.sections[0].services[0].state = ActualServiceState::Stopped;
        assert_eq!(activation(&desk), Act::Disabled);
    }

    #[test]
    fn a_narrow_frame_shows_only_the_focused_pane_and_stays_inside_the_terminal() {
        let mut desk = Desk {
            workspaces: vec![
                workspace("1", "viclass", true),
                workspace("2", "other", false),
            ],
            sections: group_sections(
                &[meta("postgres", false, false)],
                &[],
                &[live("postgres", "ready")],
            ),
            summary: summary_of(&group_sections(
                &[meta("postgres", false, false)],
                &[],
                &[live("postgres", "ready")],
            )),
            log: "line-1\nline-2".into(),
            log_title: "postgres".into(),
            ..Desk::default()
        };
        let buffer = paint(&mut desk, 70, 16);
        assert_eq!((buffer.area.width, buffer.area.height), (70, 16));
        let joined = buffer_text(&buffer);
        assert!(joined.contains("viclass"), "{joined}");
        assert!(!joined.contains("postgres"), "{joined}");
        assert!(
            joined.contains("move") && joined.contains("quit"),
            "{joined}"
        );
        desk.focus = Pane::Services;
        let joined = buffer_text(&paint(&mut desk, 70, 20));
        assert!(joined.contains("postgres"), "{joined}");
        assert!(joined.contains("daemon log"), "{joined}");
        assert!(joined.contains("LOG — postgres"), "{joined}");
        assert!(!joined.contains("other"), "{joined}");
    }

    #[test]
    fn a_wide_frame_shows_workspaces_beside_grouped_services() {
        let mut desk = Desk {
            focus: Pane::Services,
            workspaces: vec![workspace("1", "viclass", true)],
            ..Desk::default()
        };
        let tree = vec![CatalogGroup {
            name: "infra".into(),
            members: vec!["postgres".into()],
        }];
        let sections = group_sections(
            &[meta("postgres", false, false), meta("api", false, false)],
            &tree,
            &[live("postgres", "ready")],
        );
        desk.summary = summary_of(&sections);
        desk.set_sections(sections, vec!["postgres".into(), "api".into()]);
        desk.service_cursor = 2;
        let buffer = paint(&mut desk, 120, 24);
        assert_eq!((buffer.area.width, buffer.area.height), (120, 24));
        let joined = buffer_text(&buffer);
        assert!(joined.contains("viclass"), "{joined}");
        assert!(joined.contains("INFRA"), "{joined}");
        assert!(joined.contains("postgres"), "{joined}");
        assert!(joined.contains("api"), "{joined}");
        assert!(joined.contains('>'), "{joined}");
        assert!(joined.contains("stop all"), "{joined}");
        assert!(joined.contains("quit"), "{joined}");
        assert!(buffer.content.iter().any(|cell| cell.symbol() == ">"
            && cell.bg == Color::Indexed(238)
            && cell.modifier.contains(Modifier::BOLD)));
        assert!(buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "┌" && cell.fg == Color::Cyan));
        desk.focus = Pane::Workspaces;
        let workspace = buffer_text(&paint(&mut desk, 120, 24));
        assert!(
            workspace.contains("forget") && workspace.contains("quit"),
            "{workspace}"
        );
    }

    #[test]
    fn reclaim_confirm_keeps_its_suffix_on_a_narrow_notice() {
        let mut desk = Desk {
            pending: Some(Pending::Reclaim("api".into())),
            sections: vec![Section {
                name: None,
                services: vec![ServiceLine {
                    id: "api".into(),
                    label: "api".into(),
                    ports: String::new(),
                    state: ActualServiceState::ExternallyOwned,
                    disabled: false,
                    finite: false,
                    infra: false,
                    shared: false,
                    shared_instance: None,
                    error: Some("x".repeat(80)),
                }],
            }],
            ..Desk::default()
        };
        let text = buffer_text(&paint(&mut desk, 40, 16));
        assert!(text.contains("press again"), "{text}");
        assert!(
            text.contains("confirm") && text.contains("cancel"),
            "{text}"
        );
    }

    #[test]
    fn shared_enter_installs_the_chosen_version_and_stops_a_running_instance() {
        let mut desk = Desk {
            focus: Pane::Shared,
            shared_open: true,
            ..Desk::default()
        };
        desk.recipes.push(RecipeLine {
            name: "redis".into(),
            versions: vec!["8.2.10".into(), "7.2".into()],
            version_index: 0,
        });
        desk.instances.push(InstanceLine {
            id: "redis@8.2.10".into(),
            install_state: "installed".into(),
            actual_state: "ready".into(),
            port: 43110,
            attachments: 2,
            install_error: None,
        });
        assert_eq!(activation(&desk), Act::Install("redis@8.2.10".into()));
        assert!(desk.cycle_version(1));
        assert_eq!(activation(&desk), Act::Install("redis@7.2".into()));
        desk.shared_cursor = 1;
        assert_eq!(activation(&desk), Act::StopInstance("redis@8.2.10".into()));
        desk.instances[0].actual_state = "running-unready".into();
        assert_eq!(activation(&desk), Act::StopInstance("redis@8.2.10".into()));
        desk.instances[0].actual_state = "preparing".into();
        desk.instances[0].install_error = Some("sha256 mismatch".into());
        assert_eq!(activation(&desk), Act::StartInstance("redis@8.2.10".into()));
        let joined = buffer_text(&paint(&mut desk, 100, 20));
        assert!(joined.contains("starting"), "{joined}");
        assert!(joined.contains("sha256 mismatch"), "{joined}");
        assert_eq!(key_command(&desk, key('X')), Some(Command::RemoveShared));
        assert!(!desk.arm(Pending::RemoveShared {
            id: "redis@8.2.10".into(),
            affected: vec!["infra".into(), "viclass".into()],
            unchecked: false,
        }));
        let notice = desk.header_notice();
        assert!(
            notice.contains("infra") && notice.contains("viclass"),
            "{notice}"
        );
        assert!(notice.contains("Press again"), "{notice}");
    }

    #[test]
    fn question_mark_opens_help_and_other_keys_do_not_act() {
        let mut desk = Desk::default();
        assert_eq!(key_command(&desk, key('?')), Some(Command::ToggleHelp));
        desk.help = true;
        assert_eq!(key_command(&desk, key('a')), None);
        assert_eq!(
            key_command(&desk, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Command::CloseHelp)
        );
        assert!(buffer_text(&paint(&mut desk, 80, 16)).contains("untrusted"));
        assert_eq!(
            key_command(&Desk::default(), key('u')),
            Some(Command::Updates)
        );
        let composing = Desk {
            composer: Some(String::new()),
            ..Desk::default()
        };
        assert_eq!(key_command(&composing, key('u')), Some(Command::Type('u')));
    }

    #[test]
    fn enter_cancels_a_queued_start_and_restarts_a_group_that_is_already_up() {
        let mut desk = Desk {
            focus: Pane::Services,
            ..Desk::default()
        };
        desk.sections = vec![Section {
            name: Some("app".into()),
            services: vec![row("api", ActualServiceState::QueuedStart, false)],
        }];
        desk.service_cursor = 1;
        assert_eq!(activation(&desk), Act::StopService("api".into()));
        desk.sections[0].services[0].state = ActualServiceState::Stopping;
        assert_eq!(activation(&desk), Act::Idle);
        desk.sections[0].services[0].state = ActualServiceState::Ready;
        desk.service_cursor = 0;
        assert_eq!(activation(&desk), Act::RestartGroup("app".into()));
        desk.sections[0].services[0].state = ActualServiceState::Stopped;
        assert_eq!(activation(&desk), Act::StartGroup("app".into()));
        desk.sections[0].services[0].finite = true;
        desk.sections[0].services[0].state = ActualServiceState::Ready;
        assert_eq!(activation(&desk), Act::StartGroup("app".into()));
    }

    #[test]
    fn stop_all_skips_stopped_and_finished_rows() {
        let mut sections = vec![Section {
            name: None,
            services: vec![
                row("down", ActualServiceState::Stopped, false),
                row("done", ActualServiceState::Succeeded, true),
                row("bad", ActualServiceState::Failed, false),
                row("wait", ActualServiceState::QueuedStart, false),
                row("off", ActualServiceState::Running, false),
            ],
        }];
        sections[0].services[4].disabled = true;
        assert_eq!(
            stop_all_targets(&sections),
            vec!["bad".to_string(), "wait".to_string()]
        );
    }

    #[test]
    fn urls_hide_until_the_service_is_up_and_a_catalog_change_reloads_once() {
        assert!(!url_visible(true, ActualServiceState::Starting));
        assert!(!url_visible(true, ActualServiceState::Stopped));
        assert!(!url_visible(true, ActualServiceState::Failed));
        assert!(url_visible(true, ActualServiceState::Ready));
        assert!(url_visible(true, ActualServiceState::RunningUnready));
        assert!(url_visible(true, ActualServiceState::Succeeded));
        assert!(url_visible(false, ActualServiceState::Stopped));
        assert!(!should_reload_catalog(None, Some(1)));
        assert!(!should_reload_catalog(Some(1), Some(1)));
        assert!(!should_reload_catalog(Some(1), None));
        assert!(should_reload_catalog(Some(1), Some(2)));
    }

    #[test]
    fn rows_show_the_label_ports_path_and_failure_detail() {
        let failed = Service {
            error: None,
            readiness_detail: Some("timed out".into()),
            ..live("api", "failed")
        };
        let sections = group_sections(&[meta("api", false, false)], &[], &[failed]);
        assert_eq!(sections[0].services[0].error.as_deref(), Some("timed out"));
        let line = ServiceLine {
            id: "api".into(),
            label: "API".into(),
            ports: "8080".into(),
            state: ActualServiceState::Failed,
            disabled: false,
            finite: false,
            infra: false,
            shared: false,
            shared_instance: None,
            error: Some("timed out".into()),
        };
        let text = line_text(&service_line(&line, true));
        assert!(
            text.contains("API")
                && text.contains("api")
                && text.contains("8080")
                && text.contains("timed out"),
            "{text}"
        );
        let workspace_row = WorkspaceLine {
            id: "1".into(),
            name: "viclass".into(),
            path: "~/src/viclass".into(),
            trusted: true,
            missing: false,
        };
        let shown = line_text(&workspace_line(&workspace_row, true, "1/2 ready"));
        assert!(
            shown.contains("viclass")
                && shown.contains("~/src/viclass")
                && shown.contains("1/2 ready"),
            "{shown}"
        );
    }

    #[test]
    fn service_names_are_bold_and_disabled_rows_dim_but_keep_state_colour() {
        let enabled = service_line(&row("api", ActualServiceState::Ready, false), false);
        let name = enabled
            .spans
            .iter()
            .find(|span| span.content.contains("api"))
            .unwrap();
        assert_eq!(name.style.fg, Some(Color::White));
        assert!(name.style.add_modifier.contains(Modifier::BOLD));

        let mut disabled_row = row("worker", ActualServiceState::Ready, false);
        disabled_row.disabled = true;
        let line = service_line(&disabled_row, false);
        let name = line
            .spans
            .iter()
            .find(|span| span.content.contains("worker"))
            .unwrap();
        assert_eq!(name.style.fg, Some(Color::DarkGray));
        let flags = line
            .spans
            .iter()
            .find(|span| span.content.contains("disabled"))
            .unwrap();
        assert_eq!(flags.style.fg, Some(Color::DarkGray));
        let state = line
            .spans
            .iter()
            .find(|span| span.content.contains("ready"))
            .unwrap();
        assert_eq!(state.style.fg, Some(Color::Green));
        assert_eq!(state.style.bg, None);
    }

    #[test]
    fn a_shared_catalog_row_remembers_its_instance() {
        use hearth_core::catalog::{ServiceCommand, ServiceId, ServiceOwnership, ServiceProfiles};
        let service = ServiceDefinition {
            id: ServiceId::from("postgres"),
            label: Some("postgres@16.4 (shared)".into()),
            kind: Some(ServiceKind::Infrastructure),
            ownership: Some(ServiceOwnership::External),
            disabled: false,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: ServiceCommand {
                        command: CommandSpec::Argv {
                            argv: vec![
                                "hearth".into(),
                                "shared".into(),
                                "attach".into(),
                                "postgres@16.4".into(),
                            ],
                        },
                        cwd: ".".into(),
                        environment: None,
                        container_name: None,
                        docker_stop_command: None,
                    },
                    readiness_timeout_ms: None,
                    readiness: ReadinessSpec::Process,
                    preparation: None,
                    preparation_command: None,
                },
                build: None,
            },
            ports: None,
            urls: None,
            artifact: None,
        };
        assert_eq!(
            shared_instance_of(&service).as_deref(),
            Some("postgres@16.4")
        );
        let metas = metas_from_catalog(&ServiceCatalog {
            services: vec![service],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: Default::default(),
            private_file_guard: None,
        });
        assert!(metas[0].shared);
        assert_eq!(metas[0].shared_instance.as_deref(), Some("postgres@16.4"));
    }

    #[test]
    fn the_word_shared_is_blue_and_the_state_keeps_its_colour() {
        let mut shared = row("postgres", ActualServiceState::Ready, false);
        shared.shared = true;
        shared.label = "postgres@16.4 (shared)".into();
        shared.shared_instance = Some("postgres@16.4".into());
        let line = service_line(&shared, false);
        let state = line
            .spans
            .iter()
            .find(|span| span.content.contains("ready"))
            .unwrap();
        assert_eq!(state.style.fg, Some(Color::Green));
        assert_eq!(state.style.bg, None);
        let word = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "shared")
            .unwrap();
        assert_eq!(word.style.fg, Some(Color::Blue));
        assert_eq!(word.style.bg, None);
        assert!(word.style.add_modifier.contains(Modifier::BOLD));
        assert!(
            line_text(&line).contains("(shared)"),
            "{}",
            line_text(&line)
        );

        let mut stopped = row("postgres", ActualServiceState::Stopped, false);
        stopped.shared = true;
        stopped.label = "postgres@16.4 (shared)".into();
        let stopped_line = service_line(&stopped, false);
        let state = stopped_line
            .spans
            .iter()
            .find(|span| span.content.contains("stopped"))
            .unwrap();
        assert_eq!(state.style.fg, Some(Color::DarkGray));
        assert_eq!(state.style.bg, None);

        let mut failed = row("postgres", ActualServiceState::Failed, false);
        failed.shared = true;
        failed.label = "postgres@16.4 (shared)".into();
        let failed_line = service_line(&failed, false);
        let state = failed_line
            .spans
            .iter()
            .find(|span| span.content.contains("failed"))
            .unwrap();
        assert_eq!(state.style.fg, Some(Color::Red));
        assert_eq!(state.style.bg, None);
        assert!(failed_line.spans.iter().any(|span| {
            span.content.as_ref() == "shared"
                && span.style.fg == Some(Color::Blue)
                && span.style.bg.is_none()
        }));

        let mut bare = row("postgres", ActualServiceState::Ready, false);
        bare.shared = true;
        let bare_line = service_line(&bare, false);
        assert!(bare_line
            .spans
            .iter()
            .any(|span| { span.content.contains("shared") && span.style.fg == Some(Color::Blue) }));
        let state = bare_line
            .spans
            .iter()
            .find(|span| span.content.contains("ready"))
            .unwrap();
        assert_eq!(state.style.bg, None);

        let mut ordinary = row("api", ActualServiceState::Ready, false);
        ordinary.label = "api (shared)".into();
        let ordinary_line = service_line(&ordinary, false);
        assert!(ordinary_line.spans.iter().all(|span| {
            !(span.content.as_ref() == "shared" && span.style.fg == Some(Color::Blue))
        }));

        let instance = InstanceLine {
            id: "redis@8.2.10".into(),
            install_state: "installed".into(),
            actual_state: "running".into(),
            port: 1,
            attachments: 0,
            install_error: None,
        };
        let painted = instance_line(&instance, false, "", "");
        let state = painted
            .spans
            .iter()
            .find(|span| span.content.contains("running"))
            .unwrap();
        assert_eq!(state.style.fg, Some(Color::Cyan));
        assert_eq!(state.style.bg, None);
    }

    #[test]
    fn another_workspace_is_named_before_a_shared_action() {
        let known = vec![
            KnownRoot {
                root: "/work/viclass".into(),
                name: "viclass".into(),
                path: "~/work/viclass".into(),
            },
            KnownRoot {
                root: "/work/infra".into(),
                name: "infra".into(),
                path: "~/work/infra".into(),
            },
            KnownRoot {
                root: "/work/shop".into(),
                name: "shop".into(),
                path: "~/work/shop".into(),
            },
        ];
        let roots = vec![
            "/work/viclass".into(),
            "/work/infra".into(),
            "/work/shop".into(),
        ];
        let report = classify_attachments(&roots, Some("/work/viclass"), &known);
        assert_eq!(report.others, vec!["infra".to_string(), "shop".to_string()]);
        assert_eq!(
            report.all,
            vec![
                "infra".to_string(),
                "shop".to_string(),
                "viclass".to_string()
            ]
        );
        let only_here =
            classify_attachments(&["/work/viclass".into()], Some("/work/viclass"), &known);
        assert!(only_here.others.is_empty());

        let down = instance_shared_notice("stop", "redis@8.2.10", &report.all);
        assert!(
            down.contains("viclass") && down.contains("infra") && down.contains("shop"),
            "{down}"
        );
        assert!(down.contains("takes the shared service down"), "{down}");

        let detach = project_shared_notice(
            "stop",
            "viclass",
            &[SharedTouch {
                instance: "postgres@16.4".into(),
                others: report.others.clone(),
            }],
            &[],
        );
        assert!(detach.contains("postgres@16.4"), "{detach}");
        assert!(detach.contains("infra and shop"), "{detach}");
        assert!(detach.contains("only detaches viclass"), "{detach}");
        assert!(detach.contains("keep it"), "{detach}");

        let twins = vec![
            KnownRoot {
                root: "/a/web".into(),
                name: "web".into(),
                path: "~/a/web".into(),
            },
            KnownRoot {
                root: "/b/web".into(),
                name: "web".into(),
                path: "~/b/web".into(),
            },
        ];
        let named = classify_attachments(&["/a/web".into(), "/b/web".into()], None, &twins);
        assert!(
            named.all.iter().any(|label| label.contains("~/a/web")),
            "{named:?}"
        );
        assert!(
            named.all.iter().any(|label| label.contains("~/b/web")),
            "{named:?}"
        );
    }

    #[test]
    fn painted_urls_become_links_at_their_cell_positions() {
        let mut links = Vec::new();
        let line = Line::styled("URL http://localhost:3000/path.", theme::dim());
        collect_links(&line, 2, 5, 60, true, &mut links);
        assert_eq!(links.len(), 1);
        assert_eq!((links[0].x, links[0].y), (6, 5));
        assert_eq!(links[0].text, "http://localhost:3000/path");
        assert_eq!(links[0].target, "http://localhost:3000/path");
        assert_eq!((links[0].row_x, links[0].row_width), (2, 60));

        // A clipped tail still links the visible prefix to the full target.
        links.clear();
        collect_links(&line, 2, 5, 10, true, &mut links);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].text, "http:/");
        assert_eq!(links[0].target, "http://localhost:3000/path");

        // Log rows find URLs inside styled text too.
        links.clear();
        let log = sgr_to_line("ready on https://a.test:1/x  next");
        collect_links(&log, 0, 0, 60, false, &mut links);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].x, 9);
        assert_eq!(links[0].target, "https://a.test:1/x");
    }

    #[test]
    fn a_press_on_the_url_row_selects_that_link_and_a_drag_copies_the_span() {
        let link = Link {
            x: 6,
            y: 5,
            text: "http://localhost:3000/path".into(),
            target: "http://localhost:3000/path".into(),
            row_x: 2,
            row_width: 60,
            row_press: true,
        };
        let mut desk = Desk {
            links: vec![link.clone()],
            ..Desk::default()
        };
        assert_eq!(
            desk.link_for_press(2, 5)
                .as_ref()
                .map(|hit| hit.target.as_str()),
            Some(link.target.as_str())
        );
        assert_eq!(desk.link_for_press(6, 5).as_ref().map(|hit| hit.x), Some(6));
        assert_eq!(desk.link_for_press(0, 5), None);

        desk.select_link(&link, 6, 6, false);
        assert_eq!(
            desk.link_selection
                .as_ref()
                .map(|selection| selection.copied()),
            Some(link.target.clone())
        );
        // One column of movement is a click, not a partial selection.
        desk.select_link(&link, 6, 7, true);
        assert_eq!(
            desk.link_selection
                .as_ref()
                .map(|selection| selection.copied()),
            Some(link.target.clone())
        );
        desk.select_link(&link, 11, 20, true);
        let partial = desk.link_selection.as_ref().unwrap().copied();
        assert_eq!(partial, "//localhos");
        assert_ne!(partial, link.target);

        let clipped = Link {
            text: "http:/".into(),
            ..link
        };
        desk.select_link(&clipped, clipped.x, clipped.x, false);
        assert_eq!(
            desk.link_selection
                .as_ref()
                .map(|selection| selection.copied())
                .as_deref(),
            Some("http://localhost:3000/path")
        );

        // A frame that no longer paints the link drops the highlight.
        desk.links.clear();
        let _ = paint(&mut desk, 40, 8);
        assert!(desk.link_selection.is_none());
    }

    #[test]
    fn painted_service_urls_are_underlined_and_a_selection_is_reversed() {
        let mut desk = Desk {
            focus: Pane::Services,
            workspaces: vec![workspace("1", "viclass", true)],
            urls: vec!["app  http://127.0.0.1:3000/health".into()],
            ..Desk::default()
        };
        desk.sections = group_sections(&[meta("api", false, false)], &[], &[live("api", "ready")]);
        let _ = paint(&mut desk, 120, 24);
        let link = desk
            .links
            .iter()
            .find(|link| link.target.contains("3000"))
            .expect("url row")
            .clone();
        assert!(desk
            .link_for_press(link.row_x as usize, link.y as usize)
            .is_some());
        desk.select_link(&link, link.x, link.x, false);
        let buffer = paint(&mut desk, 120, 24);
        let cell = buffer.cell((link.x, link.y)).expect("url cell");
        assert!(
            cell.modifier.contains(Modifier::UNDERLINED),
            "{:?}",
            cell.modifier
        );
        assert!(
            cell.modifier.contains(Modifier::REVERSED),
            "{:?}",
            cell.modifier
        );
        let overlay = link_overlay(&link, desk.link_selection.as_ref(), Color::DarkGray);
        assert!(
            overlay.contains("\x1b]8;;http://127.0.0.1:3000/health\x1b\\"),
            "{overlay}"
        );
        assert!(overlay.contains("\x1b[7m"), "{overlay}");
    }

    #[test]
    fn command_table_keeps_focus_gates_and_aliases() {
        let desk = Desk::default();
        assert_eq!(
            key_command(&desk, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Some(Command::Move(-1))
        );
        assert_eq!(
            key_command(&desk, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Some(Command::Move(1))
        );
        assert_eq!(
            key_command(&desk, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            Some(Command::ScrollLog(1))
        );
        assert_eq!(
            key_command(&desk, KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
            Some(Command::ScrollLog(-1))
        );
        assert_eq!(key_command(&desk, key(' ')), Some(Command::Activate));
        assert_eq!(key_command(&desk, key('R')), Some(Command::Restart));
        assert_eq!(key_command(&desk, key(']')), Some(Command::CycleVersion(1)));
        assert_eq!(key_command(&desk, key('D')), Some(Command::StopDaemon));
        assert_eq!(key_command(&desk, key('X')), None);
        let services = Desk {
            focus: Pane::Services,
            ..Desk::default()
        };
        assert_eq!(key_command(&services, key('D')), None);
        assert_eq!(key_command(&services, key('a')), Some(Command::StartAll));
        let shared = Desk {
            focus: Pane::Shared,
            shared_open: true,
            ..Desk::default()
        };
        assert_eq!(
            key_command(&shared, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Command::ToggleShared)
        );
        let armed = Desk {
            pending: Some(Pending::Forget("1".into())),
            ..Desk::default()
        };
        assert_eq!(
            key_command(&armed, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Command::ClearArm)
        );
        let composing = Desk {
            composer: Some(String::new()),
            ..Desk::default()
        };
        assert_eq!(
            key_command(
                &composing,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Some(Command::Quit)
        );
    }

    #[test]
    fn log_keeps_follow_tail_until_scrolled_and_parses_sgr_color() {
        let mut lines = Vec::new();
        for index in 0..40 {
            lines.push(format!("LOGROW{index:02}"));
        }
        let mut desk = Desk {
            focus: Pane::Services,
            workspaces: vec![workspace("1", "viclass", true)],
            log: lines.join("\n"),
            log_title: "postgres".into(),
            ..Desk::default()
        };
        let tail = buffer_text(&paint(&mut desk, 80, 24));
        assert!(tail.contains("LOGROW39"), "{tail}");
        assert!(!tail.contains("LOGROW00"), "{tail}");
        desk.scroll_log(10_000);
        let head = buffer_text(&paint(&mut desk, 80, 24));
        assert!(head.contains("LOGROW00"), "{head}");
        desk.log = "\x1b[32mLOGGREEN\x1b[0m".into();
        desk.log_scroll = 0;
        let buffer = paint(&mut desk, 80, 24);
        assert!(buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "L" && cell.fg == Color::Green));
    }

    #[test]
    fn hit_uses_the_cells_that_were_painted() {
        let mut desk = Desk {
            focus: Pane::Services,
            workspaces: vec![workspace("1", "viclass", true)],
            ..Desk::default()
        };
        desk.sections = group_sections(
            &[meta("postgres", false, false)],
            &[],
            &[live("postgres", "ready")],
        );
        let _ = paint(&mut desk, 120, 24);
        let workspace_at = desk.geometry.workspace_items;
        assert_eq!(
            desk.hit(workspace_at.x as usize, workspace_at.y as usize),
            Some(Hit::Workspace(0))
        );
        let service_at = desk.geometry.main_items;
        assert_eq!(
            desk.hit(service_at.x as usize, service_at.y as usize),
            Some(Hit::Service(0))
        );
    }

    #[test]
    fn a_notice_paints_at_the_top_of_the_service_pane() {
        let mut desk = Desk {
            focus: Pane::Services,
            workspaces: vec![workspace("1", "viclass", true)],
            notice: "Manager unavailable".to_string(),
            ..Desk::default()
        };
        desk.sections = group_sections(&[meta("api", false, false)], &[], &[live("api", "ready")]);
        let buffer = paint(&mut desk, 120, 24);
        let text = buffer_text(&buffer);
        let (row, _) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("Manager unavailable"))
            .expect("notice row");
        // Inside the SERVICES block, right of the sidebar — not a full-width banner row.
        let inner_x = desk.geometry.main_items.x;
        let row16 = row as u16;
        assert_eq!(
            buffer.cell((inner_x, row16)).unwrap().symbol(),
            "M",
            "{text}"
        );
        assert_eq!(
            buffer.cell((inner_x - 1, row16)).unwrap().symbol(),
            "│",
            "{text}"
        );
        assert!(
            text.lines().nth(row - 1).unwrap().contains("SERVICES"),
            "{text}"
        );
        // Items start below the notice; a click on it hits nothing.
        assert_eq!(desk.geometry.main_items.y, row16 + 1, "{text}");
        assert_eq!(desk.hit(inner_x as usize, row), None);
    }
}
