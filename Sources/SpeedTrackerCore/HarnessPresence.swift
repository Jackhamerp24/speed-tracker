import Foundation

/// Works out which harnesses are actually on this machine, so the app only offers
/// ones the user could be running. Looked up once at launch and again now and then,
/// since a harness can be installed while the app is open.
///
/// A harness counts as installed when its command is found in a place commands live,
/// or one of its known files or app bundles exists. Session logs and running processes
/// are separate evidence, added by the detector.
public enum HarnessPresence {
    struct Signature {
        let harness: String
        let commands: [String]
        /// Absolute, or relative to the home folder.
        var paths: [String] = []
    }

    /// Names match what `HarnessCatalog.classify` returns for the running process.
    static let signatures: [Signature] = [
        Signature(harness: "Claude Code", commands: ["claude"], paths: [
            "Library/Application Support/Claude/claude-code", ".claude/local/claude", ".local/share/claude/versions",
        ]),
        Signature(harness: "Codex", commands: ["codex"], paths: [
            "/Applications/ChatGPT.app/Contents/Resources/codex-cli", "/Applications/Codex.app", ".codex/packages",
        ]),
        Signature(harness: "OMP", commands: ["omp"]),
        Signature(harness: "Pi", commands: ["pi"]),
        Signature(harness: "opencode", commands: ["opencode"], paths: [".opencode/bin/opencode"]),
        Signature(harness: "Gemini CLI", commands: ["gemini"]),
        Signature(harness: "Antigravity", commands: ["agy"], paths: [
            "/Applications/Antigravity.app", "Applications/Antigravity.app",
        ]),
        Signature(harness: "DeepSeek CLI", commands: ["dsh"]),
        Signature(harness: "Qwen Code", commands: ["qwen"]),
        Signature(harness: "Aider", commands: ["aider"]),
        Signature(harness: "Goose", commands: ["goose"]),
        Signature(harness: "Crush", commands: ["crush"]),
        Signature(harness: "Amp", commands: ["amp"]),
        Signature(harness: "Droid", commands: ["droid"]),
        Signature(harness: "Kimi CLI", commands: ["kimi"]),
        Signature(harness: "Cursor CLI", commands: ["cursor-agent"]),
        Signature(harness: "Cline", commands: ["cline"]),
    ]

    /// Every harness this app can name, installed or not.
    public static var knownHarnesses: [String] { signatures.map(\.harness) }

    /// Harnesses found installed. `searchPath` is the `PATH` to honour, on top of the
    /// usual per-user and package-manager folders; an app started from the Finder gets
    /// a bare `PATH`, so those folders matter more than it does.
    /// `systemRoot` stands in for `/` so tests can build a machine in a temporary folder.
    public static func installed(
        home: URL = FileManager.default.homeDirectoryForCurrentUser,
        searchPath: String? = ProcessInfo.processInfo.environment["PATH"],
        systemRoot: URL = URL(fileURLWithPath: "/")
    ) -> Set<String> {
        let folders = commandFolders(home: home, searchPath: searchPath, systemRoot: systemRoot)
        let manager = FileManager.default
        var found: Set<String> = []
        for signature in signatures {
            let hasCommand = signature.commands.contains { command in
                folders.contains { folder in
                    // A folder also passes the executable test; a command is a file.
                    var isFolder: ObjCBool = false
                    let path = folder + "/" + command
                    return manager.fileExists(atPath: path, isDirectory: &isFolder) && !isFolder.boolValue
                        && manager.isExecutableFile(atPath: path)
                }
            }
            let hasPath = signature.paths.contains { path in
                manager.fileExists(atPath: (path.hasPrefix("/") ? systemRoot : home).appendingPathComponent(path).path)
            }
            if hasCommand || hasPath { found.insert(signature.harness) }
        }
        return found
    }

    /// Where commands are installed for this user, most specific first, without duplicates.
    static func commandFolders(home: URL, searchPath: String?, systemRoot: URL = URL(fileURLWithPath: "/")) -> [String] {
        var folders = (searchPath ?? "").split(separator: ":").map(String.init).filter { $0.hasPrefix("/") }
        folders += [
            ".local/bin", ".bun/bin", "bin", ".npm-global/bin", ".cargo/bin", ".volta/bin", ".deno/bin",
            "Library/pnpm", ".opencode/bin", ".yarn/bin", "go/bin", ".asdf/shims", ".local/share/mise/shims",
        ].map { home.appendingPathComponent($0).path }
        folders += ["opt/homebrew/bin", "usr/local/bin", "opt/local/bin"].map { systemRoot.appendingPathComponent($0).path }
        // Node version managers keep one bin folder per installed version.
        for versions in [".nvm/versions/node", ".local/share/fnm/node-versions", "Library/Application Support/fnm/node-versions"] {
            let root = home.appendingPathComponent(versions)
            for version in (try? FileManager.default.contentsOfDirectory(atPath: root.path)) ?? [] {
                folders.append(root.appendingPathComponent(version).appendingPathComponent("bin").path)
                folders.append(root.appendingPathComponent(version).appendingPathComponent("installation/bin").path)
            }
        }
        var seen: Set<String> = []
        return folders.filter { seen.insert($0).inserted }
    }
}
