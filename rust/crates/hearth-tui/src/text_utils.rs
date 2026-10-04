//! Port of `src/tui/text.utils.ts` plus the `visibleWidth`/`truncateToWidth` primitives from
//! `@oh-my-pi/pi-tui` that `src/tui/screen.ts` builds on. pi-tui's own versions of the latter two
//! delegate to a native (Rust) addon whose source isn't vendored into this checkout, so they are
//! reimplemented here from their observed behavior (see the module docs on `truncate_to_width` for
//! what was verified and what is a documented simplification).
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// pi-tui's `DEFAULT_TAB_WIDTH` could not be located anywhere in its published package (it's baked
/// into a native addon with no vendored source) — empirically confirmed instead: `bun -e 'import {
/// sanitizeTerminalText } from "./src/tui/text.utils"; console.log(JSON.stringify(sanitizeTerminalText("a\tb")))'`
/// from the repo root prints `"a   b"` (3 spaces), for every tab regardless of the column it starts
/// at (not a tab-stop calculation — confirmed by expanding tabs after prefixes of several lengths).
const TAB_WIDTH: usize = 3;

pub fn replace_tabs(text: &str) -> String {
    text.replace('\t', &" ".repeat(TAB_WIDTH))
}

const SGR_RESET: &str = "\x1b[0m";

/// Make untrusted text safe to place inside a single viewport row.
///
/// Service logs are raw terminal streams: they carry erase-display, cursor-home and carriage
/// returns, and stderr notices carry newlines. Emitted verbatim those bytes are executed by the
/// terminal mid-frame, so the rows after them land at the wrong position and whole areas of the
/// frame stay blank. Drop every sequence that moves or erases, keep colour, and turn the rest into
/// a space so column alignment survives.
pub fn sanitize_terminal_text(text: &str) -> String {
    let expanded = replace_tabs(text);
    let bytes = expanded.as_bytes();
    let mut out = String::with_capacity(expanded.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            let (consumed, is_sgr) = scan_escape(bytes, i);
            if is_sgr {
                out.push_str(&expanded[i..i + consumed]);
            }
            i += consumed;
        } else if b <= 0x08 || (0x0a..=0x1f).contains(&b) || b == 0x7f {
            out.push(' ');
            i += 1;
        } else {
            let start = i;
            i += 1;
            while i < bytes.len() && (bytes[i] & 0xc0) == 0x80 {
                i += 1;
            }
            out.push_str(&expanded[start..i]);
        }
    }
    out
}

/// Scans one escape sequence starting at `bytes[start]` (which must be `0x1b`). Returns the number
/// of bytes it consumes (including the leading ESC) and whether it is a colour-only SGR sequence
/// (`\x1b[<digits/;/:>*m`) — the one kind `sanitize_terminal_text` keeps. Mirrors the TS source's
/// single combined regex: CSI, OSC, DCS/SOS/PM/APC strings, the two-byte ESC form, a bare
/// single-byte form, and a stray/truncated trailing ESC.
fn scan_escape(bytes: &[u8], start: usize) -> (usize, bool) {
    let i = start + 1;
    if i >= bytes.len() {
        return (1, false);
    }
    match bytes[i] {
        b'[' => {
            let params_start = i + 1;
            let mut j = params_start;
            while j < bytes.len()
                && matches!(
                    bytes[j],
                    b'0'..=b'9' | b';' | b':' | b'<' | b'=' | b'>' | b'?'
                )
            {
                j += 1;
            }
            let params_end = j;
            while j < bytes.len() && (0x20..=0x2f).contains(&bytes[j]) {
                j += 1;
            }
            let intermediates_end = j;
            if j < bytes.len() && (0x40..=0x7e).contains(&bytes[j]) {
                let final_byte = bytes[j];
                let is_sgr = final_byte == b'm'
                    && intermediates_end == params_end
                    && bytes[params_start..params_end]
                        .iter()
                        .all(|c| matches!(c, b'0'..=b'9' | b';' | b':'));
                return (j + 1 - start, is_sgr);
            }
            (1, false)
        }
        b']' => scan_terminated_by_bel_or_st(bytes, i + 1, start),
        b'P' | b'^' | b'_' | b'X' => scan_terminated_by_st_only(bytes, i + 1, start),
        c if (0x20..=0x2f).contains(&c) => {
            if i + 1 < bytes.len() && (0x40..=0x7e).contains(&bytes[i + 1]) {
                (i + 2 - start, false)
            } else {
                (1, false)
            }
        }
        c if (0x30..=0x7e).contains(&c) => (i + 1 - start, false),
        _ => (1, false),
    }
}

fn scan_terminated_by_bel_or_st(bytes: &[u8], mut j: usize, start: usize) -> (usize, bool) {
    loop {
        if j >= bytes.len() {
            return (j - start, false);
        }
        if bytes[j] == 0x07 {
            return (j + 1 - start, false);
        }
        if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
            return (j + 2 - start, false);
        }
        j += 1;
    }
}

fn scan_terminated_by_st_only(bytes: &[u8], mut j: usize, start: usize) -> (usize, bool) {
    loop {
        if j >= bytes.len() {
            return (j - start, false);
        }
        if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
            return (j + 2 - start, false);
        }
        j += 1;
    }
}

/// Visible width of a string in terminal columns: every SGR/other escape sequence counts as zero
/// cells, wide (e.g. CJK) characters count as 2, combining marks as 0, everything else as 1 —
/// matching `unicode-width`'s default (ambiguous-width East Asian characters treated as narrow),
/// which is also what pi-tui pins its native engine to.
pub fn visible_width(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut width = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            let (consumed, _) = scan_escape(bytes, i);
            i += consumed;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        width += UnicodeWidthChar::width(ch).unwrap_or(0);
        i += ch.len_utf8();
    }
    width
}

fn is_reset_sgr(seq: &str) -> bool {
    let params = &seq[2..seq.len() - 1];
    params.is_empty() || params == "0"
}

/// Truncates `text` to at most `max_width` visible columns (never splitting a wide character in
/// half), optionally padding with spaces up to exactly `max_width`. No ellipsis is ever appended
/// (every call site in this codebase passes pi-tui's `Ellipsis.Omit`, so that parameter is dropped
/// here rather than ported).
///
/// **Documented simplification**: pi-tui's real `truncateToWidth` delegates to a native addon whose
/// source isn't vendored in this checkout, so its exact behaviour was reverse-engineered by probing
/// the compiled function directly (`bun -e 'import { truncateToWidth } from "@oh-my-pi/pi-tui"; ...'`)
/// rather than read from source. The rule this reproduces, confirmed against roughly a dozen probed
/// inputs: an open (non-reset) SGR sequence encountered exactly at the width cap is only kept if a
/// colour is already open at that point (closing it), and a synthetic trailing `\x1b[0m` is appended
/// when truncation actually dropped content, at least one SGR sequence survived into the kept
/// output, and that output doesn't already end with one. This matches every probed case, but the
/// native engine may special-case inputs not covered by that probing (e.g. nested/nonstandard SGR
/// forms) — none of the ported TUI tests exercise the ambiguous cases, so this gap is unlikely to
/// matter in practice for real service log output.
pub fn truncate_to_width(text: &str, max_width: usize, pad: bool) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut width = 0usize;
    let mut i = 0usize;
    let mut truncated = false;
    let mut saw_sgr = false;
    let mut color_open = false;
    let mut at_cap = false;

    while i < bytes.len() {
        if bytes[i] == 0x1b {
            let (consumed, is_sgr) = scan_escape(bytes, i);
            let seq = &text[i..i + consumed];
            if is_sgr {
                if at_cap && !color_open {
                    // Drop: an unclosed colour-open (or a redundant reset) right at the cap, with
                    // nothing open to close, contributes nothing the caller would see.
                } else {
                    out.push_str(seq);
                    saw_sgr = true;
                    color_open = !is_reset_sgr(seq);
                }
            } else {
                out.push_str(seq);
            }
            i += consumed;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > max_width {
            truncated = true;
            break;
        }
        out.push(ch);
        width += w;
        i += ch.len_utf8();
        if width == max_width {
            at_cap = true;
        }
    }
    if i < bytes.len() && !truncated {
        truncated = true;
    }
    if truncated && saw_sgr && !out.ends_with(SGR_RESET) {
        out.push_str(SGR_RESET);
    }
    if pad && width < max_width {
        out.push_str(&" ".repeat(max_width - width));
    }
    out
}

/// Turn sanitized SGR into Ratatui spans. Cursor, erase, and other non-colour sequences are
/// dropped by [`sanitize_terminal_text`] first, so a log line cannot move the cursor.
pub fn sgr_to_line(text: &str) -> Line<'static> {
    let sanitized = sanitize_terminal_text(text);
    let bytes = sanitized.as_bytes();
    let mut spans = Vec::new();
    let mut style = Style::default();
    let mut buf = String::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            push_span(&mut spans, &mut buf, style);
            let (consumed, is_sgr) = scan_escape(bytes, index);
            if is_sgr {
                let seq = &sanitized[index..index + consumed];
                style = apply_sgr(style, &seq[2..seq.len() - 1]);
            }
            index += consumed;
            continue;
        }
        let ch = sanitized[index..].chars().next().unwrap();
        buf.push(ch);
        index += ch.len_utf8();
    }
    push_span(&mut spans, &mut buf, style);
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    Line::from(spans)
}

fn push_span(spans: &mut Vec<Span<'static>>, buf: &mut String, style: Style) {
    if buf.is_empty() {
        return;
    }
    spans.push(Span::styled(std::mem::take(buf), style));
}

fn apply_sgr(mut style: Style, params: &str) -> Style {
    if params.is_empty() {
        return Style::default();
    }
    let parts: Vec<&str> = params.split([';', ':']).collect();
    let mut index = 0usize;
    while index < parts.len() {
        let Ok(code) = parts[index].parse::<i32>() else {
            index += 1;
            continue;
        };
        index += 1;
        match code {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            39 => style.fg = None,
            49 => style.bg = None,
            30..=37 => style.fg = Some(ansi_color((code - 30) as u8, false)),
            40..=47 => style.bg = Some(ansi_color((code - 40) as u8, false)),
            90..=97 => style.fg = Some(ansi_color((code - 90) as u8, true)),
            100..=107 => style.bg = Some(ansi_color((code - 100) as u8, true)),
            38 | 48 => style = apply_extended(style, code == 38, &parts, &mut index),
            _ => {}
        }
    }
    style
}

fn apply_extended(mut style: Style, foreground: bool, parts: &[&str], index: &mut usize) -> Style {
    if *index >= parts.len() {
        return style;
    }
    let mode = parts[*index];
    *index += 1;
    let color = if mode == "5" {
        let Some(value) = parts.get(*index).and_then(|part| part.parse::<u8>().ok()) else {
            return style;
        };
        *index += 1;
        Color::Indexed(value)
    } else if mode == "2" {
        if parts.get(*index).is_some_and(|part| part.is_empty()) {
            *index += 1;
        }
        let Some(red) = parts.get(*index).and_then(|part| part.parse::<u8>().ok()) else {
            return style;
        };
        let Some(green) = parts
            .get(*index + 1)
            .and_then(|part| part.parse::<u8>().ok())
        else {
            return style;
        };
        let Some(blue) = parts
            .get(*index + 2)
            .and_then(|part| part.parse::<u8>().ok())
        else {
            return style;
        };
        *index += 3;
        Color::Rgb(red, green, blue)
    } else {
        return style;
    };
    if foreground {
        style.fg = Some(color);
    } else {
        style.bg = Some(color);
    }
    style
}

fn ansi_color(index: u8, bright: bool) -> Color {
    match (index, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        _ => Color::White,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_erase_display_and_cursor_home_sequences_that_would_move_the_paint_cursor() {
        assert_eq!(
            sanitize_terminal_text("\x1b[2J\x1b[3J\x1b[Hcompiling"),
            "compiling"
        );
    }

    #[test]
    fn keeps_colour_sequences_so_log_severity_stays_visible() {
        assert_eq!(
            sanitize_terminal_text("\x1b[90m1:53 PM\x1b[0m ready"),
            "\x1b[90m1:53 PM\x1b[0m ready"
        );
    }

    #[test]
    fn drops_cursor_movement_while_keeping_the_surrounding_colour() {
        assert_eq!(
            sanitize_terminal_text("\x1b[32mok\x1b[1G\x1b[0K\x1b[0m"),
            "\x1b[32mok\x1b[0m"
        );
    }

    #[test]
    fn replaces_carriage_returns_with_a_space_instead_of_restarting_the_row() {
        assert_eq!(
            sanitize_terminal_text("node compile.js\r"),
            "node compile.js "
        );
    }

    #[test]
    fn replaces_an_embedded_newline_so_one_log_line_cannot_become_two_rows() {
        assert_eq!(sanitize_terminal_text("first\nsecond"), "first second");
    }

    #[test]
    fn drops_osc_dcs_and_apc_strings_including_unterminated_ones() {
        assert_eq!(sanitize_terminal_text("\x1b]0;title\x07after"), "after");
        assert_eq!(sanitize_terminal_text("\x1bPtmux;x\x1b\\after"), "after");
        assert_eq!(sanitize_terminal_text("before\x1b]0;truncated"), "before");
    }

    #[test]
    fn drops_a_trailing_escape_left_by_a_truncated_write() {
        assert_eq!(sanitize_terminal_text("partial\x1b"), "partial");
    }

    #[test]
    fn expands_tabs_so_columns_match_the_width_the_renderer_measures() {
        assert_eq!(sanitize_terminal_text("a\tb"), "a   b");
    }

    #[test]
    fn returns_plain_text_unchanged() {
        assert_eq!(sanitize_terminal_text("Found 0 errors."), "Found 0 errors.");
    }

    #[test]
    fn visible_width_counts_wide_characters_and_skips_escapes() {
        assert_eq!(visible_width("hello"), 5);
        assert_eq!(visible_width("你好"), 4);
        assert_eq!(visible_width("\x1b[31mhi\x1b[0m"), 2);
    }

    #[test]
    fn truncate_to_width_pads_and_cuts_without_splitting_wide_characters() {
        assert_eq!(truncate_to_width("hello world", 5, true), "hello");
        assert_eq!(truncate_to_width("hi", 5, true), "hi   ");
        assert_eq!(truncate_to_width("你好世界", 4, true), "你好");
        assert_eq!(truncate_to_width("你好世界", 3, true), "你 ");
    }

    #[test]
    fn sgr_colours_become_spans_and_cursor_controls_stay_dropped() {
        let failed = sgr_to_line("\x1b[1;31mfail\x1b[0m");
        assert_eq!(failed.spans[0].content.as_ref(), "fail");
        assert_eq!(failed.spans[0].style.fg, Some(Color::Red));
        assert!(failed.spans[0].style.add_modifier.contains(Modifier::BOLD));
        let dimmed = sgr_to_line("\x1b[90m1:53 PM\x1b[0m ready");
        assert_eq!(dimmed.spans[0].style.fg, Some(Color::DarkGray));
        assert_eq!(dimmed.spans[1].content.as_ref(), " ready");
        let kept = sgr_to_line("\x1b[32mok\x1b[1G\x1b[0K\x1b[0m");
        assert_eq!(kept.spans[0].content.as_ref(), "ok");
        assert_eq!(kept.spans[0].style.fg, Some(Color::Green));
        let indexed = sgr_to_line("\x1b[38;5;196mhi\x1b[0m");
        assert_eq!(indexed.spans[0].style.fg, Some(Color::Indexed(196)));
    }

    #[test]
    fn truncate_to_width_manages_open_colour_across_a_cut() {
        assert_eq!(
            truncate_to_width("\x1b[31mhello world", 7, true),
            "\x1b[31mhello w\x1b[0m"
        );
        assert_eq!(
            truncate_to_width("\x1b[31mhello\x1b[0m world", 7, true),
            "\x1b[31mhello\x1b[0m w\x1b[0m"
        );
        assert_eq!(
            truncate_to_width("\x1b[31mhi\x1b[0mxx", 2, true),
            "\x1b[31mhi\x1b[0m"
        );
        assert_eq!(truncate_to_width("hi\x1b[0m more", 2, true), "hi");
    }
}
