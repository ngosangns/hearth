//! How the log pane lays out its lines.
//!
//! The pane is a fixed-row list, so a long line is cut into several display
//! rows here instead of being clipped. Selection, copy, and paint offsets stay
//! on the original line; a `Row` only says which byte range of which line it
//! shows. Runs of blank lines collapse to one row, and a log record that
//! announces a warning or an error carries that level to its continuation
//! lines.

use std::ops::Range;

/// Height of one display row in pixels.
pub const ROW_H: f32 = 18.;
/// Cells a wrapped continuation row is indented by.
pub const CONTINUATION_CELLS: usize = 2;

/// One display row: a byte range of one log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    /// Character column of `start` in the line.
    pub first_char: usize,
    /// A wrapped remainder of a longer line.
    pub cont: bool,
}

/// Every row of the loaded window, and where each line's rows begin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rows {
    pub rows: Vec<Row>,
    /// `first[line]` is the first row at or after `line`. One past the last
    /// line is the row count.
    first: Vec<usize>,
}

impl Rows {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The first row at or after `line`, clamped to the last row.
    pub fn first_row(&self, line: usize) -> usize {
        let at = self.first.get(line).copied().unwrap_or(self.rows.len());
        at.min(self.rows.len().saturating_sub(1))
    }

    /// Rows that belong to the first `lines` lines.
    pub fn rows_before(&self, lines: usize) -> usize {
        self.first.get(lines).copied().unwrap_or(self.rows.len())
    }

    pub fn line_of(&self, row: usize) -> Option<usize> {
        self.rows.get(row).map(|row| row.line)
    }
}

/// Terminal cells a character covers in a monospace face.
pub fn cell_width(ch: char) -> usize {
    let code = ch as u32;
    if code < 0x300 {
        return 1;
    }
    if (0x300..=0x36F).contains(&code) || code == 0x200B || code == 0x200D || code == 0xFE0F {
        return 0;
    }
    let wide = matches!(code,
        0x1100..=0x115F
        | 0x2E80..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF
        | 0x20000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

fn breaks_after(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '/' | ',')
}

/// Cut `lines` into display rows `cols` cells wide.
pub fn wrap(lines: &[String], cols: usize) -> Rows {
    let cols = cols.max(8);
    let mut rows = Vec::new();
    let mut first = Vec::with_capacity(lines.len() + 1);
    let mut previous_blank = false;
    for (index, line) in lines.iter().enumerate() {
        first.push(rows.len());
        if line.trim().is_empty() {
            if !previous_blank {
                rows.push(Row {
                    line: index,
                    start: 0,
                    end: line.len(),
                    first_char: 0,
                    cont: false,
                });
            }
            previous_blank = true;
            continue;
        }
        previous_blank = false;
        wrap_line(index, line, cols, &mut rows);
    }
    first.push(rows.len());
    Rows { rows, first }
}

fn wrap_line(index: usize, line: &str, cols: usize, rows: &mut Vec<Row>) {
    let mut start = 0;
    let mut start_char = 0;
    let mut cont = false;
    let mut cells = 0;
    let mut chars_in_row = 0;
    // Byte, char count, and cells just after the last place a row may end.
    let mut soft: Option<(usize, usize, usize)> = None;
    for (at, ch) in line.char_indices() {
        let cap = if cont {
            cols.saturating_sub(CONTINUATION_CELLS).max(1)
        } else {
            cols
        };
        let width = cell_width(ch);
        if cells + width > cap && chars_in_row > 0 {
            let (end, taken, _) = match soft {
                Some((byte, count, used)) if used * 2 >= cap => (byte, count, used),
                _ => (at, chars_in_row, cells),
            };
            rows.push(Row {
                line: index,
                start,
                end,
                first_char: start_char,
                cont,
            });
            start = end;
            start_char += taken;
            cont = true;
            // Re-measure what was carried over from the soft break.
            cells = 0;
            chars_in_row = 0;
            soft = None;
            for (offset, carried) in line[start..at].char_indices() {
                cells += cell_width(carried);
                chars_in_row += 1;
                if breaks_after(carried) {
                    soft = Some((start + offset + carried.len_utf8(), chars_in_row, cells));
                }
            }
        }
        cells += width;
        chars_in_row += 1;
        if breaks_after(ch) {
            soft = Some((at + ch.len_utf8(), chars_in_row, cells));
        }
    }
    rows.push(Row {
        line: index,
        start,
        end: line.len(),
        first_char: start_char,
        cont,
    });
}

/// The character column nearest `cell` cells into `row`.
pub fn char_at_cell(line: &str, row: &Row, cell: f32) -> usize {
    let mut used = 0.;
    let mut count = 0;
    for ch in line[row.start..row.end].chars() {
        let width = cell_width(ch) as f32;
        if used + width / 2. > cell {
            break;
        }
        used += width;
        count += 1;
    }
    row.first_char + count
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    None,
    Warn,
    Error,
}

/// The level a line carries, and the byte range of the word that named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineLevel {
    pub level: Level,
    pub token: Option<Range<usize>>,
}

impl LineLevel {
    const NONE: Self = Self {
        level: Level::None,
        token: None,
    };
}

enum Seen {
    Level(Level, Range<usize>),
    /// A new record with no warning or error level.
    Record,
    /// Nothing recognised.
    Plain,
}

fn tokens(line: &str) -> Vec<(Range<usize>, &str)> {
    let mut out = Vec::new();
    let mut from = None;
    for (at, ch) in line.char_indices() {
        if ch.is_whitespace() {
            if let Some(start) = from.take() {
                out.push((start..at, &line[start..at]));
            }
        } else if from.is_none() {
            from = Some(at);
        }
        if out.len() == 4 {
            return out;
        }
    }
    if let Some(start) = from {
        out.push((start..line.len(), &line[start..]));
    }
    out
}

fn timestampish(token: &str) -> bool {
    token
        .trim_start_matches(['[', '('])
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_digit())
}

fn classify(token: &str) -> Option<Option<Level>> {
    let word = token
        .trim_matches(|ch: char| "[]():<>|=!-,".contains(ch))
        .to_ascii_lowercase();
    let word = word.strip_prefix("level=").unwrap_or(&word);
    match word {
        "error" | "err" | "fatal" | "panic" | "critical" | "crit" | "severe" | "exception" => {
            Some(Some(Level::Error))
        }
        "warn" | "warning" => Some(Some(Level::Warn)),
        "info" | "debug" | "trace" | "notice" | "log" | "verbose" => Some(None),
        _ => None,
    }
}

fn detect(line: &str) -> Seen {
    // An indented line continues the record above it.
    if line.starts_with(char::is_whitespace) {
        return Seen::Plain;
    }
    let mut stamped = false;
    for (range, token) in tokens(line).into_iter().take(3) {
        match classify(token) {
            Some(Some(level)) => return Seen::Level(level, range),
            Some(None) => return Seen::Record,
            None => {
                if timestampish(token) {
                    stamped = true;
                } else if token != "npm" {
                    break;
                }
            }
        }
    }
    if stamped { Seen::Record } else { Seen::Plain }
}

/// One entry per line. A line that names a level starts a block; the lines
/// after it keep that level until a blank line or a new record.
pub fn levels(lines: &[String]) -> Vec<LineLevel> {
    let mut carry = Level::None;
    lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                carry = Level::None;
                return LineLevel::NONE;
            }
            match detect(line) {
                Seen::Level(level, range) => {
                    carry = level;
                    LineLevel {
                        level,
                        token: Some(range),
                    }
                }
                Seen::Record => {
                    carry = Level::None;
                    LineLevel::NONE
                }
                Seen::Plain => LineLevel {
                    level: carry,
                    token: None,
                },
            }
        })
        .collect()
}

/// `inner` clipped to `outer`, shifted so `outer.start` is zero.
pub fn clip(inner: Range<usize>, outer: Range<usize>) -> Option<Range<usize>> {
    let start = inner.start.max(outer.start);
    let end = inner.end.min(outer.end);
    (start < end).then(|| start - outer.start..end - outer.start)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| line.to_string()).collect()
    }

    fn shown(rows: &Rows, source: &[String]) -> Vec<String> {
        rows.rows
            .iter()
            .map(|row| source[row.line][row.start..row.end].to_string())
            .collect()
    }

    #[test]
    fn a_long_line_wraps_without_losing_text() {
        let source = lines(&["abcdefghijklmnopqrstuvwxyz0123456789"]);
        let rows = wrap(&source, 10);
        let parts = shown(&rows, &source);
        assert!(parts.len() > 3);
        assert_eq!(parts.concat(), source[0]);
        assert!(!rows.rows[0].cont);
        assert!(rows.rows[1..].iter().all(|row| row.cont));
        // Continuation rows are indented, so they hold two fewer cells.
        assert!(parts[1..parts.len() - 1].iter().all(|part| part.len() <= 8));
        assert!(parts[0].len() <= 10);
        let mut chars = 0;
        for (row, part) in rows.rows.iter().zip(&parts) {
            assert_eq!(row.first_char, chars);
            chars += part.chars().count();
        }
    }

    #[test]
    fn wrapping_prefers_a_slash_or_space_past_the_middle() {
        let source = lines(&["see /Users/dev/project/file.mjs now"]);
        let rows = wrap(&source, 20);
        let parts = shown(&rows, &source);
        assert_eq!(parts.concat(), source[0]);
        assert!(parts[0].ends_with('/') || parts[0].ends_with(' '));
    }

    #[test]
    fn a_short_line_is_one_row_and_blank_runs_collapse() {
        let source = lines(&["one", "", "", " ", "two", ""]);
        let rows = wrap(&source, 80);
        assert_eq!(
            rows.rows.iter().map(|row| row.line).collect::<Vec<_>>(),
            vec![0, 1, 4, 5]
        );
        assert_eq!(rows.first_row(2), 2);
        assert_eq!(rows.first_row(4), 2);
        assert_eq!(rows.rows_before(4), 2);
        assert_eq!(rows.first_row(99), 3);
    }

    #[test]
    fn a_wide_character_counts_two_cells() {
        let source = lines(&["漢字漢字漢字"]);
        let rows = wrap(&source, 8);
        let parts = shown(&rows, &source);
        assert_eq!(parts.concat(), source[0]);
        assert!(parts[0].chars().count() <= 4);
        assert_eq!(char_at_cell(&source[0], &rows.rows[0], 2.), 1);
        assert_eq!(char_at_cell(&source[0], &rows.rows[0], 100.), 4);
        assert_eq!(char_at_cell(&source[0], &rows.rows[1], 0.), 4);
    }

    #[test]
    fn a_warning_record_colours_its_continuation_until_a_blank_line() {
        let source = lines(&[
            "Warning: /path/to/file.mjs depends on 'x'",
            "For more info see: https://example.dev",
            "",
            "NX   Successfully ran target build",
            "2025-01-01 10:00:00 ERROR boom",
            "    at main (index.js:1)",
            "2025-01-01 10:00:01 INFO fine",
            "    detail",
            "npm ERR! code E404",
        ]);
        let found = levels(&source);
        let kinds: Vec<Level> = found.iter().map(|line| line.level).collect();
        assert_eq!(
            kinds,
            vec![
                Level::Warn,
                Level::Warn,
                Level::None,
                Level::None,
                Level::Error,
                Level::Error,
                Level::None,
                Level::None,
                Level::Error,
            ]
        );
        assert_eq!(found[0].token, Some(0..8));
        assert_eq!(found[1].token, None);
        assert_eq!(found[4].token.clone().map(|r| &source[4][r]), Some("ERROR"));
    }

    #[test]
    fn a_level_word_inside_a_sentence_is_not_a_level() {
        let found = levels(&lines(&[
            "the error rate is fine",
            "Compiling errors-crate v1",
        ]));
        assert!(found.iter().all(|line| line.level == Level::None));
    }

    #[test]
    fn clip_shifts_into_the_row() {
        assert_eq!(clip(5..9, 4..8), Some(1..4));
        assert_eq!(clip(0..3, 4..8), None);
    }
}
