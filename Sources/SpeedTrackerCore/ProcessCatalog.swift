import Darwin
import Foundation

/// Looks up what a process is, to tell a coding harness from everything else.
public enum ProcessInspector {
    public static func path(of pid: Int32) -> String? {
        var buffer = [CChar](repeating: 0, count: 4 * Int(MAXPATHLEN))
        let length = proc_pidpath(pid, &buffer, UInt32(buffer.count))
        return length > 0 ? String(cString: buffer) : nil
    }

    /// The process's command-line arguments. The environment that follows them
    /// in the kernel's buffer is never read: it can hold API keys.
    public static func arguments(of pid: Int32) -> [String] {
        var mib: [Int32] = [CTL_KERN, KERN_PROCARGS2, pid]
        var size = 0
        guard sysctl(&mib, 3, nil, &size, nil, 0) == 0, size > MemoryLayout<Int32>.size else { return [] }
        var buffer = [UInt8](repeating: 0, count: size)
        guard sysctl(&mib, 3, &buffer, &size, nil, 0) == 0 else { return [] }

        let argc = buffer.withUnsafeBytes { $0.load(as: Int32.self) }
        var index = MemoryLayout<Int32>.size
        // Skip the executable path and the padding after it.
        while index < size, buffer[index] != 0 { index += 1 }
        while index < size, buffer[index] == 0 { index += 1 }

        var arguments: [String] = []
        while arguments.count < Int(argc), arguments.count < 12, index < size {
            let start = index
            while index < size, buffer[index] != 0 { index += 1 }
            arguments.append(String(decoding: buffer[start..<index], as: UTF8.self))
            index += 1
        }
        return arguments
    }
}

public enum HarnessCatalog {
    /// Names the harness a process is, or nil if it is not a known one.
    /// `path` is the executable; `arguments` matter for harnesses that run
    /// under an interpreter such as node, bun or python.
    public static func classify(path: String, arguments: [String]) -> String? {
        let executable = (path as NSString).lastPathComponent.lowercased()
        let lowerPath = path.lowercased()
        // The script an interpreter is running, e.g. `bun /…/bin/omp`.
        let script = arguments.dropFirst().first { !$0.hasPrefix("-") }?.lowercased() ?? ""
        let scriptName = (script as NSString).lastPathComponent
        let isInterpreter = ["node", "bun", "deno", "python", "python3", "ruby"].contains(executable)
            || executable.hasPrefix("python3.")

        func named(_ names: String...) -> Bool {
            names.contains(executable) || (isInterpreter && names.contains(scriptName))
        }

        if executable.hasPrefix("codexbar") || executable.hasPrefix("speedtracker") { return nil }
        // The desktop app's own binary is `Claude`; the harness it embeds is `claude`.
        if (path as NSString).lastPathComponent == "Claude" { return nil }
        if named("claude") || lowerPath.contains("/claude/versions/") || script.contains("claude-code") { return "Claude Code" }
        if named("codex") || executable.hasPrefix("codex-") { return "Codex" }
        if named("omp") || script.contains("oh-my-pi") { return "OMP" }
        if named("pi") || script.contains("pi-coding-agent") { return "Pi" }
        if named("opencode") || script.contains("opencode") { return "opencode" }
        if named("gemini") || script.contains("gemini-cli") { return "Gemini CLI" }
        // Antigravity's agent runs in a language server beside the editor, or as the `agy` terminal agent.
        if named("agy") || (lowerPath.contains("antigravity") && (executable.hasPrefix("language_server") || executable == "agentapi")) {
            return "Antigravity"
        }
        if named("qwen") || script.contains("qwen-code") { return "Qwen Code" }
        if named("dsh", "deepseek", "deepseek-cli", "deepseek-tui") || script.contains("deepseek-harness") { return "DeepSeek CLI" }
        if named("aider") || script.contains("aider") { return "Aider" }
        if named("goose", "goosed") { return "Goose" }
        if named("crush") { return "Crush" }
        if named("amp") { return "Amp" }
        if named("droid") { return "Droid" }
        if named("kimi") || script.contains("kimi-cli") { return "Kimi CLI" }
        if named("cursor-agent") { return "Cursor CLI" }
        if named("cline") { return "Cline" }
        return nil
    }

    /// The inner processes of the desktop apps that host a harness. Their own connections
    /// to the provider carry sync and status traffic; the harness they embed runs as a
    /// separate process and is tracked through its session log.
    public static func isHostAppProcess(path: String) -> Bool {
        ["/Claude.app/", "/ChatGPT.app/", "/Codex.app/", "/Antigravity.app/"].contains { path.contains($0) }
    }

    /// Processes whose traffic to a model provider is not a harness at work:
    /// browsers, content filters that proxy other apps, and usage monitors.
    public static func isIgnored(processName: String) -> Bool {
        let name = processName.lowercased()
        return [
            "speedtracker", "codexbar", "com.adguard", "adguard", "safari", "com.apple.webkit", "google chrome",
            "chrome", "firefox", "arc", "brave", "microsoft edge", "opera", "vivaldi", "zen", "orion",
            "mdnsresponder", "apsd", "nsurlsessiond", "trustd", "cloudd", "netstat",
        ].contains { name.hasPrefix($0) }
    }
}

/// Knows which addresses belong to model providers, so traffic from a process
/// that is not a known harness can still be recognised.
public final class HostCatalog: @unchecked Sendable {
    public static let knownHosts = [
        "api.anthropic.com", "api.openai.com", "chatgpt.com", "api.deepseek.com", "openrouter.ai",
        "api.x.ai", "api.groq.com", "api.cerebras.ai", "api.mistral.ai", "api.moonshot.ai",
        "api.moonshot.cn", "api.z.ai", "open.bigmodel.cn", "api.together.xyz", "api.fireworks.ai",
        "dashscope.aliyuncs.com", "api.kimi.com",
    ]

    /// Shared with every other Google API, so only trusted for known harnesses.
    public static let sharedHosts = [
        "generativelanguage.googleapis.com", "cloudcode-pa.googleapis.com", "daily-cloudcode-pa.googleapis.com",
        "aiplatform.googleapis.com",
    ]

    private let lock = NSLock()
    private var hostByAddress: [String: String] = [:]
    private var sharedAddresses: Set<String> = []
    private var lastRefresh = Date.distantPast
    private let queue = DispatchQueue(label: "speedtracker.dns", qos: .utility)

    public init() {}

    /// The provider host behind an address. `includingShared` adds hosts whose
    /// addresses also serve unrelated services.
    public func host(for address: String, includingShared: Bool) -> String? {
        lock.lock()
        defer { lock.unlock() }
        guard let host = hostByAddress[address] else { return nil }
        if !includingShared, sharedAddresses.contains(address) { return nil }
        return host
    }

    /// Resolves the provider list again if it is stale. Returns at once; the work happens in the background.
    public func refreshIfNeeded(now: Date = Date(), home: URL = FileManager.default.homeDirectoryForCurrentUser) {
        lock.lock()
        let stale = now.timeIntervalSince(lastRefresh) > 300
        if stale { lastRefresh = now }
        lock.unlock()
        guard stale else { return }
        queue.async { [self] in
            let configured = Self.configuredHosts(home: home)
            var resolved: [String: String] = [:]
            var shared: Set<String> = []
            for host in Self.knownHosts + configured {
                for address in Self.resolve(host) { resolved[address] = host }
            }
            for host in Self.sharedHosts {
                for address in Self.resolve(host) where resolved[address] == nil {
                    resolved[address] = host
                    shared.insert(address)
                }
            }
            lock.lock()
            // Keep old answers too: a long-lived connection outlives a DNS rotation.
            hostByAddress.merge(resolved) { _, new in new }
            sharedAddresses.formUnion(shared)
            lock.unlock()
        }
    }

    static func resolve(_ host: String) -> [String] {
        var hints = addrinfo()
        hints.ai_socktype = SOCK_STREAM
        var result: UnsafeMutablePointer<addrinfo>?
        guard getaddrinfo(host, nil, &hints, &result) == 0 else { return [] }
        defer { freeaddrinfo(result) }
        var addresses: [String] = []
        var cursor = result
        while let info = cursor {
            var buffer = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            if getnameinfo(info.pointee.ai_addr, info.pointee.ai_addrlen, &buffer, socklen_t(buffer.count), nil, 0, NI_NUMERICHOST) == 0 {
                addresses.append(String(cString: buffer))
            }
            cursor = info.pointee.ai_next
        }
        return addresses
    }

    /// Gateways the user's harnesses are pointed at. Only lines that set a base
    /// URL are looked at; keys and tokens in the same files are not read.
    static func configuredHosts(home: URL) -> [String] {
        let files = [".codex/config.toml", ".omp/agent/models.yml", ".pi/agent/models.json", ".claude/settings.json"]
        var hosts: Set<String> = []
        for file in files {
            guard let text = try? String(contentsOf: home.appendingPathComponent(file), encoding: .utf8) else { continue }
            hosts.formUnion(baseURLHosts(in: text))
        }
        return hosts.sorted()
    }

    static func baseURLHosts(in text: String) -> [String] {
        var hosts: [String] = []
        for line in text.split(separator: "\n") {
            let lower = line.lowercased()
            guard lower.contains("base_url") || lower.contains("baseurl") else { continue }
            guard let range = line.range(of: #"https?://[A-Za-z0-9.\-]+"#, options: .regularExpression),
                  let host = URL(string: String(line[range]))?.host,
                  host.contains("."), host != "127.0.0.1", host != "localhost"
            else { continue }
            hosts.append(host.lowercased())
        }
        return hosts
    }
}
