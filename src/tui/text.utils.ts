import { replaceTabs } from "@oh-my-pi/pi-tui";

// One pass over everything the renderer cannot place inside a single row:
// escape sequences (CSI, OSC, DCS/SOS/PM/APC strings, two-byte ESC forms), a
// stray or truncated ESC, and the remaining C0/DEL characters. Matching in one
// pass keeps a preserved sequence from being re-matched by a later rule.
const unsafeText = /\x1b(?:\[[\d;:<=>?]*[ -/]*[@-~]|\][\s\S]*?(?:\x07|\x1b\\|$)|[P^_X][\s\S]*?(?:\x1b\\|$)|[ -/][@-~]|[0-~])?|[\x00-\x08\x0a-\x1f\x7f]/g;
// Colour-only CSI: safe to keep because the renderer measures SGR as zero-width
// and closes every row with a reset.
const sgrSequence = /^\x1b\[[\d;:]*m$/;

/**
 * Make untrusted text safe to place inside a single viewport row.
 *
 * Service logs are raw terminal streams: they carry erase-display, cursor-home
 * and carriage returns, and stderr notices carry newlines. Emitted verbatim
 * those bytes are executed by the terminal mid-frame, so the rows after them
 * land at the wrong position and whole areas of the frame stay blank. Drop
 * every sequence that moves or erases, keep colour, and turn the rest into a
 * space so column alignment survives.
 *
 * infra's own copy of this tool was missing this fix entirely (its sanitizer regex never matched
 * the ESC byte 0x1b, so CSI/OSC escape sequences passed straight through) — this is one of the two
 * pending fixes the migration explicitly carries over.
 */
export function sanitizeTerminalText(text: string): string {
  const expanded = replaceTabs(text);
  if (!unsafeText.test(expanded)) {
    unsafeText.lastIndex = 0;
    return expanded;
  }
  unsafeText.lastIndex = 0;
  return expanded.replace(unsafeText, (match) => {
    if (sgrSequence.test(match)) return match;
    return match.charCodeAt(0) === 0x1b ? "" : " ";
  });
}
