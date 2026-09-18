import { describe, expect, test } from "bun:test";

import { sanitizeTerminalText } from "../../src/tui/text.utils";

describe("sanitizeTerminalText", () => {
  test("drops erase-display and cursor-home sequences that would move the paint cursor", () => {
    expect(sanitizeTerminalText("\x1b[2J\x1b[3J\x1b[Hcompiling")).toBe("compiling");
  });

  test("keeps colour sequences so log severity stays visible", () => {
    expect(sanitizeTerminalText("\x1b[90m1:53 PM\x1b[0m ready")).toBe("\x1b[90m1:53 PM\x1b[0m ready");
  });

  test("drops cursor movement while keeping the surrounding colour", () => {
    expect(sanitizeTerminalText("\x1b[32mok\x1b[1G\x1b[0K\x1b[0m")).toBe("\x1b[32mok\x1b[0m");
  });

  test("replaces carriage returns with a space instead of restarting the row", () => {
    expect(sanitizeTerminalText("node compile.js\r")).toBe("node compile.js ");
  });

  test("replaces an embedded newline so one log line cannot become two rows", () => {
    expect(sanitizeTerminalText("first\nsecond")).toBe("first second");
  });

  test("drops osc, dcs and apc strings including unterminated ones", () => {
    expect(sanitizeTerminalText("\x1b]0;title\x07after")).toBe("after");
    expect(sanitizeTerminalText("\x1bPtmux;x\x1b\\after")).toBe("after");
    expect(sanitizeTerminalText("before\x1b]0;truncated")).toBe("before");
  });

  test("drops a trailing escape left by a truncated write", () => {
    expect(sanitizeTerminalText("partial\x1b")).toBe("partial");
  });

  test("expands tabs so columns match the width the renderer measures", () => {
    expect(sanitizeTerminalText("a\tb")).toBe("a   b");
  });

  test("returns plain text unchanged", () => {
    expect(sanitizeTerminalText("Found 0 errors.")).toBe("Found 0 errors.");
  });
});
