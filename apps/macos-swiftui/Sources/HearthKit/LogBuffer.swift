import Foundation

/// Rolling text for the log column: one service at a time.
///
/// The first read is the short tail (`defaultLimit`). `hasMore` means the file still has bytes
/// before `origin`. `install` splices a wider no-cursor window onto that tail instead of blanking
/// it. `generation` is `lifecycle * 1_000_000 + rotation`, so echoing it back lets the daemon reset
/// the cursor on a restart or a rotation. `reset: true` replaces the text with the slice.
public struct LogBuffer: Sendable, Equatable {
    public static let defaultLimit = 16_384
    public static let maxLimit = 262_144

    public private(set) var text = ""
    /// Follow point: the next byte to append. Sent back as `cursor` on a tail poll.
    public private(set) var cursor: Int?
    public private(set) var generation: Int?
    /// Byte size of a tail poll. An earlier page asks for `nextWindowLimit()` instead.
    public private(set) var limit = LogBuffer.defaultLimit
    /// An earlier page exists in the current file and the retained window is under `maxLimit`.
    public private(set) var hasMore = false
    /// UTF-16 units added at the front by the last `install`. The pane adds this to its scroll anchor.
    public private(set) var prepended = 0
    /// Bumps on every text change so a view can skip an equal-text redraw.
    public private(set) var revision = 0
    /// Byte offset of `text`'s first byte. Nil when this buffer has no file mapping.
    private var origin: Int?
    private var liveGeneration: Int?

    public init() {}
    /// The text without terminal escape sequences. Copy uses this; the pane paints [`colored`].
    public var plain: String { Self.stripEscapes(text) }

    /// SGR colour runs for the pane. Cursor, erase, and OSC sequences are dropped, not drawn.
    public var colored: [LogRun] { Self.colorRuns(text) }

    private static let escapes = try! NSRegularExpression(
        pattern: "\u{1B}\\[[0-?]*[ -/]*[@-~]|\u{1B}\\][^\u{07}\u{1B}]*(?:\u{07}|\u{1B}\\\\)|\u{1B}[@-Z\\\\-_]")

    public static func stripEscapes(_ text: String) -> String {
        guard text.contains("\u{1B}") else { return text }
        let range = NSRange(text.startIndex..., in: text)
        return escapes.stringByReplacingMatches(in: text, range: range, withTemplate: "")
    }

    /// One painted run. `fg`/`bg` are ANSI indexes 0–15, or 256-color 0–255 when `indexed` is set.
    /// Nil means the terminal default.
    public struct LogRun: Sendable, Equatable {
        public var text: String
        public var fg: Int?
        public var bg: Int?
        public var indexed: Bool
        public var bold: Bool
        public var dim: Bool
        public var italic: Bool
        public var underline: Bool
    }

    public static func colorRuns(_ text: String) -> [LogRun] {
        guard text.contains("\u{1B}") else {
            return text.isEmpty ? [] : [LogRun(text: text, fg: nil, bg: nil, indexed: false, bold: false, dim: false, italic: false, underline: false)]
        }
        var style = Style()
        var runs: [LogRun] = []
        var chunk = ""
        var i = text.startIndex
        func flush() {
            guard !chunk.isEmpty else { return }
            runs.append(LogRun(text: chunk, fg: style.fg, bg: style.bg, indexed: style.indexed, bold: style.bold, dim: style.dim, italic: style.italic, underline: style.underline))
            chunk = ""
        }
        while i < text.endIndex {
            if text[i] == "\u{1B}" {
                let (next, sgr) = scanEscape(text, from: i)
                if let sgr {
                    let nextStyle = style.applying(sgr)
                    if nextStyle != style { flush(); style = nextStyle }
                }
                i = next
            } else {
                chunk.append(text[i])
                i = text.index(after: i)
            }
        }
        flush()
        return runs
    }

    private struct Style: Equatable {
        var fg: Int?
        var bg: Int?
        var indexed = false
        var bold = false
        var dim = false
        var italic = false
        var underline = false

        func applying(_ params: String) -> Style {
            var next = self
            let parts = params.split(separator: ";", omittingEmptySubsequences: false).map { Int($0) ?? 0 }
            var i = 0
            if parts.isEmpty { return Style() }
            while i < parts.count {
                let code = parts[i]
                switch code {
                case 0: next = Style()
                case 1: next.bold = true
                case 2: next.dim = true
                case 3: next.italic = true
                case 4: next.underline = true
                case 22: next.bold = false; next.dim = false
                case 23: next.italic = false
                case 24: next.underline = false
                case 39: next.fg = nil
                case 49: next.bg = nil
                case 30...37: next.fg = code - 30; next.indexed = false
                case 40...47: next.bg = code - 40
                case 90...97: next.fg = code - 90 + 8; next.indexed = false
                case 100...107: next.bg = code - 100 + 8
                case 38, 48:
                    let isFg = code == 38
                    if i + 1 < parts.count, parts[i + 1] == 5, i + 2 < parts.count {
                        if isFg { next.fg = parts[i + 2]; next.indexed = true } else { next.bg = parts[i + 2] }
                        i += 2
                    } else if i + 1 < parts.count, parts[i + 1] == 2, i + 4 < parts.count {
                        // Truecolor is stored as a negative sentinel; the view reads the following
                        // bytes from the original params. Fall back to the nearest 256-color cube.
                        let r = parts[i + 2], g = parts[i + 3], b = parts[i + 4]
                        let cube = 16 + 36 * (r * 5 / 255) + 6 * (g * 5 / 255) + (b * 5 / 255)
                        if isFg { next.fg = cube; next.indexed = true } else { next.bg = cube }
                        i += 4
                    }
                default: break
                }
                i += 1
            }
            return next
        }
    }

    /// Returns the index after the sequence, and the SGR parameter string when it is a colour
    /// sequence (`ESC [ … m`). Anything else is consumed and dropped.
    private static func scanEscape(_ text: String, from start: String.Index) -> (String.Index, String?) {
        let bytes = Array(text.utf8)
        let offset = text.utf8.distance(from: text.startIndex, to: start)
        let i = offset + 1
        guard i < bytes.count else { return (text.endIndex, nil) }
        if bytes[i] == UInt8(ascii: "[") {
            var j = i + 1
            let paramsStart = j
            while j < bytes.count, isParam(bytes[j]) { j += 1 }
            let paramsEnd = j
            while j < bytes.count, (0x20...0x2f).contains(bytes[j]) { j += 1 }
            guard j < bytes.count, (0x40...0x7e).contains(bytes[j]) else { return (text.index(after: start), nil) }
            let end = text.utf8.index(text.startIndex, offsetBy: j + 1)
            if bytes[j] == UInt8(ascii: "m"), j == paramsEnd {
                let params = String(decoding: bytes[paramsStart..<paramsEnd], as: UTF8.self)
                return (end, params)
            }
            return (end, nil)
        }
        if bytes[i] == UInt8(ascii: "]") {
            var j = i + 1
            while j < bytes.count {
                if bytes[j] == 0x07 { return (text.utf8.index(text.startIndex, offsetBy: j + 1), nil) }
                if bytes[j] == 0x1b, j + 1 < bytes.count, bytes[j + 1] == UInt8(ascii: "\\") {
                    return (text.utf8.index(text.startIndex, offsetBy: j + 2), nil)
                }
                j += 1
            }
            return (text.endIndex, nil)
        }
        return (text.index(text.startIndex, offsetBy: min(i + 1, bytes.count)), nil)
    }

    private static func isParam(_ byte: UInt8) -> Bool {
        (0x30...0x39).contains(byte) || byte == UInt8(ascii: ";") || byte == UInt8(ascii: ":")
    }

    /// A different service was selected, or the daemon went away.
    public mutating func reset() {
        self = LogBuffer(revision: revision + 1)
    }

    private init(revision: Int) { self.revision = revision }

    /// A tail poll. `reset` or an empty buffer installs the slice as the whole window. Otherwise
    /// `data` is the bytes after `cursor` and is appended.
    public mutating func apply(_ slice: LogSlice) {
        prepended = 0
        if slice.reset == true || text.isEmpty {
            replace(slice)
            return
        }
        let (kept, dropped) = Self.boundedBytes(text + slice.data, limit: Self.maxLimit)
        if dropped > 0, let origin {
            self.origin = origin + dropped
            hasMore = kept.utf8.count < Self.maxLimit
        }
        if kept != text {
            text = kept
            revision += 1
        }
        if let next = slice.nextCursor { cursor = next }
        if let generation = slice.generation { self.generation = generation }
    }

    /// A no-cursor read of a wider tail. Prepends the bytes before `origin` and appends anything
    /// that landed past the follow point. A window that does not move `origin` while `truncated`
    /// is still set stops paging — the server handed back the same start.
    public mutating func install(_ slice: LogSlice) {
        let before = origin
        prepended = 0
        guard absorb(slice) else {
            replace(slice)
            return
        }
        if let before, let origin, origin >= before, slice.truncated == true {
            hasMore = false
        }
    }

    /// The next no-cursor `limit`: one page before the bytes already held, capped at `maxLimit`.
    /// Nil when there is nothing earlier to ask for.
    public func nextWindowLimit() -> Int? {
        guard hasMore else { return nil }
        let held = text.utf8.count
        guard held < Self.maxLimit else { return nil }
        let next = min(held + Self.defaultLimit, Self.maxLimit)
        return next > held ? next : nil
    }

    /// `limit` counts bytes. The cut snaps forward to a UTF-8 boundary. The dropped count is the
    /// byte distance a later `install` adds to `origin`.
    static func boundedBytes(_ text: String, limit: Int) -> (String, Int) {
        guard limit >= 1 else { return ("", text.utf8.count) }
        let bytes = Array(text.utf8)
        if bytes.count <= limit { return (text, 0) }
        var start = bytes.count - limit
        while start < bytes.count, isContinuation(bytes[start]) { start += 1 }
        if start >= bytes.count { return ("", bytes.count) }
        return (String(decoding: bytes[start...], as: UTF8.self), start)
    }

    private static func isContinuation(_ byte: UInt8) -> Bool {
        (byte & 0b1100_0000) == 0b1000_0000
    }

    private mutating func replace(_ slice: LogSlice) {
        let (kept, dropped) = Self.boundedBytes(slice.data, limit: Self.maxLimit)
        if kept != text {
            text = kept
            revision += 1
        }
        origin = slice.cursor.map { $0 + dropped }
        if let next = slice.nextCursor { cursor = next }
        if let generation = slice.generation { self.generation = generation }
        hasMore = slice.truncated == true && text.utf8.count < Self.maxLimit && dropped == 0
    }

    /// Splices `slice` when it covers the retained bytes. False leaves the buffer untouched.
    private mutating func absorb(_ slice: LogSlice) -> Bool {
        guard slice.reset != true,
              let origin = self.origin,
              let end = self.cursor,
              let newStart = slice.cursor,
              let newEnd = slice.nextCursor,
              slice.generation == generation,
              newStart <= origin,
              newEnd >= end,
              text.utf8.count == end - origin
        else { return false }
        let bytes = Array(slice.data.utf8)
        let prefixLen = origin - newStart
        let midLen = end - origin
        guard prefixLen >= 0, midLen >= 0, prefixLen + midLen <= bytes.count else { return false }
        let mid = String(decoding: bytes[prefixLen..<(prefixLen + midLen)], as: UTF8.self)
        guard mid == text else { return false }
        let prefix = String(decoding: bytes[..<prefixLen], as: UTF8.self)
        let tail = String(decoding: bytes[(prefixLen + midLen)...], as: UTF8.self)
        let merged = prefix + text + tail
        let (kept, dropped) = Self.boundedBytes(merged, limit: Self.maxLimit)
        if dropped > 0 {
            prepended = 0
            if kept != text {
                text = kept
                revision += 1
            }
            self.origin = newStart + dropped
            cursor = newEnd
            if let generation = slice.generation { self.generation = generation }
            hasMore = false
            return true
        }
        prepended = prefix.utf16.count
        if merged != text {
            text = merged
            revision += 1
        }
        self.origin = newStart
        cursor = newEnd
        if let generation = slice.generation { self.generation = generation }
        hasMore = slice.truncated == true && text.utf8.count < Self.maxLimit
        return true
    }

    /// Feed the service's current lifecycle generation from `/v1/services`. A change after the first
    /// observation invalidates the cursor so the next fetch starts a fresh run.
    public mutating func observe(liveGeneration next: Int?) {
        if liveGeneration != nil && next != liveGeneration {
            cursor = nil
            generation = nil
        }
        liveGeneration = next
    }
}
