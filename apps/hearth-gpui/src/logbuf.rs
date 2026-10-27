//! Paged reads of a service log.
//!
//! `hearth logs --tail` only keeps the last lines of one 16 KiB snapshot, and
//! the daemon caps `GET /v1/logs/:id` at that same tail. Older bytes live in
//! `<runtime>/logs/<service>.log`, the file the daemon appends. This module
//! reads that file from the end, one page at a time.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const LOG_PAGE: u64 = 16 * 1024;
const MAX_WINDOW: u64 = 1024 * 1024;
const BACKSCAN: u64 = 4096;

/// One colored span on a plain log line. Offsets are bytes into that line.
/// `fg` and `bg` are `0xRRGGBB`. Unset colors use the pane's own text color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogPaint {
    pub start: usize,
    pub end: usize,
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogPage {
    pub lines: Vec<String>,
    /// Parallel to `lines`. Empty when that line has no SGR color.
    pub paints: Vec<Vec<LogPaint>>,
    pub origin: u64,
    pub end: u64,
    pub has_more: bool,
    /// Lines inserted in front of the previous window. Zero for a fresh tail.
    pub inserted: usize,
    /// Lines removed from the front because the window passed its cap.
    pub dropped: usize,
    /// The file shrank or was replaced. The page is a new tail.
    pub reset: bool,
}

/// A character-column selection inside the loaded log lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogSelection {
    pub anchor_line: usize,
    pub anchor_col: usize,
    pub head_line: usize,
    pub head_col: usize,
}

impl LogSelection {
    pub fn is_caret(self) -> bool {
        self.anchor_line == self.head_line && self.anchor_col == self.head_col
    }

    /// Inclusive start and exclusive end as `(line, char column)`.
    pub fn ordered(self) -> ((usize, usize), (usize, usize)) {
        let anchor = (self.anchor_line, self.anchor_col);
        let head = (self.head_line, self.head_col);
        if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        }
    }
}

/// Still on the tail. A few pixels cover rounding; any further travel leaves
/// the bottom, so a short scroll keeps the current rows when new lines arrive.
pub fn sticks_to_tail(max_scroll: f32, scroll: f32) -> bool {
    !max_scroll.is_finite() || max_scroll <= 0.5 || !scroll.is_finite() || max_scroll - scroll <= 4.
}

/// Byte range to paint on `line` for this selection. `None` when the line is outside it.
pub fn selection_bytes(
    selection: LogSelection,
    line_index: usize,
    line: &str,
) -> Option<std::ops::Range<usize>> {
    if selection.is_caret() {
        return None;
    }
    let (start, end) = selection.ordered();
    if line_index < start.0 || line_index > end.0 {
        return None;
    }
    let from = if line_index == start.0 {
        byte_at(line, start.1)
    } else {
        0
    };
    let to = if line_index == end.0 {
        byte_at(line, end.1)
    } else {
        line.len()
    };
    (from < to).then_some(from..to)
}

pub fn selected_text(lines: &[String], selection: LogSelection) -> String {
    if selection.is_caret() || lines.is_empty() {
        return String::new();
    }
    let (start, end) = selection.ordered();
    let last = lines.len() - 1;
    if start.0 > last {
        return String::new();
    }
    let end_line = end.0.min(last);
    let mut parts = Vec::new();
    for (index, line) in lines.iter().enumerate().take(end_line + 1).skip(start.0) {
        if start.0 == end_line {
            let from = byte_at(line, start.1);
            let to = byte_at(line, end.1);
            parts.push(line.get(from..to.max(from)).unwrap_or("").to_string());
            break;
        } else if index == start.0 {
            parts.push(line.get(byte_at(line, start.1)..).unwrap_or("").to_string());
        } else if index == end_line {
            parts.push(line.get(..byte_at(line, end.1)).unwrap_or("").to_string());
        } else {
            parts.push(line.clone());
        }
    }
    parts.join("\n")
}

fn byte_at(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map(|(index, _)| index)
        .unwrap_or(line.len())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogFetch {
    Page(LogPage),
    Same,
    Exhausted,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thumb {
    pub top: f32,
    pub height: f32,
    pub travel: f32,
    pub max_scroll: f32,
    pub scroll: f32,
    pub viewport: f32,
    pub scrollable: bool,
}

pub fn thumb_metrics(viewport: f32, content: f32, scroll: f32) -> Thumb {
    let viewport = if viewport.is_finite() && viewport > 1. {
        viewport
    } else {
        1.
    };
    let content = if content.is_finite() {
        content.max(0.)
    } else {
        0.
    };
    let max_scroll = (content - viewport).max(0.);
    let scrollable = max_scroll > 0.5;
    let min_thumb = 28_f32.min(viewport);
    let height = if scrollable {
        (viewport * viewport / content.max(viewport)).clamp(min_thumb, viewport)
    } else {
        viewport
    };
    let travel = (viewport - height).max(0.);
    let scroll = if scroll.is_finite() {
        scroll.clamp(0., max_scroll)
    } else {
        0.
    };
    let top = if max_scroll <= 0.5 || travel <= 0. {
        0.
    } else {
        scroll / max_scroll * travel
    };
    Thumb {
        top,
        height,
        travel,
        max_scroll,
        scroll,
        viewport,
        scrollable,
    }
}

/// The log file for one service inside a runtime directory.
pub fn service_log_path(runtime: &Path, service: &str) -> Option<PathBuf> {
    if !service_id_ok(service) {
        return None;
    }
    Some(runtime.join("logs").join(format!("{service}.log")))
}

pub fn fetch_tail(path: &Path, page: u64) -> LogFetch {
    let Ok((mut file, len)) = open_len(path) else {
        return LogFetch::Failed("No log yet.".into());
    };
    page_from(&mut file, len, 0, len, page, true).unwrap_or_else(LogFetch::Failed)
}

pub fn fetch_earlier(path: &Path, page: u64, origin: u64, end: u64) -> LogFetch {
    if origin == 0 {
        return LogFetch::Exhausted;
    }
    let Ok((mut file, len)) = open_len(path) else {
        return LogFetch::Failed("No log yet.".into());
    };
    if len < end || origin > len {
        return fetch_tail(path, page);
    }
    let rough = origin.saturating_sub(page.max(1));
    let new_origin = match line_start_at_or_before(&mut file, rough) {
        Ok(origin) => origin,
        Err(error) => return LogFetch::Failed(error),
    };
    if new_origin >= origin {
        return LogFetch::Exhausted;
    }
    let inserted = match read_span(&mut file, new_origin, origin) {
        Ok(prefix) => inserted_lines(&String::from_utf8_lossy(&prefix)),
        Err(error) => return LogFetch::Failed(error),
    };
    match page_from(&mut file, len, new_origin, len, page, false) {
        Ok(LogFetch::Page(mut page)) => {
            page.inserted = inserted;
            page.reset = false;
            Ok(LogFetch::Page(page))
        }
        other => other,
    }
    .unwrap_or_else(LogFetch::Failed)
}

pub fn fetch_refresh(path: &Path, page: u64, origin: u64, end: u64) -> LogFetch {
    let Ok((mut file, len)) = open_len(path) else {
        return LogFetch::Failed("No log yet.".into());
    };
    if len < end || origin > len {
        return fetch_tail(path, page);
    }
    if len == end {
        return LogFetch::Same;
    }
    if len.saturating_sub(origin) > MAX_WINDOW {
        return trim_window(&mut file, len, origin).unwrap_or_else(LogFetch::Failed);
    }
    page_from(&mut file, len, origin, len, page, false).unwrap_or_else(LogFetch::Failed)
}

/// Keep the newest bytes once the loaded window would pass [`MAX_WINDOW`].
fn trim_window(file: &mut File, len: u64, old_origin: u64) -> Result<LogFetch, String> {
    let rough = len.saturating_sub(MAX_WINDOW);
    let new_origin = align_forward(file, rough, len)?.max(old_origin);
    let dropped = if new_origin > old_origin {
        let prefix = read_span(file, old_origin, new_origin)?;
        inserted_lines(&String::from_utf8_lossy(&prefix))
    } else {
        0
    };
    match page_from(file, len, new_origin, len, MAX_WINDOW, false)? {
        LogFetch::Page(mut page) => {
            page.dropped = dropped;
            page.reset = false;
            Ok(LogFetch::Page(page))
        }
        other => Ok(other),
    }
}

fn page_from(
    file: &mut File,
    len: u64,
    requested: u64,
    end: u64,
    page: u64,
    tail: bool,
) -> Result<LogFetch, String> {
    let end = end.min(len);
    let origin = if tail {
        let rough = len.saturating_sub(page.max(1));
        align_forward(file, rough, len)?
    } else {
        requested.min(end)
    };
    if end < origin {
        return Err("log window is out of range".into());
    }
    let bytes = read_span(file, origin, end)?;
    let decoded = decode_log(&String::from_utf8_lossy(&bytes));
    let mut lines = decoded.lines;
    let mut paints = decoded.paints;
    let mut has_more = origin > 0 && end.saturating_sub(origin) < MAX_WINDOW;
    if lines.is_empty() {
        lines.push("Log is empty.".into());
        paints.push(Vec::new());
        has_more = false;
    }
    Ok(LogFetch::Page(LogPage {
        lines,
        paints,
        origin,
        end,
        has_more,
        inserted: 0,
        dropped: 0,
        reset: tail,
    }))
}

fn service_id_ok(service: &str) -> bool {
    !service.is_empty()
        && service.len() < 200
        && service != "."
        && service != ".."
        && service
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '@'))
}

/// The runtime directory a project daemon writes to: `runtimeDirectory:` from
/// its `hearth.yaml`, else `.hearth/runtime-v1` under the root.
pub fn runtime_directory(root: &Path) -> PathBuf {
    let yaml = std::fs::read_to_string(root.join("hearth.yaml")).unwrap_or_default();
    for raw in yaml.lines() {
        if raw.starts_with([' ', '\t', '#']) {
            continue;
        }
        let Some(rest) = raw.trim().strip_prefix("runtimeDirectory:") else {
            continue;
        };
        let value = rest.trim().trim_matches(['"', '\'']);
        if value.is_empty() || value.contains('\0') {
            break;
        }
        let path = PathBuf::from(value);
        return if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
    }
    root.join(".hearth/runtime-v1")
}

/// The smp daemon's runtime directory. Its root is `~/.hearth/shared`, and the
/// runtime directory sits directly under it rather than in a nested `.hearth`,
/// because that root already is the hearth-namespaced directory.
pub fn shared_runtime_directory() -> PathBuf {
    ns_ui::home_dir().join(".hearth/shared/runtime-v1")
}

fn open_len(path: &Path) -> Result<(File, u64), String> {
    let file = File::open(path).map_err(|_| "No log yet.".to_string())?;
    let len = file.metadata().map_err(|error| error.to_string())?.len();
    Ok((file, len))
}

fn read_span(file: &mut File, start: u64, end: u64) -> Result<Vec<u8>, String> {
    if end < start {
        return Err("log window is out of range".into());
    }
    let len = (end - start) as usize;
    if len > MAX_WINDOW as usize {
        return Err("log window is too large".into());
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|error| error.to_string())?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)
        .map_err(|error| error.to_string())?;
    Ok(buf)
}

/// First byte of a line at or after `rough`. A cut through the middle of a
/// line waits for the next newline so the pane does not open on a fragment.
fn align_forward(file: &mut File, rough: u64, end: u64) -> Result<u64, String> {
    if rough == 0 || rough >= end {
        return Ok(rough.min(end));
    }
    let prev = read_span(file, rough - 1, rough)?;
    if prev.first() == Some(&b'\n') {
        return Ok(rough);
    }
    let buf = read_span(file, rough, end)?;
    if let Some(index) = buf.iter().position(|byte| *byte == b'\n') {
        let next = rough + index as u64 + 1;
        if next < end {
            return Ok(next);
        }
    }
    Ok(rough)
}

/// First byte of the line that contains `rough`, looking back at most
/// [`BACKSCAN`] bytes. Reaching byte 0 means the file start.
fn line_start_at_or_before(file: &mut File, rough: u64) -> Result<u64, String> {
    if rough == 0 {
        return Ok(0);
    }
    let from = rough.saturating_sub(BACKSCAN);
    let buf = read_span(file, from, rough)?;
    if let Some(index) = buf.iter().rposition(|byte| *byte == b'\n') {
        return Ok(from + index as u64 + 1);
    }
    if from == 0 { Ok(0) } else { Ok(rough) }
}

fn inserted_lines(prefix: &str) -> usize {
    if prefix.is_empty() {
        0
    } else {
        decode_log(prefix).plain.matches('\n').count()
    }
}

struct Decoded {
    plain: String,
    lines: Vec<String>,
    paints: Vec<Vec<LogPaint>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Sgr {
    fg: Option<u32>,
    bg: Option<u32>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    reverse: bool,
}

impl Sgr {
    fn styled(self) -> bool {
        self.fg.is_some()
            || self.bg.is_some()
            || self.bold
            || self.dim
            || self.italic
            || self.underline
            || self.reverse
    }

    fn paint(self, start: usize, end: usize) -> LogPaint {
        LogPaint {
            start,
            end,
            fg: self.fg,
            bg: self.bg,
            bold: self.bold,
            dim: self.dim,
            italic: self.italic,
            underline: self.underline,
            reverse: self.reverse,
        }
    }

    fn apply(self, params: &str) -> Self {
        let mut next = self;
        let parts: Vec<u32> = if params.is_empty() {
            vec![0]
        } else {
            params
                .split(';')
                .map(|part| part.parse().unwrap_or(0))
                .collect()
        };
        let mut index = 0;
        while index < parts.len() {
            let code = parts[index];
            match code {
                0 => next = Sgr::default(),
                1 => next.bold = true,
                2 => next.dim = true,
                3 => next.italic = true,
                4 => next.underline = true,
                7 => next.reverse = true,
                22 => {
                    next.bold = false;
                    next.dim = false;
                }
                23 => next.italic = false,
                24 => next.underline = false,
                27 => next.reverse = false,
                39 => next.fg = None,
                49 => next.bg = None,
                30..=37 => next.fg = Some(ansi_color(code - 30)),
                40..=47 => next.bg = Some(ansi_color(code - 40)),
                90..=97 => next.fg = Some(ansi_color(code - 90 + 8)),
                100..=107 => next.bg = Some(ansi_color(code - 100 + 8)),
                38 | 48 => {
                    let foreground = code == 38;
                    if parts.get(index + 1) == Some(&5) && index + 2 < parts.len() {
                        if let Some(color) = indexed_color(parts[index + 2]) {
                            if foreground {
                                next.fg = Some(color)
                            } else {
                                next.bg = Some(color)
                            }
                        }
                        index += 2;
                    } else if parts.get(index + 1) == Some(&2) && index + 4 < parts.len() {
                        let color = (parts[index + 2].min(255) << 16)
                            | (parts[index + 3].min(255) << 8)
                            | parts[index + 4].min(255);
                        if foreground {
                            next.fg = Some(color)
                        } else {
                            next.bg = Some(color)
                        }
                        index += 4;
                    }
                }
                _ => {}
            }
            index += 1;
        }
        next
    }
}

/// ANSI 0 is black. The log pane is dark, so foreground 0 is a readable gray.
fn ansi_color(index: u32) -> u32 {
    const PALETTE: [u32; 16] = [
        0x9aa3b2, 0xcc2626, 0x26a633, 0xbf991a, 0x3366e6, 0xb340b3, 0x26a6b3, 0xbfbfbf, 0x737373,
        0xff5959, 0x66e673, 0xffe666, 0x73a6ff, 0xe680e6, 0x73f2f2, 0xffffff,
    ];
    PALETTE[index as usize % 16]
}

fn indexed_color(index: u32) -> Option<u32> {
    if index <= 15 {
        return Some(ansi_color(index));
    }
    if (232..=255).contains(&index) {
        let level = 8 + (index - 232) * 10;
        return Some((level << 16) | (level << 8) | level);
    }
    if (16..=231).contains(&index) {
        let n = index - 16;
        let steps = [0, 95, 135, 175, 215, 255];
        let red = steps[(n / 36) as usize];
        let green = steps[((n / 6) % 6) as usize];
        let blue = steps[(n % 6) as usize];
        return Some((red << 16) | (green << 8) | blue);
    }
    None
}

/// Drop cursor, erase, and OSC sequences. SGR colors stay on the plain text.
fn decode_log(text: &str) -> Decoded {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut style = Sgr::default();
    let mut plain = String::new();
    let mut line = String::new();
    let mut paints = Vec::new();
    let mut run_at: Option<usize> = None;
    let mut lines = Vec::new();
    let mut all = Vec::new();
    let mut last_newline = text.is_empty();

    let flush = |run_at: &mut Option<usize>, line: &str, style: Sgr, paints: &mut Vec<LogPaint>| {
        let Some(start) = run_at.take() else { return };
        if line.len() > start && style.styled() {
            paints.push(style.paint(start, line.len()));
        }
    };
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            let (next, sgr) = scan_escape(bytes, index);
            if let Some(params) = sgr {
                let updated = style.apply(&params);
                if updated != style {
                    flush(&mut run_at, &line, style, &mut paints);
                    style = updated;
                }
            }
            index = next;
            continue;
        }
        if bytes[index] == b'\n' {
            flush(&mut run_at, &line, style, &mut paints);
            plain.push('\n');
            lines.push(std::mem::take(&mut line));
            all.push(std::mem::take(&mut paints));
            last_newline = true;
            index += 1;
            continue;
        }
        if bytes[index] == b'\r' {
            index += 1;
            continue;
        }
        let ch = text[index..].chars().next().unwrap_or(' ');
        if style.styled() && run_at.is_none() {
            run_at = Some(line.len());
        }
        if !style.styled() {
            run_at = None;
        }
        line.push(ch);
        plain.push(ch);
        last_newline = false;
        index += ch.len_utf8();
    }
    if !last_newline {
        flush(&mut run_at, &line, style, &mut paints);
        lines.push(line);
        all.push(paints);
    }
    Decoded {
        plain,
        lines,
        paints: all,
    }
}

fn scan_escape(bytes: &[u8], start: usize) -> (usize, Option<String>) {
    let next = start + 1;
    if next >= bytes.len() {
        return (bytes.len(), None);
    }
    if bytes[next] == b'[' {
        let mut cursor = next + 1;
        let params_at = cursor;
        while cursor < bytes.len() && is_sgr_param(bytes[cursor]) {
            cursor += 1;
        }
        let params_end = cursor;
        while cursor < bytes.len() && (0x20..=0x2f).contains(&bytes[cursor]) {
            cursor += 1;
        }
        if cursor >= bytes.len() || !(0x40..=0x7e).contains(&bytes[cursor]) {
            return ((start + 1).min(bytes.len()), None);
        }
        let end = cursor + 1;
        if bytes[cursor] == b'm' && cursor == params_end {
            let params = String::from_utf8_lossy(&bytes[params_at..params_end]).into_owned();
            return (end, Some(params));
        }
        return (end, None);
    }
    if bytes[next] == b']' {
        let mut cursor = next + 1;
        while cursor < bytes.len() {
            if bytes[cursor] == 0x07 {
                return (cursor + 1, None);
            }
            if bytes[cursor] == 0x1b && cursor + 1 < bytes.len() && bytes[cursor + 1] == b'\\' {
                return (cursor + 2, None);
            }
            cursor += 1;
        }
        return (bytes.len(), None);
    }
    ((next + 1).min(bytes.len()), None)
}

fn is_sgr_param(byte: u8) -> bool {
    byte.is_ascii_digit() || byte == b';' || byte == b':'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("hearth-logbuf-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    fn page(fetch: LogFetch) -> LogPage {
        match fetch {
            LogFetch::Page(page) => page,
            other => panic!("expected a page, got {other:?}"),
        }
    }

    #[test]
    fn tail_of_a_short_file_is_the_whole_file() {
        let dir = temp("short");
        let path = dir.join("svc.log");
        write(&path, "one\ntwo\nthree\n");
        let page = page(fetch_tail(&path, 64));
        assert_eq!(page.lines, vec!["one", "two", "three"]);
        assert!(!page.has_more);
        assert_eq!(page.origin, 0);
        assert!(page.reset);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tail_opens_on_the_newest_line_and_earlier_pages_rebuild_the_file() {
        let dir = temp("pages");
        let path = dir.join("svc.log");
        let lines: Vec<String> = (0..40).map(|index| format!("line-{index:02}")).collect();
        write(&path, &(lines.join("\n") + "\n"));
        let mut window = page(fetch_tail(&path, 80));
        assert_eq!(window.lines.last().unwrap(), "line-39");
        assert!(window.has_more);
        assert_ne!(window.lines.first().unwrap(), "line-00");
        let mut guard = 0;
        while window.has_more {
            guard += 1;
            assert!(guard < 20, "paging did not reach the start");
            let next = page(fetch_earlier(&path, 80, window.origin, window.end));
            assert!(next.inserted > 0);
            assert_eq!(next.lines.len(), window.lines.len() + next.inserted);
            assert_eq!(next.lines[next.inserted..], window.lines[..]);
            window = next;
        }
        assert_eq!(window.lines, lines);
        assert!(matches!(
            fetch_earlier(&path, 80, window.origin, window.end),
            LogFetch::Exhausted
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refresh_appends_and_a_shorter_file_resets_the_tail() {
        let dir = temp("refresh");
        let path = dir.join("svc.log");
        write(&path, "alpha\nbeta\n");
        let first = page(fetch_tail(&path, 64));
        assert!(matches!(
            fetch_refresh(&path, 64, first.origin, first.end),
            LogFetch::Same
        ));
        write(&path, "alpha\nbeta\ngamma\n");
        let grown = page(fetch_refresh(&path, 64, first.origin, first.end));
        assert_eq!(grown.lines, vec!["alpha", "beta", "gamma"]);
        assert!(!grown.reset);
        assert_eq!(grown.inserted, 0);
        write(&path, "only\n");
        let reset = page(fetch_refresh(&path, 64, grown.origin, grown.end));
        assert!(reset.reset);
        assert_eq!(reset.lines, vec!["only"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ansi_and_crlf_do_not_become_extra_lines() {
        let dir = temp("ansi");
        let path = dir.join("svc.log");
        write(&path, "plain\r\n\u{1b}[31mred\u{1b}[0m\r\n");
        let page = page(fetch_tail(&path, 64));
        assert_eq!(page.lines, vec!["plain", "red"]);
        assert!(page.paints[0].is_empty());
        assert_eq!(page.paints[1][0].fg, Some(0xcc2626));
        assert_eq!(page.paints[1][0].start, 0);
        assert_eq!(page.paints[1][0].end, 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sgr_colors_stay_on_the_plain_text_and_cross_lines() {
        let dir = temp("color");
        let path = dir.join("svc.log");
        write(
            &path,
            "\u{1b}[32mLOG\u{1b}[39m \u{1b}[38;5;3m[Nest]\u{1b}[0m\n\u{1b}[1;31mred\nstill\u{1b}[0m\n\u{1b}[2K\u{1b}[1Gkeep\n",
        );
        let page = page(fetch_tail(&path, 256));
        assert_eq!(page.lines, vec!["LOG [Nest]", "red", "still", "keep"]);
        assert_eq!(page.paints[0][0].fg, Some(0x26a633));
        assert_eq!(
            &page.lines[0][page.paints[0][0].start..page.paints[0][0].end],
            "LOG"
        );
        assert_eq!(page.paints[0][1].fg, Some(0xbf991a));
        assert_eq!(
            &page.lines[0][page.paints[0][1].start..page.paints[0][1].end],
            "[Nest]"
        );
        assert!(page.paints[1][0].bold);
        assert_eq!(page.paints[1][0].fg, Some(0xcc2626));
        assert_eq!(page.paints[2][0].fg, Some(0xcc2626));
        assert!(page.paints[3].is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_is_a_soft_failure() {
        let dir = temp("missing");
        assert!(matches!(
            fetch_tail(&dir.join("nope.log"), 64),
            LogFetch::Failed(_)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn service_path_stays_inside_the_runtime_log_dir() {
        let dir = temp("path");
        write(
            &dir.join("hearth.yaml"),
            "runtimeDirectory: .hearth/runtime-v1\nservices:\n",
        );
        let runtime = runtime_directory(&dir);
        assert!(runtime.ends_with(".hearth/runtime-v1"));
        let path = service_log_path(&runtime, "config-server").unwrap();
        assert!(path.ends_with(".hearth/runtime-v1/logs/config-server.log"));
        assert!(service_log_path(&runtime, "../secret").is_none());
        assert!(service_log_path(&runtime, "a/b").is_none());
        assert!(service_log_path(&runtime, "").is_none());
        // A shared instance id carries its version, so `@` is a legal id.
        assert!(service_log_path(&runtime, "redis@8.2.10").is_some());
        assert!(shared_runtime_directory().ends_with(".hearth/shared/runtime-v1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn selection_reads_the_dragged_span_and_ignores_a_caret() {
        let lines = vec![
            "alpha".to_string(),
            "bravo".to_string(),
            "chárlie".to_string(),
        ];
        let selection = LogSelection {
            anchor_line: 0,
            anchor_col: 2,
            head_line: 2,
            head_col: 3,
        };
        assert_eq!(selected_text(&lines, selection), "pha\nbravo\nchá");
        assert_eq!(selection_bytes(selection, 1, "bravo"), Some(0..5));
        assert_eq!(selection_bytes(selection, 2, "chárlie"), Some(0..4));
        let backward = LogSelection {
            anchor_line: 2,
            anchor_col: 3,
            head_line: 0,
            head_col: 2,
        };
        assert_eq!(
            selected_text(&lines, backward),
            selected_text(&lines, selection)
        );
        let caret = LogSelection {
            anchor_line: 1,
            anchor_col: 1,
            head_line: 1,
            head_col: 1,
        };
        assert!(caret.is_caret());
        assert_eq!(selected_text(&lines, caret), "");
        assert_eq!(selection_bytes(caret, 1, "bravo"), None);
    }

    #[test]
    fn a_few_pixels_stay_on_the_tail_and_a_short_scroll_leaves_it() {
        assert!(sticks_to_tail(1000., 1000.));
        assert!(sticks_to_tail(1000., 996.));
        assert!(!sticks_to_tail(1000., 990.));
        assert!(sticks_to_tail(0., 0.));
    }

    #[test]
    fn refresh_drops_the_front_once_the_window_passes_a_megabyte() {
        let dir = temp("trim");
        let path = dir.join("svc.log");
        let mut body = String::new();
        for index in 0..1000 {
            body.push_str(&format!("A{index:06}\n"));
        }
        write(&path, &body);
        let first = page(fetch_tail(&path, 2 * 1024 * 1024));
        assert_eq!(first.origin, 0);
        assert_eq!(first.lines[0], "A000000");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let mut extra = 0usize;
        while std::fs::metadata(&path).unwrap().len() <= 1024 * 1024 {
            use std::io::Write;
            file.write_all(b"B234567\n").unwrap();
            extra += 1;
        }
        let grown = page(fetch_refresh(&path, 1024 * 1024, first.origin, first.end));
        assert!(grown.dropped > 0, "dropped {}", grown.dropped);
        assert!(!grown.reset);
        assert!(grown.origin > 0);
        assert_eq!(grown.lines.last().map(String::as_str), Some("B234567"));
        assert_ne!(grown.lines[0], "A000000");
        assert_eq!(grown.lines.len() + grown.dropped, first.lines.len() + extra);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn thumb_sits_at_the_bottom_when_scrolled_to_the_end() {
        let thumb = thumb_metrics(100., 400., 300.);
        assert!(thumb.scrollable);
        assert!(thumb.height < 100.);
        assert!((thumb.top + thumb.height - 100.).abs() < 0.6);
        let top = thumb_metrics(100., 400., 0.);
        assert!(top.top < 0.6);
    }
}
