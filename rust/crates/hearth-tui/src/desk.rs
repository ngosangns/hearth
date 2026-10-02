//! Layout, key map, and Ratatui paint for the workspace shell. No network: `shell` applies
//! [`Command`]s and draws a [`Frame`]. The command table is the key map and the footer.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hearth_core::catalog::{CatalogGroup, ReadinessSpec, ServiceCatalog, ServiceKind};
use hearth_core::state::ActualServiceState;
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
    RemoveShared(String),
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
        let version = self.versions.get(self.version_index).map(String::as_str).unwrap_or("");
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
        self.sections.iter().flat_map(|section| section.services.iter()).find(|service| service.id == id)
    }

    /// Replaces the service tree and keeps the cursor on the same row when it still exists.
    pub fn set_sections(&mut self, sections: Vec<Section>, start_all: Vec<String>) {
        let keep = self.selected_service();
        self.sections = sections;
        self.start_all = start_all;
        let ids = self.service_ids();
        self.service_cursor = keep.and_then(|id| ids.iter().position(|row| row == &id)).unwrap_or(0);
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
        self.focus = if self.shared_open { Pane::Shared } else { Pane::Services };
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
        if area.width == 0 || area.height == 0 {
            return;
        }
        let notice = self.status_notice();
        let (header_h, notice_h, footer_h) = chrome_heights(area.height, !notice.is_empty());
        let [header, notice_area, body, footer] = Layout::vertical([
            Constraint::Length(header_h),
            Constraint::Length(notice_h),
            Constraint::Min(0),
            Constraint::Length(footer_h),
        ])
        .areas(area);
        if header_h > 0 {
            frame.render_widget(Paragraph::new(title_line(&self.summary, self.daemon_pid)).style(theme::title()), header);
        }
        if notice_h > 0 {
            frame.render_widget(Paragraph::new(notice).style(theme::notice()), notice_area);
        }
        if footer_h > 0 {
            frame.render_widget(Paragraph::new(footer_line(self, footer.width as usize)), footer);
        }
        if self.help {
            self.draw_help(frame, body);
        } else if let Some(sidebar) = split_at(body.width as usize) {
            self.draw_split(frame, body, sidebar as u16);
        } else {
            self.draw_narrow(frame, body);
        }
        if self.composer.is_some() || self.pending.is_some() {
            self.draw_modal(frame, area);
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
            scroll_window(&mut self.workspace_offset, &mut self.workspace_index, self.workspaces.len(), geo.workspace_items.height as usize, delta);
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
                    if let Some(item) = visuals.iter().skip(self.service_offset).find(|row| row.kind == Visual::Item) {
                        self.service_cursor = item.selectable;
                    }
                } else if current >= self.service_offset + height {
                    if let Some(item) = visuals.iter().take(self.service_offset + height).rev().find(|row| row.kind == Visual::Item) {
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
        let lines: Vec<Line> = HELP.iter().take(target.height as usize).map(|line| Line::from(*line)).collect();
        frame.render_widget(Paragraph::new(lines), target);
    }

    fn draw_split(&mut self, frame: &mut Frame, body: Rect, sidebar: u16) {
        let [left, right] = Layout::horizontal([Constraint::Length(sidebar), Constraint::Min(0)]).areas(body);
        let (height, offset) = self.fit_workspace(block_inner_height(left), self.workspaces.len());
        self.paint_workspaces(frame, left, height, offset);
        let main = if self.shared_open { Pane::Shared } else { Pane::Services };
        self.paint_main(frame, right, main);
    }

    fn draw_narrow(&mut self, frame: &mut Frame, body: Rect) {
        match self.focus {
            Pane::Workspaces => {
                let (height, offset) = self.fit_workspace(block_inner_height(body), self.workspaces.len());
                self.paint_workspaces(frame, body, height, offset);
                self.geometry.main = Pane::Workspaces;
            }
            Pane::Services | Pane::Shared => {
                let main = if self.focus == Pane::Shared || self.shared_open { Pane::Shared } else { Pane::Services };
                self.paint_main(frame, body, main);
            }
        }
    }

    fn fit_workspace(&mut self, height: usize, len: usize) -> (usize, usize) {
        let height = height.min(len);
        reveal(&mut self.workspace_offset, self.workspace_index, height, len);
        (height, self.workspace_offset)
    }

    fn paint_workspaces(&mut self, frame: &mut Frame, area: Rect, item_slots: usize, offset: usize) {
        let focused = self.focus == Pane::Workspaces;
        let block = pane_block("WORKSPACES", focused);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = item_slots.min(inner.height as usize);
        self.geometry.workspace_items = Rect { x: inner.x, y: inner.y, width: inner.width, height: rows as u16 };
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
            paint_line(buf, inner.x, inner.y + slot as u16, inner.width, &line, focused && current);
        }
    }

    fn paint_main(&mut self, frame: &mut Frame, area: Rect, pane: Pane) {
        let (list_area, log_area) = split_main(area);
        let visuals = if pane == Pane::Shared { shared_visuals(self) } else { service_visuals(self) };
        let cursor = if pane == Pane::Shared { self.shared_cursor } else { self.service_cursor };
        let focused = self.focus == pane;
        let title = pane_title(self, pane);
        let block = pane_block(&title, focused);
        let inner = block.inner(list_area);
        frame.render_widget(block, list_area);
        let url_rows = if pane == Pane::Services { self.urls.len().min(3).min(inner.height as usize) } else { 0 };
        let url_rows = if (inner.height as usize) > url_rows { url_rows } else { 0 };
        let item_capacity = (inner.height as usize).saturating_sub(url_rows);
        let item_height = item_capacity.min(visuals.len());
        let visual_cursor = visual_index_of_item(&visuals, cursor).unwrap_or(0);
        let offset = {
            let slot = if pane == Pane::Shared { &mut self.shared_offset } else { &mut self.service_offset };
            reveal(slot, visual_cursor, item_height, visuals.len());
            *slot
        };
        self.geometry.main = pane;
        self.geometry.main_offset = offset;
        self.geometry.main_items = Rect { x: inner.x, y: inner.y, width: inner.width, height: item_height as u16 };
        {
            let buf = frame.buffer_mut();
            for slot in 0..item_height {
                let painted = visuals.get(offset + slot);
                let selected = painted.is_some_and(|row| focused && row.kind == Visual::Item && row.selectable == cursor);
                let line = painted.map(|row| row.line.clone()).unwrap_or_else(|| Line::from(""));
                paint_line(buf, inner.x, inner.y + slot as u16, inner.width, &line, selected);
            }
            if pane == Pane::Services {
                for (index, url) in self.urls.iter().take(url_rows).enumerate() {
                    let line = Line::styled(format!("URL {url}"), theme::dim());
                    buf.set_line(inner.x, inner.y + item_height as u16 + index as u16, &line, inner.width);
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
            buf.set_line(log_inner.x, log_inner.y + index as u16, &line, log_inner.width);
        }
    }

    fn draw_modal(&mut self, frame: &mut Frame, area: Rect) {
        let composing = self.composer.clone();
        if composing.is_none() && self.pending.is_none() {
            return;
        }
        let title = if composing.is_some() { "add folder" } else { "confirm" };
        let rect = centered(area, modal_width(area), modal_height(area));
        self.geometry.modal = rect;
        frame.render_widget(Clear, rect);
        let block = Block::bordered().title(title).border_style(theme::modal()).title_style(theme::title());
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
            fitted_lines(&self.header_notice(), inner.width as usize, body_rows).into_iter().map(Line::from).collect()
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
        rows.push(Painted { line: item_line(selected, "daemon log"), kind: Visual::Item, selectable });
        selectable += 1;
    }
    let named = desk.sections.iter().any(|section| section.name.is_some());
    for section in &desk.sections {
        if let Some(name) = &section.name {
            let selected = focused && desk.service_cursor == selectable;
            rows.push(Painted { line: item_line(selected, &name.to_uppercase()), kind: Visual::Item, selectable });
            selectable += 1;
        } else if named {
            rows.push(Painted { line: Line::styled("OTHER", theme::dim()), kind: Visual::Label, selectable });
        }
        for service in &section.services {
            let selected = focused && desk.service_cursor == selectable;
            rows.push(Painted { line: service_line(service, selected), kind: Visual::Item, selectable });
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
        rows.push(Painted { line: Line::styled("RECIPES", theme::dim()), kind: Visual::Label, selectable });
    }
    for recipe in &desk.recipes {
        let selected = focused && desk.shared_cursor == selectable;
        let installed = desk.instances.iter().any(|instance| instance.id == recipe.chosen());
        let version = recipe.versions.get(recipe.version_index).map(String::as_str).unwrap_or("");
        rows.push(Painted { line: recipe_line(selected, &recipe.name, version, installed), kind: Visual::Item, selectable });
        selectable += 1;
    }
    if !desk.instances.is_empty() {
        rows.push(Painted { line: Line::styled("INSTALLED", theme::dim()), kind: Visual::Label, selectable });
    }
    for instance in &desk.instances {
        let selected = focused && desk.shared_cursor == selectable;
        let projects = match instance.attachments {
            0 => String::new(),
            1 => "  1 project".to_string(),
            n => format!("  {n} projects"),
        };
        let failure = instance.install_error.as_deref().filter(|text| !text.is_empty()).map(|text| format!("  {text}")).unwrap_or_default();
        rows.push(Painted { line: instance_line(instance, selected, &projects, &failure), kind: Visual::Item, selectable });
        selectable += 1;
    }
    rows
}

fn pane_title(desk: &Desk, pane: Pane) -> String {
    match pane {
        Pane::Shared => "SHARED".to_string(),
        Pane::Services | Pane::Workspaces => {
            let workspace = desk.selected_workspace();
            let name = workspace.map(|workspace| workspace.name.as_str()).unwrap_or("no workspace");
            let path = workspace.and_then(|workspace| (!workspace.path.is_empty()).then(|| format!("  {}", workspace.path))).unwrap_or_default();
            let summary = if desk.summary.is_empty() { String::new() } else { format!("  {}", desk.summary) };
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
    let ports = if service.ports.is_empty() { String::new() } else { format!("  {}", service.ports) };
    let detail = service.error.as_deref().filter(|text| !text.is_empty()).map(|text| format!("  {text}")).unwrap_or_default();
    Line::from(vec![
        Span::raw(format!("{mark} ")),
        Span::styled(format!("{label:<9}"), style),
        Span::raw(format!(" {name}{ports}{flags}{detail}")),
    ])
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

fn instance_line(instance: &InstanceLine, selected: bool, projects: &str, failure: &str) -> Line<'static> {
    let mark = if selected { ">" } else { " " };
    let state = compact_wire(&instance.actual_state);
    Line::from(vec![
        Span::raw(format!("{mark} {}  {}  ", instance.id, instance.install_state)),
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
        ActualServiceState::Preparing | ActualServiceState::QueuedStart | ActualServiceState::Starting | ActualServiceState::Stopping => Color::Yellow,
        ActualServiceState::Stopped => Color::DarkGray,
        ActualServiceState::Failed | ActualServiceState::Orphaned | ActualServiceState::ExternallyOwned => Color::Red,
    })
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
    let mut text = if summary.is_empty() { "Hearth".to_string() } else { format!("Hearth  {summary}") };
    if let Some(pid) = pid {
        text.push_str(&format!("  pid {pid}"));
    }
    Line::styled(text, theme::title())
}

fn plain_cell(text: &str) -> String {
    sgr_to_line(text).spans.into_iter().map(|span| span.content.into_owned()).collect()
}

fn chrome_heights(height: u16, show_notice: bool) -> (u16, u16, u16) {
    if height == 0 {
        return (0, 0, 0);
    }
    if height == 1 {
        return (1, 0, 0);
    }
    let notice = u16::from(show_notice);
    if height <= notice + 1 {
        return (1, 0, height - 1);
    }
    (1, notice, 1)
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused { theme::focus() } else { theme::idle() };
    Block::bordered().title(title).border_style(style).title_style(style)
}

fn block_inner_height(area: Rect) -> usize {
    pane_block("", false).inner(area).height as usize
}

fn split_main(area: Rect) -> (Rect, Rect) {
    if area.height < 6 {
        let log = Rect { x: area.x, y: area.bottom(), width: area.width, height: 0 };
        return (area, log);
    }
    let log_h = ((area.height as usize) / 3).clamp(3, area.height as usize - 3) as u16;
    let [list, log] = Layout::vertical([Constraint::Length(area.height - log_h), Constraint::Min(0)]).areas(area);
    (list, log)
}

fn paint_line(buf: &mut Buffer, x: u16, y: u16, width: u16, line: &Line, selected: bool) {
    if width == 0 {
        return;
    }
    if selected {
        buf.set_style(Rect { x, y, width, height: 1 }, Style::default().bg(Color::Indexed(238)).add_modifier(Modifier::BOLD));
    }
    buf.set_line(x, y, line, width);
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
    if area.width <= 24 { area.width } else { (area.width - 4).clamp(24, 68) }
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

const fn bind(code: KeyCode, command: Command, hint: Option<&'static str>, show: Audience, gate: Gate) -> Binding {
    Binding { code, command, hint, show, gate }
}

const COMPOSER_HINTS: &[&str] = &["esc cancel", "enter add", "bksp erase"];
const HELP_HINTS: &[&str] = &["q quit", "esc close", "? close"];
const CONFIRM_HINTS: &[&str] = &["enter confirm", "esc cancel"];

/// Dispatch order is first match. Hint order is this order, with aliases (`hint: None`) omitted.
const BINDINGS: &[Binding] = &[
    bind(KeyCode::Char('j'), Command::Move(1), Some("j/k move"), Audience::Always, Gate::Always),
    bind(KeyCode::Char('k'), Command::Move(-1), Some("j/k move"), Audience::Always, Gate::Always),
    bind(KeyCode::Enter, Command::Activate, Some("enter act"), Audience::Always, Gate::Always),
    bind(KeyCode::Char('x'), Command::Stop, Some("x stop"), Audience::Main, Gate::Always),
    bind(KeyCode::Char('a'), Command::StartAll, Some("a all"), Audience::Services, Gate::Always),
    bind(KeyCode::Char('s'), Command::StopAll, Some("s stop all"), Audience::Services, Gate::Always),
    bind(KeyCode::Char('K'), Command::Reclaim, Some("K reclaim"), Audience::Services, Gate::Always),
    bind(KeyCode::Char('c'), Command::CopyUrl, Some("c copy"), Audience::Main, Gate::Always),
    bind(KeyCode::Char('l'), Command::ReloadCatalog, Some("l reload"), Audience::Main, Gate::Always),
    bind(KeyCode::Char('['), Command::CycleVersion(-1), Some("[ ] version"), Audience::Shared, Gate::Always),
    bind(KeyCode::Char('X'), Command::RemoveShared, Some("X remove"), Audience::Shared, Gate::Focus(Pane::Shared)),
    bind(KeyCode::Char('o'), Command::OpenFolder, Some("o reveal"), Audience::Workspace, Gate::Focus(Pane::Workspaces)),
    bind(KeyCode::Backspace, Command::Forget, Some("bksp forget"), Audience::Workspace, Gate::Focus(Pane::Workspaces)),
    bind(KeyCode::Char('D'), Command::StopDaemon, Some("D stop daemon"), Audience::Workspace, Gate::Focus(Pane::Workspaces)),
    bind(KeyCode::Char('n'), Command::Add, Some("n add"), Audience::Always, Gate::Always),
    bind(KeyCode::Char('r'), Command::Restart, Some("r restart daemon"), Audience::Workspace, Gate::Always),
    bind(KeyCode::Char('r'), Command::Restart, Some("r restart"), Audience::Main, Gate::Always),
    bind(KeyCode::PageUp, Command::ScrollLog(1), Some("pgup/pgdn log"), Audience::Main, Gate::Always),
    bind(KeyCode::Tab, Command::FocusNext, Some("tab pane"), Audience::Always, Gate::Always),
    bind(KeyCode::Char('S'), Command::ToggleShared, Some("S shared"), Audience::Always, Gate::Always),
    bind(KeyCode::Char('?'), Command::ToggleHelp, Some("? help"), Audience::Always, Gate::Always),
    bind(KeyCode::Char('q'), Command::Quit, Some("q quit"), Audience::Always, Gate::Always),
    bind(KeyCode::Up, Command::Move(-1), None, Audience::Always, Gate::Always),
    bind(KeyCode::Down, Command::Move(1), None, Audience::Always, Gate::Always),
    bind(KeyCode::Char(' '), Command::Activate, None, Audience::Always, Gate::Always),
    bind(KeyCode::Char('R'), Command::Restart, None, Audience::Always, Gate::Always),
    bind(KeyCode::PageDown, Command::ScrollLog(-1), None, Audience::Always, Gate::Always),
    bind(KeyCode::Char(']'), Command::CycleVersion(1), None, Audience::Always, Gate::Always),
    bind(KeyCode::Char('u'), Command::Updates, None, Audience::Always, Gate::Always),
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
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    }

    pub fn notice() -> Style {
        Style::default().fg(Color::Yellow)
    }

    pub fn dim() -> Style {
        Style::default().fg(Color::DarkGray)
    }

    pub fn focus() -> Style {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
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
    BINDINGS.iter().find(|binding| binding.code == key.code && gate_open(binding.gate, desk)).map(|binding| binding.command)
}

fn composer_key(key: KeyEvent) -> Option<Command> {
    match key.code {
        KeyCode::Esc => Some(Command::CancelComposer),
        KeyCode::Enter => Some(Command::SubmitComposer),
        KeyCode::Backspace => Some(Command::Backspace),
        KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) && !ch.is_control() => Some(Command::Type(ch)),
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
                Some(service) if service.state == ActualServiceState::ExternallyOwned => Act::Reclaim(id),
                Some(service) if service.state == ActualServiceState::Stopping => Act::Idle,
                Some(service) if service.state == ActualServiceState::QueuedStart || is_up(service.state) => Act::StopService(id),
                Some(_) => Act::StartService(id),
                None => Act::Idle,
            },
            Some(RowId::Recipe(_) | RowId::Instance(_)) => Act::Idle,
        },
        Pane::Shared => match desk.selected_shared() {
            Some(RowId::Recipe(name)) => desk.recipes.iter().find(|recipe| recipe.name == name).map(|recipe| Act::Install(recipe.chosen())).unwrap_or(Act::Idle),
            Some(RowId::Instance(id)) => {
                let up = desk.instances.iter().find(|instance| instance.id == id).is_some_and(|instance| matches!(compact_wire(&instance.actual_state), "ready" | "running"));
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
        .map(|section| section.services.iter().filter(|service| !service.disabled && !service.finite).collect())
        .unwrap_or_default();
    !long_lived.is_empty() && long_lived.iter().all(|service| is_up(service.state))
}

/// Stop-all skips disabled rows and rows that are already stopped or finished.
pub fn stop_all_targets(sections: &[Section]) -> Vec<String> {
    sections
        .iter()
        .flat_map(|section| section.services.iter())
        .filter(|service| !service.disabled && !matches!(service.state, ActualServiceState::Stopped | ActualServiceState::Succeeded))
        .map(|service| service.id.clone())
        .collect()
}

/// A URL that requires the process is hidden until the row is ready or running.
pub fn url_visible(requires_running: bool, state: ActualServiceState) -> bool {
    !requires_running || is_up(state)
}

/// The first observed catalog mtime is recorded. A later change reloads once.
pub fn should_reload_catalog(previous: Option<u64>, next: Option<u64>) -> bool {
    matches!((previous, next), (Some(before), Some(after)) if before != after)
}

pub fn group_targets(sections: &[Section], name: &str) -> Vec<String> {
    sections
        .iter()
        .find(|section| section.name.as_deref() == Some(name))
        .map(|section| section.services.iter().filter(|service| !service.disabled).map(|service| service.id.clone()).collect())
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
        if matches!(service.state, ActualServiceState::Ready | ActualServiceState::Running | ActualServiceState::RunningUnready) {
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
pub fn group_sections(metas: &[ServiceMeta], tree: &[CatalogGroup], live: &[Service]) -> Vec<Section> {
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
            });
        }
    }
    let line = |meta: &ServiceMeta| -> ServiceLine {
        let found = live.iter().find(|service| service.name == meta.id);
        let state = found.map(|service| service.state).unwrap_or(ActualServiceState::Stopped);
        let error = found.and_then(|service| service.error.clone()).filter(|text| !text.is_empty()).or_else(|| {
            if state == ActualServiceState::Failed {
                found.and_then(|service| service.readiness_detail.clone()).filter(|text| !text.is_empty())
            } else {
                None
            }
        });
        ServiceLine {
            id: meta.id.clone(),
            label: if meta.label.is_empty() { meta.id.clone() } else { meta.label.clone() },
            ports: meta.ports.clone(),
            state,
            disabled: meta.disabled,
            finite: meta.finite,
            infra: meta.infra || found.and_then(|service| service.kind) == Some(ServiceKind::Infrastructure),
            error,
        }
    };
    if tree.is_empty() {
        return vec![Section { name: None, services: order.iter().map(line).collect() }];
    }
    let mut first = HashMap::new();
    for group in tree {
        for member in &group.members {
            first.entry(member.clone()).or_insert_with(|| group.name.clone());
        }
    }
    let mut built: Vec<Section> = tree.iter().map(|group| Section { name: Some(group.name.clone()), services: Vec::new() }).collect();
    let mut rest = Vec::new();
    for meta in &order {
        let service = line(meta);
        if let Some(name) = first.get(&meta.id) {
            if let Some(section) = built.iter_mut().find(|section| section.name.as_deref() == Some(name.as_str())) {
                section.services.push(service);
                continue;
            }
        }
        rest.push(service);
    }
    built.retain(|section| !section.services.is_empty());
    if !rest.is_empty() {
        built.push(Section { name: None, services: rest });
    }
    if built.is_empty() {
        vec![Section { name: None, services: Vec::new() }]
    } else {
        built
    }
}

pub fn metas_from_catalog(catalog: &ServiceCatalog) -> Vec<ServiceMeta> {
    catalog
        .services
        .iter()
        .map(|service| ServiceMeta {
            id: service.id.clone(),
            label: service.label.clone().unwrap_or_else(|| service.id.clone()),
            ports: service.ports.as_ref().map(|ports| ports.iter().map(|port| port.port.to_string()).collect::<Vec<_>>().join(", ")).unwrap_or_default(),
            disabled: service.disabled,
            finite: matches!(service.profiles.run.readiness(), ReadinessSpec::Exit),
            infra: service.kind == Some(ServiceKind::Infrastructure),
        })
        .collect()
}

pub fn start_all_targets(catalog: &ServiceCatalog, sections: &[Section]) -> Vec<String> {
    catalog.groups.get("all").cloned().filter(|ids| !ids.is_empty()).unwrap_or_else(|| {
        sections.iter().flat_map(|section| section.services.iter()).filter(|service| !service.disabled).map(|service| service.id.clone()).collect()
    })
}

fn is_up(state: ActualServiceState) -> bool {
    matches!(state, ActualServiceState::Ready | ActualServiceState::Running | ActualServiceState::RunningUnready)
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
    let workspace = |id: &str| -> String { desk.workspaces.iter().find(|workspace| workspace.id == id).map(|workspace| workspace.name.clone()).unwrap_or_else(|| id.to_string()) };
    match pending {
        Pending::Trust(id) => {
            let name = workspace(id);
            let path = desk.workspaces.iter().find(|row| row.id == *id).map(|row| row.path.as_str()).unwrap_or("");
            if path.is_empty() {
                format!("press again to trust {name} and start its daemon")
            } else {
                format!("press again to trust {name} ({path}) and start its daemon")
            }
        }
        Pending::Forget(id) => format!("press again to forget {} — its services keep running", workspace(id)),
        Pending::StopDaemon(id) => format!("press again to stop the daemon for {} — its services stop", workspace(id)),
        Pending::RestartDaemon(id) => format!("press again to restart the daemon for {} — services keep running", workspace(id)),
        Pending::Reclaim(id) => {
            let holder = desk.service_line(id).and_then(|service| service.error.clone()).unwrap_or_else(|| format!("{id} is externally owned"));
            format!("{holder} — press again to kill it and start {id}")
        }
        Pending::RemoveShared(id) => {
            let attached = desk.instances.iter().find(|instance| &instance.id == id).map(|instance| instance.attachments).unwrap_or(0);
            if attached > 0 {
                format!("press again to remove {id} and delete data for {attached} attached project(s)")
            } else {
                format!("press again to remove {id}")
            }
        }
    }
}

fn scroll_window(offset: &mut usize, index: &mut usize, len: usize, height: usize, delta: i64) -> bool {
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
    visuals.iter().position(|row| row.kind == Visual::Item && row.selectable == item)
}

fn log_window<'a>(log: &'a str, height: usize, scroll: &mut usize) -> Vec<&'a str> {
    if height == 0 {
        return Vec::new();
    }
    let all: Vec<&str> = if log.is_empty() { vec![" "] } else { log.split('\n').collect() };
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
    "              x stops · r restarts · X removes (twice, and deletes attached project data)",
    "Log          page up and page down · u releases page · hearth update · esc cancels · q quits",
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
        line.spans.iter().map(|span| span.content.as_ref()).collect()
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
        ServiceMeta { id: id.to_string(), label: id.to_string(), ports: String::new(), disabled, finite, infra: id == "postgres" }
    }

    fn row(id: &str, state: ActualServiceState, finite: bool) -> ServiceLine {
        ServiceLine { id: id.into(), label: id.into(), ports: String::new(), state, disabled: false, finite, infra: false, error: None }
    }

    fn workspace(id: &str, name: &str, trusted: bool) -> WorkspaceLine {
        WorkspaceLine { id: id.into(), name: name.into(), path: String::new(), trusted, missing: false }
    }

    #[test]
    fn groups_by_first_declared_membership_and_keeps_the_rest() {
        let tree = vec![
            CatalogGroup { name: "infra".to_string(), members: vec!["postgres".to_string(), "not-a-service".to_string()] },
            CatalogGroup { name: "app".to_string(), members: vec!["api".to_string(), "postgres".to_string()] },
        ];
        let sections = group_sections(&[meta("api", false, false), meta("postgres", false, false), meta("worker", true, true)], &tree, &[live("api", "ready"), live("postgres", "stopped")]);
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
        assert!(desk.header_notice().contains("press again to trust viclass"));
        assert!(desk.arm(Pending::Trust("ID".into())));
    }

    #[test]
    fn enter_on_a_live_workspace_moves_to_services_and_external_rows_reclaim() {
        let mut desk = Desk::default();
        desk.workspaces.push(workspace("ID", "viclass", true));
        desk.attached = true;
        assert_eq!(activation(&desk), Act::FocusServices);
        desk.focus = Pane::Services;
        desk.sections = group_sections(&[meta("api", false, false)], &[], &[live("api", "externally-owned")]);
        desk.service_cursor = 1;
        assert_eq!(activation(&desk), Act::Reclaim("api".into()));
        desk.sections[0].services[0].disabled = true;
        desk.sections[0].services[0].state = ActualServiceState::Stopped;
        assert_eq!(activation(&desk), Act::Disabled);
    }

    #[test]
    fn a_narrow_frame_shows_only_the_focused_pane_and_stays_inside_the_terminal() {
        let mut desk = Desk {
            workspaces: vec![workspace("1", "viclass", true), workspace("2", "other", false)],
            sections: group_sections(&[meta("postgres", false, false)], &[], &[live("postgres", "ready")]),
            summary: summary_of(&group_sections(&[meta("postgres", false, false)], &[], &[live("postgres", "ready")])),
            log: "line-1\nline-2".into(),
            log_title: "postgres".into(),
            ..Desk::default()
        };
        let buffer = paint(&mut desk, 70, 16);
        assert_eq!((buffer.area.width, buffer.area.height), (70, 16));
        let joined = buffer_text(&buffer);
        assert!(joined.contains("viclass"), "{joined}");
        assert!(!joined.contains("postgres"), "{joined}");
        assert!(joined.contains("move") && joined.contains("quit"), "{joined}");
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
        let tree = vec![CatalogGroup { name: "infra".into(), members: vec!["postgres".into()] }];
        let sections = group_sections(&[meta("postgres", false, false), meta("api", false, false)], &tree, &[live("postgres", "ready")]);
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
        assert!(buffer.content.iter().any(|cell| cell.symbol() == ">" && cell.bg == Color::Indexed(238) && cell.modifier.contains(Modifier::BOLD)));
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "┌" && cell.fg == Color::Cyan));
        desk.focus = Pane::Workspaces;
        let workspace = buffer_text(&paint(&mut desk, 120, 24));
        assert!(workspace.contains("forget") && workspace.contains("quit"), "{workspace}");
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
                    error: Some("x".repeat(80)),
                }],
            }],
            ..Desk::default()
        };
        let text = buffer_text(&paint(&mut desk, 40, 16));
        assert!(text.contains("press again"), "{text}");
        assert!(text.contains("confirm") && text.contains("cancel"), "{text}");
    }

    #[test]
    fn shared_enter_installs_the_chosen_version_and_stops_a_running_instance() {
        let mut desk = Desk { focus: Pane::Shared, shared_open: true, ..Desk::default() };
        desk.recipes.push(RecipeLine { name: "redis".into(), versions: vec!["8.2.10".into(), "7.2".into()], version_index: 0 });
        desk.instances.push(InstanceLine { id: "redis@8.2.10".into(), install_state: "installed".into(), actual_state: "ready".into(), port: 43110, attachments: 2, install_error: None });
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
        assert!(!desk.arm(Pending::RemoveShared("redis@8.2.10".into())));
        assert!(desk.header_notice().contains("2 attached"));
    }

    #[test]
    fn question_mark_opens_help_and_other_keys_do_not_act() {
        let mut desk = Desk::default();
        assert_eq!(key_command(&desk, key('?')), Some(Command::ToggleHelp));
        desk.help = true;
        assert_eq!(key_command(&desk, key('a')), None);
        assert_eq!(key_command(&desk, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Some(Command::CloseHelp));
        assert!(buffer_text(&paint(&mut desk, 80, 16)).contains("untrusted"));
        assert_eq!(key_command(&Desk::default(), key('u')), Some(Command::Updates));
        let composing = Desk { composer: Some(String::new()), ..Desk::default() };
        assert_eq!(key_command(&composing, key('u')), Some(Command::Type('u')));
    }

    #[test]
    fn enter_cancels_a_queued_start_and_restarts_a_group_that_is_already_up() {
        let mut desk = Desk { focus: Pane::Services, ..Desk::default() };
        desk.sections = vec![Section { name: Some("app".into()), services: vec![row("api", ActualServiceState::QueuedStart, false)] }];
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
        assert_eq!(stop_all_targets(&sections), vec!["bad".to_string(), "wait".to_string()]);
    }

    #[test]
    fn urls_hide_until_the_service_is_up_and_a_catalog_change_reloads_once() {
        assert!(!url_visible(true, ActualServiceState::Starting));
        assert!(!url_visible(true, ActualServiceState::Stopped));
        assert!(url_visible(true, ActualServiceState::Ready));
        assert!(url_visible(true, ActualServiceState::RunningUnready));
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
            error: Some("timed out".into()),
        };
        let text = line_text(&service_line(&line, true));
        assert!(text.contains("API") && text.contains("api") && text.contains("8080") && text.contains("timed out"), "{text}");
        let workspace_row = WorkspaceLine { id: "1".into(), name: "viclass".into(), path: "~/src/viclass".into(), trusted: true, missing: false };
        let shown = line_text(&workspace_line(&workspace_row, true, "1/2 ready"));
        assert!(shown.contains("viclass") && shown.contains("~/src/viclass") && shown.contains("1/2 ready"), "{shown}");
    }

    #[test]
    fn command_table_keeps_focus_gates_and_aliases() {
        let desk = Desk::default();
        assert_eq!(key_command(&desk, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)), Some(Command::Move(-1)));
        assert_eq!(key_command(&desk, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)), Some(Command::Move(1)));
        assert_eq!(key_command(&desk, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)), Some(Command::ScrollLog(1)));
        assert_eq!(key_command(&desk, KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)), Some(Command::ScrollLog(-1)));
        assert_eq!(key_command(&desk, key(' ')), Some(Command::Activate));
        assert_eq!(key_command(&desk, key('R')), Some(Command::Restart));
        assert_eq!(key_command(&desk, key(']')), Some(Command::CycleVersion(1)));
        assert_eq!(key_command(&desk, key('D')), Some(Command::StopDaemon));
        assert_eq!(key_command(&desk, key('X')), None);
        let services = Desk { focus: Pane::Services, ..Desk::default() };
        assert_eq!(key_command(&services, key('D')), None);
        assert_eq!(key_command(&services, key('a')), Some(Command::StartAll));
        let shared = Desk { focus: Pane::Shared, shared_open: true, ..Desk::default() };
        assert_eq!(key_command(&shared, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Some(Command::ToggleShared));
        let armed = Desk { pending: Some(Pending::Forget("1".into())), ..Desk::default() };
        assert_eq!(key_command(&armed, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Some(Command::ClearArm));
        let composing = Desk { composer: Some(String::new()), ..Desk::default() };
        assert_eq!(key_command(&composing, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), Some(Command::Quit));
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
        assert!(buffer.content.iter().any(|cell| cell.symbol() == "L" && cell.fg == Color::Green));
    }

    #[test]
    fn hit_uses_the_cells_that_were_painted() {
        let mut desk = Desk {
            focus: Pane::Services,
            workspaces: vec![workspace("1", "viclass", true)],
            ..Desk::default()
        };
        desk.sections = group_sections(&[meta("postgres", false, false)], &[], &[live("postgres", "ready")]);
        let _ = paint(&mut desk, 120, 24);
        let workspace_at = desk.geometry.workspace_items;
        assert_eq!(desk.hit(workspace_at.x as usize, workspace_at.y as usize), Some(Hit::Workspace(0)));
        let service_at = desk.geometry.main_items;
        assert_eq!(desk.hit(service_at.x as usize, service_at.y as usize), Some(Hit::Service(0)));
    }
}
