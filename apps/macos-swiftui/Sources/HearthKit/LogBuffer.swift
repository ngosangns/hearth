import Foundation

/// Rolling text for the log column: one service at a time.
///
/// `generation` is `lifecycle * 1_000_000 + rotation`, so echoing it back lets the daemon reset the
/// cursor on a restart or a rotation. `reset: true` replaces the text with the slice.
public struct LogBuffer: Sendable, Equatable {
    public static let defaultLimit = 16_384
    public static let maxLimit = 262_144

    public private(set) var text = ""
    public private(set) var cursor: Int?
    public private(set) var generation: Int?
    public private(set) var limit = LogBuffer.defaultLimit
    /// An earlier, larger window is available.
    public private(set) var hasMore = false
    /// Bumps on every change so a view can skip an equal-text redraw.
    public private(set) var revision = 0
    private var liveGeneration: Int?

    public init() {}

    /// The text without terminal escape sequences (SGR colours, cursor moves, OSC titles), which
    /// a plain text view would draw as garbage.
    public var plain: String { Self.stripEscapes(text) }

    private static let escapes = try! NSRegularExpression(
        pattern: "\u{1B}\\[[0-?]*[ -/]*[@-~]|\u{1B}\\][^\u{07}\u{1B}]*(?:\u{07}|\u{1B}\\\\)|\u{1B}[@-Z\\\\-_]")

    public static func stripEscapes(_ text: String) -> String {
        guard text.contains("\u{1B}") else { return text }
        let range = NSRange(text.startIndex..., in: text)
        return escapes.stringByReplacingMatches(in: text, range: range, withTemplate: "")
    }

    /// A different service was selected, or the daemon went away.
    public mutating func reset() {
        self = LogBuffer(revision: revision + 1)
    }

    private init(revision: Int) { self.revision = revision }

    public mutating func apply(_ slice: LogSlice) {
        let reset = slice.reset ?? false
        if reset {
            hasMore = slice.data.count >= limit - 16 && limit < Self.maxLimit
        }
        let merged = (reset || text.isEmpty) ? slice.data : text + slice.data
        let bounded = ServiceBoard.boundedTail(merged, limit: Self.maxLimit)
        if bounded != text {
            text = bounded
            revision += 1
        }
        if let next = slice.nextCursor { cursor = next }
        if let generation = slice.generation { self.generation = generation }
    }

    /// Grow the window 4x (up to `maxLimit`) and refetch from scratch. False when already maximal
    /// or nothing earlier exists.
    @discardableResult
    public mutating func expand() -> Bool {
        guard hasMore else { return false }
        limit = min(limit * 4, Self.maxLimit)
        text = ""
        cursor = nil
        generation = nil
        hasMore = false
        revision += 1
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
