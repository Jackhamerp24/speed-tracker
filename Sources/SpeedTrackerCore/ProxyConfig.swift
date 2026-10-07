import Foundation

/// Where a request path leads. `http://127.0.0.1:<port>/<route>/<rest>` is
/// forwarded to `<upstream of route>/<rest>`.
public struct ProxyConfig: Sendable {
    public struct Resolved: Equatable, Sendable {
        public var route: String
        /// Harness named explicitly in the URL as `<route>@<harness>`.
        public var harnessTag: String?
        public var url: URL
    }

    public static let defaultPort: UInt16 = 4141

    public static let defaultRoutes: [String: String] = [
        "anthropic": "https://api.anthropic.com",
        "openai": "https://api.openai.com",
        "chatgpt": "https://chatgpt.com/backend-api/codex",
        "deepseek": "https://api.deepseek.com",
        "openrouter": "https://openrouter.ai/api",
        "gemini": "https://generativelanguage.googleapis.com",
        "xai": "https://api.x.ai",
        "groq": "https://api.groq.com/openai",
        "cerebras": "https://api.cerebras.ai",
        "mistral": "https://api.mistral.ai",
        "moonshot": "https://api.moonshot.ai",
        "zai": "https://api.z.ai/api",
        "ollama": "http://127.0.0.1:11434",
        "lmstudio": "http://127.0.0.1:1234",
    ]

    public var port: UInt16
    public var routes: [String: String]

    public init(port: UInt16 = ProxyConfig.defaultPort, routes: [String: String] = ProxyConfig.defaultRoutes) {
        self.port = port
        self.routes = routes
    }

    /// `target` is the raw request target, e.g. `/anthropic/v1/messages?beta=true`.
    public func resolve(target: String) -> Resolved? {
        guard target.hasPrefix("/") else { return nil }
        let trimmed = target.dropFirst()
        let segmentEnd = trimmed.firstIndex(where: { $0 == "/" || $0 == "?" }) ?? trimmed.endIndex
        let segment = trimmed[..<segmentEnd]
        var rest = String(trimmed[segmentEnd...])

        let parts = segment.split(separator: "@", maxSplits: 1, omittingEmptySubsequences: false)
        guard let first = parts.first, !first.isEmpty else { return nil }
        let name = String(first).lowercased()
        let tag = parts.count > 1 && !parts[1].isEmpty
            ? (String(parts[1]).removingPercentEncoding ?? String(parts[1]))
            : nil

        let base: String
        if name == "_" {
            // `/_/api.example.com/v1/...` reaches any HTTPS host without a named route.
            guard rest.hasPrefix("/") else { return nil }
            let afterSlash = rest.dropFirst()
            let hostEnd = afterSlash.firstIndex(where: { $0 == "/" || $0 == "?" }) ?? afterSlash.endIndex
            let host = String(afterSlash[..<hostEnd])
            guard Self.isPlausibleHost(host) else { return nil }
            base = "https://" + host
            rest = String(afterSlash[hostEnd...])
        } else {
            guard let upstream = routes[name] else { return nil }
            base = upstream.hasSuffix("/") ? String(upstream.dropLast()) : upstream
        }
        guard let url = URL(string: base + rest), url.host != nil else { return nil }
        return Resolved(route: name == "_" ? (url.host ?? "_") : name, harnessTag: tag, url: url)
    }

    private static func isPlausibleHost(_ host: String) -> Bool {
        guard host.contains("."), host.count <= 253 else { return false }
        return host.allSatisfy { $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "." || $0 == "-" || $0 == ":") }
    }
}

// MARK: - Files

public enum AppPaths {
    /// `SPEEDTRACKER_HOME` overrides the location, which keeps test runs out of real data.
    public static var home: URL {
        if let override = ProcessInfo.processInfo.environment["SPEEDTRACKER_HOME"], !override.isEmpty {
            return URL(fileURLWithPath: override, isDirectory: true)
        }
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return support.appendingPathComponent("SpeedTracker", isDirectory: true)
    }

    public static var config: URL { home.appendingPathComponent("config.json") }
    public static var history: URL { home.appendingPathComponent("history.jsonl") }
}

/// `config.json`: `{"port": 4141, "routes": {"myhost": "https://llm.example.com"}}`.
/// Routes listed there are added to the built-in ones, or replace them by name.
public struct ConfigFile: Codable {
    public var port: UInt16?
    public var routes: [String: String]?

    public static func load() -> ProxyConfig {
        var config = ProxyConfig()
        let url = AppPaths.config
        if let data = try? Data(contentsOf: url), let file = try? JSONDecoder().decode(ConfigFile.self, from: data) {
            if let port = file.port, port > 0 { config.port = port }
            for (name, upstream) in file.routes ?? [:] where URL(string: upstream)?.host != nil {
                config.routes[name.lowercased()] = upstream
            }
        } else if !FileManager.default.fileExists(atPath: url.path) {
            // Leave a starter file behind so there is something to edit.
            try? FileManager.default.createDirectory(at: AppPaths.home, withIntermediateDirectories: true)
            let starter = ConfigFile(port: ProxyConfig.defaultPort, routes: [:])
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            try? encoder.encode(starter).write(to: url)
        }
        return config
    }
}
