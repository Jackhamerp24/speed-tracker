import Foundation

enum Format {
    /// "620 ms", "1.24 s", "12.4 s".
    static func duration(_ seconds: Double) -> (value: String, unit: String) {
        if seconds < 0.9995 { return (String(format: "%.0f", seconds * 1000), "ms") }
        if seconds < 10 { return (String(format: "%.2f", seconds), "s") }
        if seconds < 100 { return (String(format: "%.1f", seconds), "s") }
        return (String(format: "%.0f", seconds), "s")
    }

    static func durationText(_ seconds: Double?) -> String {
        guard let seconds else { return "–" }
        let parts = duration(seconds)
        return "\(parts.value) \(parts.unit)"
    }

    static func rate(_ tokensPerSecond: Double?) -> String {
        guard let tokensPerSecond else { return "–" }
        return String(format: tokensPerSecond >= 100 ? "%.0f" : "%.1f", tokensPerSecond)
    }

    /// For a number that changes several times a second: no decimals to flicker.
    static func wholeRate(_ tokensPerSecond: Double) -> String {
        String(format: tokensPerSecond >= 20 ? "%.0f" : "%.1f", tokensPerSecond)
    }

    static func tokens(_ count: Int) -> String {
        if count < 1000 { return "\(count)" }
        if count < 100_000 { return String(format: "%.1fk", Double(count) / 1000) }
        if count < 1_000_000 { return String(format: "%.0fk", Double(count) / 1000) }
        return String(format: "%.1fM", Double(count) / 1_000_000)
    }

    static func ago(_ date: Date, now: Date = Date()) -> String {
        let seconds = max(0, now.timeIntervalSince(date))
        if seconds < 5 { return "now" }
        if seconds < 60 { return "\(Int(seconds))s" }
        if seconds < 3600 { return "\(Int(seconds / 60))m" }
        if seconds < 86_400 { return "\(Int(seconds / 3600))h" }
        return "\(Int(seconds / 86_400))d"
    }
}
