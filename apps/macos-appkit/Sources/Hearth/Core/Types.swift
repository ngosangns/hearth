import Foundation

/// Result of a `hearth` CLI invocation. Mirrors apps/macos `CommandResult`.
struct CommandResult {
    let ok: Bool
    let exit: Int32?
    let stdout: String
    let stderr: String
    let json: [String: Any]?

    static func missingBinary() -> CommandResult {
        CommandResult(ok: false, exit: nil, stdout: "", stderr: "bundled hearth is missing or not executable", json: nil)
    }

    var visibleMessage: String {
        let text = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        return CommandResult.redact(text.isEmpty ? stdout.trimmingCharacters(in: .whitespacesAndNewlines) : text)
    }

    /// The last standalone JSON object wins — pretty documents and log noise are skipped.
    static func lastJson(_ stdout: String) -> [String: Any]? {
        let trimmed = stdout.trimmingCharacters(in: .whitespacesAndNewlines)
        if let whole = try? JSONSerialization.jsonObject(with: Data(trimmed.utf8)) as? [String: Any] {
            return whole
        }
        var decoded: [String: Any]?
        for line in stdout.split(whereSeparator: \.isNewline) {
            let row = line.trimmingCharacters(in: .whitespaces)
            guard row.hasPrefix("{"), let data = row.data(using: .utf8),
                  let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
            else { continue }
            decoded = object
        }
        return decoded
    }

    static func redact(_ text: String) -> String {
        text.replacingOccurrences(
            of: "(\"token\"\\s*:\\s*\")[^\"]+",
            with: "$1[redacted]",
            options: .regularExpression
        )
    }
}

/// Result of one manager HTTP call. Mirrors `ApiResult`.
struct ApiResult {
    let ok: Bool
    let status: Int?
    let json: [String: Any]?
    let unauthorized: Bool
    let message: String

    static let session = ApiResult(ok: false, status: nil, json: nil, unauthorized: true, message: "")

    static func transport(_ message: String) -> ApiResult {
        ApiResult(ok: false, status: nil, json: nil, unauthorized: false, message: message)
    }
}
