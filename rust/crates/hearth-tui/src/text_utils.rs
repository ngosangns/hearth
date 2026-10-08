//! Width and sanitizing for log lines painted by the Ratatui shell.
//! `visible_width` and the 3-space tab expansion were checked against pi-tui's observed
//! output; colour SGR is kept, cursor and erase sequences are dropped.
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

/// Turn sanitized SGR into a Ratatui line. Cursor, erase, and other non-colour sequences are
/// dropped by [`sanitize_terminal_text`] first, so a log line cannot move the cursor; the SGR
/// parsing itself is `ansi-to-tui`'s.
pub fn sgr_to_line(text: &str) -> Line<'static> {
    use ansi_to_tui::IntoText;
    sanitize_terminal_text(text)
        .into_text()
        .ok()
        .and_then(|text| text.lines.into_iter().next())
        .unwrap_or_else(|| Line::from(vec![Span::raw("")]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

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

}
