//! Port of `src/tui/screen.ts` — the pure `(state, geometry) -> lines` layout engine backing the
//! TUI's single frame: header, a scrollable service list, then the selected service's log.
use crate::state::Service;
use crate::text_utils::{sanitize_terminal_text, truncate_to_width};

#[derive(Debug, Clone, Copy)]
pub struct Viewport {
    pub columns: usize,
    pub rows: usize,
}

#[derive(Debug, Clone, Default)]
struct ScreenState {
    services: Vec<Service>,
    selected_name: String,
    log_service: String,
    log: String,
    notice: String,
    /// The focused service's URLs, pre-formatted one per line (`label  url`).
    urls: Vec<String>,
}

/// A `Partial<ScreenState>` patch — only fields set to `Some` are compared/applied by `update`.
#[derive(Debug, Clone, Default)]
pub struct ScreenUpdate {
    pub services: Option<Vec<Service>>,
    pub selected_name: Option<String>,
    pub log_service: Option<String>,
    pub log: Option<String>,
    pub notice: Option<String>,
    pub urls: Option<Vec<String>>,
}

/// At most this many URL rows are shown, so a service with many URLs cannot push its own log off
/// the screen.
const MAX_URL_ROWS: usize = 4;

const HEADER_HEIGHT: usize = 3;
const HEADER_LINE_1: &str = "Hearth";
const HEADER_LINE_2: &str = "↑/k ↓/j select • Enter toggle • x stop focused • r/R rebuild and restart focused • a start all • s stop all • q quit";

pub struct ServiceScreen {
    terminal: Viewport,
    state: ScreenState,
    service_offset: usize,
    revision: u64,
    cache_key: String,
    cache: Vec<String>,
}

impl ServiceScreen {
    pub fn new(terminal: Viewport) -> Self {
        Self { terminal, state: ScreenState { log: "Loading…".to_string(), ..Default::default() }, service_offset: 0, revision: 0, cache_key: String::new(), cache: Vec::new() }
    }

    pub fn update(&mut self, patch: ScreenUpdate) {
        let selection_changed = patch.selected_name.as_deref().is_some_and(|n| n != self.state.selected_name);
        let unchanged = patch.services.as_ref().is_none_or(|v| same_services(&self.state.services, v))
            && patch.selected_name.as_ref().is_none_or(|v| v == &self.state.selected_name)
            && patch.log_service.as_ref().is_none_or(|v| v == &self.state.log_service)
            && patch.log.as_ref().is_none_or(|v| v == &self.state.log)
            && patch.notice.as_ref().is_none_or(|v| v == &self.state.notice)
            && patch.urls.as_ref().is_none_or(|v| v == &self.state.urls);
        if unchanged {
            return;
        }
        if let Some(v) = patch.services {
            self.state.services = v;
        }
        if let Some(v) = patch.selected_name {
            self.state.selected_name = v;
        }
        if let Some(v) = patch.log_service {
            self.state.log_service = v;
        }
        if let Some(v) = patch.log {
            self.state.log = v;
        }
        if let Some(v) = patch.notice {
            self.state.notice = v;
        }
        if let Some(v) = patch.urls {
            self.state.urls = v;
        }
        if selection_changed {
            let height = self.service_height(self.terminal.rows);
            self.reveal_selected(height);
        }
        self.revision += 1;
    }

    /// Applies wheel movement inside service rows and returns the nearest visible service.
    pub fn handle_wheel(&mut self, row: usize, delta: i64, viewport_rows: Option<usize>) -> Option<String> {
        let viewport_rows = viewport_rows.unwrap_or(self.terminal.rows);
        let service_height = self.service_height(viewport_rows);
        let first_service_row = HEADER_HEIGHT + 1;
        if row < first_service_row || row >= first_service_row + service_height {
            return None;
        }
        let max_offset = self.max_service_offset(service_height) as i64;
        let offset = (self.service_offset as i64 + delta).clamp(0, max_offset) as usize;
        if offset != self.service_offset {
            self.service_offset = offset;
            self.revision += 1;
        }
        let selected_index = self.state.services.iter().position(|s| s.name == self.state.selected_name);
        let nearest_index = match selected_index {
            Some(index) if index < self.service_offset => self.service_offset,
            Some(index) if index >= self.service_offset + service_height => self.service_offset + service_height - 1,
            Some(index) => index,
            None => return self.state.services.get(self.service_offset).map(|s| s.name.clone()),
        };
        self.state.services.get(nearest_index).map(|s| s.name.clone())
    }

    /// Returns the service rendered at a left-click row, if any.
    pub fn service_at(&mut self, row: usize, viewport_rows: Option<usize>) -> Option<String> {
        let viewport_rows = viewport_rows.unwrap_or(self.terminal.rows);
        let service_height = self.service_height(viewport_rows);
        if row < HEADER_HEIGHT + 1 {
            return None;
        }
        let service_row = row - HEADER_HEIGHT - 1;
        if service_row >= service_height {
            return None;
        }
        let start = self.clamp_service_offset(service_height);
        self.state.services.get(start + service_row).map(|s| s.name.clone())
    }

    pub fn render(&mut self, width: usize, viewport_rows: Option<usize>) -> &[String] {
        let viewport_rows = viewport_rows.unwrap_or(self.terminal.rows);
        let rows = viewport_rows;
        let cache_key = format!("{width}:{rows}:{}", self.revision);
        if cache_key == self.cache_key {
            return &self.cache;
        }

        let body_height = rows.saturating_sub(HEADER_HEIGHT);
        let service_height = self.service_height(rows);
        let url_rows = self.state.urls.len().min(MAX_URL_ROWS);
        let log_height = body_height.saturating_sub(service_height + url_rows + if service_height > 0 { 2 } else { 1 });
        let start = self.clamp_service_offset(service_height);
        let services: Vec<&Service> = self.state.services.iter().skip(start).take(service_height).collect();
        let has_service_scrollbar = self.state.services.len() > service_height;
        let service_width = width.saturating_sub(if has_service_scrollbar { 1 } else { 0 });
        let logs = split_lines(&self.state.log, log_height);

        let mut lines: Vec<String> = vec![fit(HEADER_LINE_1, width), fit(HEADER_LINE_2, width), fit(&self.state.notice, width)];
        lines.truncate(rows);

        if service_height > 0 {
            lines.push(fit("SERVICES", width));
            for index in 0..service_height {
                let service = services.get(index).copied();
                let kind = if service.and_then(|s| s.kind) == Some(hearth_core::catalog::ServiceKind::Infrastructure) { " infra" } else { "" };
                let content = match service {
                    Some(service) => format!("{} {} {}{}", if service.name == self.state.selected_name { ">" } else { " " }, status_label(&service.state), service.name, kind),
                    None => String::new(),
                };
                let scrollbar = if has_service_scrollbar { scrollbar_cell(index, start, service_height, self.state.services.len()) } else { "" };
                lines.push(format!("{}{}", fit(&content, service_width), scrollbar));
            }
        }
        for url in self.state.urls.iter().take(url_rows) {
            if lines.len() < rows {
                lines.push(fit(&format!("URL {url}"), width));
            }
        }
        if lines.len() < rows {
            lines.push(fit(&format!("LOG — {}", self.state.log_service), width));
            lines.extend(logs.iter().map(|log| fit(log, width)));
        }
        while lines.len() < rows {
            lines.push(" ".repeat(width));
        }
        lines.truncate(rows);

        self.cache_key = cache_key;
        self.cache = lines;
        &self.cache
    }

    fn service_height(&self, viewport_rows: usize) -> usize {
        let body_height = viewport_rows.saturating_sub(HEADER_HEIGHT);
        if body_height < 3 {
            0
        } else {
            self.state.services.len().min(((body_height - 2) / 3).max(1))
        }
    }

    fn max_service_offset(&self, service_height: usize) -> usize {
        self.state.services.len().saturating_sub(service_height)
    }

    fn clamp_service_offset(&mut self, service_height: usize) -> usize {
        self.service_offset = self.service_offset.min(self.max_service_offset(service_height));
        self.service_offset
    }

    /// Keep keyboard selection visible without discarding a manual viewport otherwise.
    fn reveal_selected(&mut self, service_height: usize) {
        if service_height == 0 {
            return;
        }
        let Some(selected_index) = self.state.services.iter().position(|s| s.name == self.state.selected_name) else { return };
        if selected_index < self.service_offset {
            self.service_offset = selected_index;
        } else if selected_index >= self.service_offset + service_height {
            self.service_offset = selected_index - service_height + 1;
        }
        self.clamp_service_offset(service_height);
    }

    pub fn invalidate(&mut self) {
        self.cache_key.clear();
    }
}

fn fit(text: &str, width: usize) -> String {
    truncate_to_width(&sanitize_terminal_text(text), width, true)
}

fn status_colour(state: &str) -> u8 {
    match state {
        "ready" => 32,
        "running" => 36,
        "preparing" | "queued-start" | "starting" | "stopping" | "degraded" => 33,
        "stopped" => 90,
        "failed" | "orphaned" | "externally-owned" => 31,
        _ => 37,
    }
}

fn status_label(state: &str) -> String {
    format!("\x1b[{}m{:<9}\x1b[0m", status_colour(state), state)
}

fn scrollbar_cell(index: usize, start: usize, height: usize, total: usize) -> &'static str {
    let thumb_height = (((height * height) as f64 / total as f64).ceil() as usize).max(1);
    let max_thumb_start = height.saturating_sub(thumb_height);
    let max_start = total.saturating_sub(height);
    let thumb_start = if max_start == 0 { 0 } else { ((start * max_thumb_start) as f64 / max_start as f64).round() as usize };
    if index >= thumb_start && index < thumb_start + thumb_height {
        "█"
    } else {
        "░"
    }
}

fn split_lines(text: &str, limit: usize) -> Vec<&str> {
    if limit == 0 {
        return Vec::new();
    }
    let all: Vec<&str> = text.split('\n').collect();
    let skip = all.len().saturating_sub(limit);
    all[skip..].to_vec()
}

fn same_services(left: &[Service], right: &[Service]) -> bool {
    left.len() == right.len() && left.iter().zip(right.iter()).all(|(a, b)| a.name == b.name && a.kind == b.kind && a.state == b.state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Service;
    use crate::text_utils::visible_width;

    fn terminal() -> Viewport {
        Viewport { columns: 80, rows: 8 }
    }

    fn strip_ansi(line: &str) -> String {
        let mut out = String::new();
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                let mut j = i + 2;
                while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';' || bytes[j] == b':') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'm' {
                    i = j + 1;
                    continue;
                }
            }
            let ch = line[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }

    fn service(name: &str, state: &str) -> Service {
        Service { name: name.to_string(), kind: None, state: state.to_string(), generation: None, current_operation_id: None }
    }

    #[test]
    fn keeps_every_rendered_line_within_a_narrow_terminal_width() {
        let mut screen = ServiceScreen::new(terminal());
        screen.update(ScreenUpdate {
            services: Some(vec![service("very-long-service-name", "stopped")]),
            selected_name: Some("very-long-service-name".to_string()),
            log_service: Some("very-long-service-name".to_string()),
            log: Some("a deliberately long log line that must be truncated".to_string()),
            ..Default::default()
        });
        assert!(screen.render(2, None).iter().all(|l| visible_width(l) <= 2));
        assert!(screen.render(16, None).iter().all(|l| visible_width(l) <= 16));
    }

    #[test]
    fn shows_the_focused_service_stop_binding() {
        let mut screen = ServiceScreen::new(Viewport { columns: 160, rows: 8 });
        assert!(screen.render(160, None).iter().any(|l| l.contains("x stop focused")));
    }

    #[test]
    fn labels_infrastructure_services_in_the_normal_service_list() {
        let mut screen = ServiceScreen::new(Viewport { columns: 80, rows: 8 });
        let mut mongo = service("mongo", "ready");
        mongo.kind = Some(hearth_core::catalog::ServiceKind::Infrastructure);
        screen.update(ScreenUpdate { services: Some(vec![mongo]), selected_name: Some("mongo".to_string()), ..Default::default() });
        assert!(screen.render(80, None).iter().any(|l| strip_ansi(l).contains("mongo infra")));
    }

    #[test]
    fn renders_stopped_service_logs_below_the_service_pane() {
        let mut screen = ServiceScreen::new(terminal());
        screen.update(ScreenUpdate {
            services: Some(vec![service("frontend", "stopped")]),
            selected_name: Some("frontend".to_string()),
            log_service: Some("frontend".to_string()),
            log: Some("No log yet.".to_string()),
            ..Default::default()
        });
        let lines = screen.render(80, None).to_vec();
        let log_index = lines.iter().position(|l| strip_ansi(l).contains("LOG — frontend")).unwrap();
        let service_index = lines.iter().position(|l| strip_ansi(l).contains("> stopped   frontend")).unwrap();
        assert!(log_index > service_index);
        assert!(lines.iter().any(|l| strip_ansi(l).contains("No log yet.")));
    }

    #[test]
    fn renders_the_newest_retained_log_lines() {
        let mut screen = ServiceScreen::new(terminal());
        screen.update(ScreenUpdate {
            services: Some(vec![service("metadata", "ready")]),
            selected_name: Some("metadata".to_string()),
            log_service: Some("metadata".to_string()),
            log: Some("old-1\nold-2\nold-3\nold-4\nnew-1\nnew-2".to_string()),
            ..Default::default()
        });
        let lines = screen.render(80, None).to_vec();
        assert!(lines.iter().any(|l| l.contains("new-1")));
        assert!(lines.iter().any(|l| l.contains("new-2")));
        let joined = lines.join("\n");
        for old in ["old-1", "old-2", "old-3", "old-4"] {
            assert!(!joined.contains(old));
        }
    }

    #[test]
    fn returns_the_same_frame_until_state_changes() {
        let mut screen = ServiceScreen::new(terminal());
        let first = screen.render(80, None).to_vec();
        let second = screen.render(80, None).to_vec();
        assert_eq!(first, second);
        screen.update(ScreenUpdate { notice: Some("updated".to_string()), ..Default::default() });
        assert_ne!(screen.render(80, None).to_vec(), first);
    }

    #[test]
    fn does_not_invalidate_the_frame_for_equivalent_status_data() {
        let mut screen = ServiceScreen::new(terminal());
        screen.update(ScreenUpdate { services: Some(vec![service("metadata", "ready")]), ..Default::default() });
        let first = screen.render(80, None).to_vec();
        screen.update(ScreenUpdate { services: Some(vec![service("metadata", "ready")]), ..Default::default() });
        assert_eq!(screen.render(80, None).to_vec(), first);
    }

    #[test]
    fn never_returns_more_rows_than_the_terminal_owns() {
        let mut screen = ServiceScreen::new(Viewport { columns: 80, rows: 2 });
        screen.update(ScreenUpdate { services: Some(vec![service("metadata", "ready")]), ..Default::default() });
        assert_eq!(screen.render(80, None).len(), 2);
    }

    #[test]
    fn fills_every_row_when_the_caller_passes_the_frame_height() {
        let mut screen = ServiceScreen::new(Viewport { columns: 80, rows: 8 });
        screen.update(ScreenUpdate { services: Some(vec![service("metadata", "ready")]), ..Default::default() });
        assert_eq!(screen.render(80, Some(20)).len(), 20);
        assert_eq!(screen.render(80, Some(3)).len(), 3);
    }

    #[test]
    fn strips_log_cursor_and_erase_sequences_that_would_displace_later_rows() {
        let mut screen = ServiceScreen::new(terminal());
        screen.update(ScreenUpdate {
            services: Some(vec![service("composer", "ready")]),
            selected_name: Some("composer".to_string()),
            log_service: Some("composer".to_string()),
            log: Some("\x1b[2J\x1b[3J\x1b[Hstarting compile\nsecond line".to_string()),
            ..Default::default()
        });
        let lines = screen.render(80, None).to_vec();
        assert_eq!(lines.len(), 8);
        for line in &lines {
            let stripped = strip_ansi(line);
            assert!(!stripped.bytes().any(|b| b <= 0x1f || b == 0x7f));
        }
        assert!(lines.iter().any(|l| l.contains("starting compile")));
        assert!(lines.iter().any(|l| l.contains("second line")));
    }

    #[test]
    fn keeps_log_colour_sequences() {
        let mut screen = ServiceScreen::new(terminal());
        screen.update(ScreenUpdate {
            services: Some(vec![service("composer", "ready")]),
            selected_name: Some("composer".to_string()),
            log_service: Some("composer".to_string()),
            log: Some("\x1b[90m1:53 PM\x1b[0m ready".to_string()),
            ..Default::default()
        });
        assert!(screen.render(80, None).iter().any(|l| l.contains("\x1b[90m1:53 PM\x1b[0m")));
    }

    #[test]
    fn keeps_a_carriage_return_log_line_on_one_row() {
        let mut screen = ServiceScreen::new(terminal());
        screen.update(ScreenUpdate {
            services: Some(vec![service("frontend", "ready")]),
            selected_name: Some("frontend".to_string()),
            log_service: Some("frontend".to_string()),
            log: Some("compile step\r\nnext step".to_string()),
            ..Default::default()
        });
        let lines = screen.render(80, None).to_vec();
        assert!(lines.iter().any(|l| l.contains("compile step")));
        assert!(lines.iter().any(|l| l.contains("next step")));
    }

    #[test]
    fn pads_every_row_to_the_full_width_at_any_geometry() {
        let services: Vec<Service> = (0..30).map(|i| service(&format!("svc-{i}"), "ready")).collect();
        for (columns, rows) in [(3, 4), (12, 5), (40, 6), (200, 40)] {
            let mut screen = ServiceScreen::new(Viewport { columns, rows });
            screen.update(ScreenUpdate { services: Some(services.clone()), selected_name: Some("svc-29".to_string()), log_service: Some("svc-29".to_string()), log: Some("x".repeat(500)), ..Default::default() });
            let lines = screen.render(columns, None).to_vec();
            assert_eq!(lines.len(), rows);
            for line in &lines {
                assert_eq!(visible_width(line), columns);
            }
        }
    }

    #[test]
    fn scrolls_the_list_to_keep_the_selected_service_visible_and_renders_its_position() {
        let services: Vec<Service> = (0..20).map(|i| service(&format!("svc-{i}"), "ready")).collect();
        let mut screen = ServiceScreen::new(Viewport { columns: 80, rows: 20 });
        screen.update(ScreenUpdate { services: Some(services), selected_name: Some("svc-19".to_string()), ..Default::default() });
        let lines = screen.render(80, None).to_vec();
        assert!(lines.iter().any(|l| strip_ansi(l).contains("> ready     svc-19")));
        assert!(lines.iter().any(|l| l.ends_with('█')));
        assert!(lines.iter().any(|l| l.ends_with('░')));
    }

    #[test]
    fn colours_service_states_without_colouring_their_names() {
        let mut screen = ServiceScreen::new(Viewport { columns: 80, rows: 20 });
        screen.update(ScreenUpdate { services: Some(vec![service("ready-service", "ready"), service("queued-service", "queued-start"), service("failed-service", "failed")]), ..Default::default() });
        let lines = screen.render(80, None).to_vec();
        assert!(lines.iter().any(|l| l.contains("\x1b[32mready    ")));
        assert!(lines.iter().any(|l| l.contains("\x1b[31mfailed   ")));
        assert!(lines.iter().any(|l| l.contains("\x1b[33mqueued-start")));
        assert!(lines.iter().any(|l| strip_ansi(l).contains("queued-service")));
        assert!(lines.iter().any(|l| strip_ansi(l).contains("failed-service")));
    }

    #[test]
    fn shows_the_first_services_when_the_selection_is_unknown() {
        let services: Vec<Service> = (0..20).map(|i| service(&format!("svc-{i}"), "ready")).collect();
        let mut screen = ServiceScreen::new(Viewport { columns: 80, rows: 8 });
        screen.update(ScreenUpdate { services: Some(services), selected_name: Some("missing".to_string()), ..Default::default() });
        assert!(screen.render(80, None).iter().any(|l| l.contains("svc-0")));
    }

    #[test]
    fn keeps_wheel_focus_while_it_remains_visible_then_selects_the_closest_visible_boundary() {
        let services: Vec<Service> = (0..20).map(|i| service(&format!("svc-{i}"), "ready")).collect();
        let mut screen = ServiceScreen::new(Viewport { columns: 80, rows: 20 });
        screen.update(ScreenUpdate { services: Some(services.clone()), selected_name: Some("svc-3".to_string()), ..Default::default() });

        let mut selected = screen.handle_wheel(4, 1, None);
        assert_eq!(selected.as_deref(), Some("svc-3"));
        screen.update(ScreenUpdate { selected_name: selected.clone(), ..Default::default() });

        for _ in 0..3 {
            selected = screen.handle_wheel(4, 1, None);
            screen.update(ScreenUpdate { selected_name: selected.clone(), ..Default::default() });
        }
        assert_eq!(selected.as_deref(), Some("svc-4"));
        assert!(screen.render(80, None).iter().any(|l| strip_ansi(l).contains("> ready     svc-4")));

        for _ in 0..services.len() {
            selected = screen.handle_wheel(4, 1, None);
            screen.update(ScreenUpdate { selected_name: selected.clone(), ..Default::default() });
        }
        assert_eq!(selected.as_deref(), Some("svc-15"));
        assert!(screen.render(80, None).iter().any(|l| strip_ansi(l).contains("> ready     svc-15")));
    }

    #[test]
    fn returns_the_service_under_a_click_row() {
        let services: Vec<Service> = (0..20).map(|i| service(&format!("svc-{i}"), "ready")).collect();
        let mut screen = ServiceScreen::new(Viewport { columns: 80, rows: 20 });
        screen.update(ScreenUpdate { services: Some(services), selected_name: Some("svc-0".to_string()), ..Default::default() });

        assert_eq!(screen.service_at(4, None).as_deref(), Some("svc-0"));
        assert_eq!(screen.service_at(5, None).as_deref(), Some("svc-1"));
        assert_eq!(screen.service_at(3, None), None);
        screen.handle_wheel(4, 1, None);
        assert_eq!(screen.service_at(4, None).as_deref(), Some("svc-1"));
    }

    #[test]
    fn keeps_keyboard_selection_visible_after_manual_service_scrolling() {
        let services: Vec<Service> = (0..20).map(|i| service(&format!("svc-{i}"), "ready")).collect();
        let mut screen = ServiceScreen::new(Viewport { columns: 80, rows: 20 });
        screen.update(ScreenUpdate { services: Some(services), selected_name: Some("svc-0".to_string()), ..Default::default() });
        for _ in 0..3 {
            screen.handle_wheel(4, 1, None);
        }

        screen.update(ScreenUpdate { selected_name: Some("svc-1".to_string()), ..Default::default() });
        assert!(screen.render(80, None).iter().any(|l| strip_ansi(l).contains("> ready     svc-1")));

        screen.update(ScreenUpdate { selected_name: Some("svc-19".to_string()), ..Default::default() });
        assert!(screen.render(80, None).iter().any(|l| strip_ansi(l).contains("> ready     svc-19")));
    }

    #[test]
    fn ignores_wheel_input_outside_rendered_service_rows() {
        let services: Vec<Service> = (0..20).map(|i| service(&format!("svc-{i}"), "ready")).collect();
        let mut screen = ServiceScreen::new(Viewport { columns: 80, rows: 20 });
        screen.update(ScreenUpdate { services: Some(services), selected_name: Some("svc-0".to_string()), log: Some("log content".to_string()), ..Default::default() });
        let first = screen.render(80, None).to_vec();

        assert_eq!(screen.handle_wheel(3, 1, None), None);
        assert_eq!(screen.handle_wheel(9, 1, None), None);
        assert_eq!(screen.render(80, None).to_vec(), first);
        assert!(screen.render(80, None).iter().any(|l| strip_ansi(l).contains("svc-0")));
    }

    #[test]
    fn shows_the_focused_service_urls_above_its_log() {
        let mut screen = ServiceScreen::new(terminal());
        screen.update(ScreenUpdate {
            services: Some(vec![service("metadata", "ready")]),
            selected_name: Some("metadata".to_string()),
            log_service: Some("metadata".to_string()),
            log: Some("hello".to_string()),
            urls: Some(vec!["app  http://127.0.0.1:1166/".to_string()]),
            ..Default::default()
        });
        let lines: Vec<String> = screen.render(80, Some(12)).iter().map(|l| strip_ansi(l)).collect();
        let url = lines.iter().position(|l| l.contains("URL app  http://127.0.0.1:1166/")).expect("url row rendered");
        let log = lines.iter().position(|l| l.starts_with("LOG")).expect("log header rendered");
        assert!(url < log, "urls sit between the services and the log: {lines:?}");
    }
}
