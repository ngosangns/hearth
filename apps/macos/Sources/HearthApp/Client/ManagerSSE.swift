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

    /// Pulls complete `\\n\\n`-delimited frames off `buffer`, returning them and the remainder.
    static func takeFrames(from buffer: inout String) -> [String] {
        var frames: [String] = []
        while let range = buffer.range(of: "\n\n") {
            frames.append(String(buffer[buffer.startIndex..<range.lowerBound]))
            buffer = String(buffer[range.upperBound...])
        }
        return frames
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
