import Foundation

/// Names the coding harness behind a request. An explicit `<route>@<name>` tag
/// in the base URL always wins, because some harnesses borrow another client's
/// User-Agent.
public enum HarnessDetector {
    private static let known: [(needle: String, name: String)] = [
        ("claude-cli", "Claude Code"),
        ("claude-code", "Claude Code"),
        ("codex", "Codex"),
        ("oh-my-pi", "OMP"),
        ("omp/", "OMP"),
        ("pi-coding-agent", "Pi"),
        ("opencode", "opencode"),
        ("geminicli", "Gemini CLI"),
        ("gemini-cli", "Gemini CLI"),
        ("qwen", "Qwen Code"),
        ("kimicli", "Kimi CLI"),
        ("deepseek", "DeepSeek CLI"),
        ("dsh/", "DeepSeek CLI"),
        ("aider", "Aider"),
        ("cline", "Cline"),
        ("roo-code", "Roo Code"),
        ("kilo-code", "Kilo Code"),
        ("cursor", "Cursor"),
        ("goose", "Goose"),
        ("crush", "Crush"),
        ("factory-cli", "Droid"),
        ("zed/", "Zed"),
        ("continue", "Continue"),
        ("copilot", "Copilot"),
        ("curl/", "curl"),
    ]

    private static let aliases: [String: String] = [
        "claude": "Claude Code", "claude-code": "Claude Code", "cc": "Claude Code",
        "codex": "Codex", "omp": "OMP", "pi": "Pi", "deepseek": "DeepSeek CLI",
        "dsh": "DeepSeek CLI", "deepseek-harness": "DeepSeek CLI", "opencode": "opencode",
    ]

    /// SDK and runtime names say nothing about which harness is on top of them.
    private static let generic: Set<String> = [
        "openai", "anthropic", "node", "node-fetch", "undici", "bun", "deno", "axios",
        "python-httpx", "python-requests", "python", "go-http-client", "okhttp", "reqwest",
        "mozilla", "ai-sdk",
    ]

    public static func detect(tag: String?, header: (String) -> String?) -> String {
        if let tag, !tag.isEmpty {
            return aliases[tag.lowercased()] ?? tag
        }
        let agent = header("User-Agent") ?? ""
        let haystack = [agent, header("originator") ?? "", header("x-title") ?? ""]
            .joined(separator: " ").lowercased()
        if let match = known.first(where: { haystack.contains($0.needle) }) {
            return match.name
        }
        if let title = header("x-title"), !title.isEmpty { return title }
        let product = agent.prefix { $0 != "/" && $0 != " " }
        if !product.isEmpty, !generic.contains(product.lowercased()) {
            return String(product)
        }
        return "Unknown"
    }
}
