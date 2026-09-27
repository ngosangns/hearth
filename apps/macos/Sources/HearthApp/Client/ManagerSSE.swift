import Foundation

enum ManagerStreamEvent: Equatable {
    case replay(epoch: String, reset: Bool, latestSequence: UInt64)
    case manager(sequence: UInt64, type: String)
}

enum SSEParser {
    static let maxFrameBytes = 65_536

    static func parseFrame(_ frame: String) -> ManagerStreamEvent? {
        guard frame.utf8.count <= maxFrameBytes else { return nil }
        let eventType = frame.split(whereSeparator: \.isNewline)
            .first(where: { $0.hasPrefix("event:") })
            .map { $0.dropFirst("event:".count).trimmingCharacters(in: .whitespaces) }
            ?? "message"
        let raw = frame.split(whereSeparator: \.isNewline)
            .filter { $0.hasPrefix("data:") }
            .map { $0.dropFirst("data:".count).trimmingCharacters(in: .whitespaces) }
            .joined(separator: "\n")
        guard !raw.isEmpty, let data = raw.data(using: .utf8) else { return nil }
        if eventType == "replay" {
            guard let replay = try? JSONDecoder().decode(EventReplayDTO.self, from: data) else { return nil }
            return .replay(epoch: replay.epoch, reset: replay.reset, latestSequence: replay.latestSequence)
        }
        guard let event = try? JSONDecoder().decode(ManagerEventDTO.self, from: data) else { return nil }
        return .manager(sequence: event.sequence, type: event.type)
    }
}

/// Splits a raw SSE byte stream into frames — the lines between two blank lines, joined by `\n`.
///
/// Fed byte by byte rather than from `AsyncBytes.lines`: `.lines` drops empty lines, and the blank
/// line IS the frame delimiter, so a line-based reader never saw a frame end and no event was ever
/// delivered. Any of `\n`, `\r\n` or a lone `\r` ends a line, per the SSE spec.
struct SSEFrameSplitter {
    struct FrameTooLarge: Error {}

    private var line: [UInt8] = []
    private var frame: [UInt8] = []
    private var afterCR = false

    /// Returns a completed frame when `byte` finishes one. Comment-only (`: keepalive`) and empty
    /// frames are returned too — `SSEParser.parseFrame` answers `nil` for them.
    mutating func push(_ byte: UInt8) throws -> String? {
        let wasCR = afterCR
        afterCR = false
        switch byte {
        case UInt8(ascii: "\n") where wasCR:
            return nil // second half of a `\r\n` already handled at the `\r`
        case UInt8(ascii: "\r"):
            afterCR = true
            return endLine()
        case UInt8(ascii: "\n"):
            return endLine()
        default:
            line.append(byte)
            if line.count + frame.count > SSEParser.maxFrameBytes { throw FrameTooLarge() }
            return nil
        }
    }

    private mutating func endLine() -> String? {
        guard line.isEmpty else {
            if !frame.isEmpty { frame.append(UInt8(ascii: "\n")) }
            frame.append(contentsOf: line)
            line.removeAll(keepingCapacity: true)
            return nil
        }
        guard !frame.isEmpty else { return nil }
        defer { frame.removeAll(keepingCapacity: true) }
        return String(decoding: frame, as: UTF8.self)
    }
}

private struct EventReplayDTO: Decodable {
    let epoch: String
    let reset: Bool
    let latestSequence: UInt64
}

private struct ManagerEventDTO: Decodable {
    let sequence: UInt64
    let type: String
}
