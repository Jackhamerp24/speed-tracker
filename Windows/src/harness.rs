use crate::discovery;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn file_stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) => &name[..dot],
        None => name,
    }
}

// Splits a command line into arguments: quoted runs keep their spaces, everything else breaks on whitespace.
fn arguments(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut rest = text.trim_start();
    while let Some(first) = rest.chars().next() {
        let quoted = (first == '"' || first == '\'')
            .then(|| rest[1..].find(first))
            .flatten();
        let end = match quoted {
            Some(close) => {
                tokens.push(&rest[1..1 + close]);
                close + 2
            }
            None => {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                tokens.push(&rest[..end]);
                end
            }
        };
        rest = rest[end..].trim_start();
    }
    tokens
}

/// Whether the path is inside a desktop app that only hosts a harness. Their own processes talk to
/// the same providers without being a coding harness. On Windows the Claude app's executable is
/// `claude.exe` too, so the name alone cannot tell it from Claude Code; only the folder can.
pub fn is_host_app(executable: &str) -> bool {
    let path = executable.replace('\\', "/");
    let lower = path.to_lowercase();
    [
        "Claude.app/",
        "ChatGPT.app/",
        "Codex.app/",
        "Antigravity.app/",
    ]
    .iter()
    .any(|app| path.contains(app))
        || ["/windowsapps/claude_", "/anthropicclaude/", "/windowsapps/openai."]
            .iter()
            .any(|folder| lower.contains(folder))
}

/// Names the harness a process is, or None. Command-line matching uses only the interpreter's
/// script path, not prompt arguments.
pub fn classify(executable: &str, command_line: Option<&str>) -> Option<&'static str> {
    let path = executable.replace('\\', "/");
    let mut name = file_stem(&path).to_lowercase();
    // Antigravity's agent runs in a language server beside the editor, or as the agy terminal agent.
    // Checked before the host-app rule: the server lives inside the editor's own bundle.
    if name == "agy"
        || (path.to_lowercase().contains("antigravity")
            && (name.starts_with("language_server") || name == "agentapi"))
    {
        return Some("Antigravity");
    }
    if name.starts_with("speedtracker")
        || name.starts_with("codexbar")
        || name.contains("electron")
        || is_host_app(executable)
    {
        return None;
    }
    if matches!(
        name.as_str(),
        "node" | "bun" | "deno" | "python" | "python3" | "ruby"
    ) || name.starts_with("python3.")
    {
        let tokens = arguments(command_line.unwrap_or(""));
        let start =
            usize::from(tokens.first().is_some_and(|first| {
                file_stem(&first.replace('\\', "/")).eq_ignore_ascii_case(&name)
            }));
        let script = tokens
            .iter()
            .skip(start)
            .find(|token| !token.starts_with('-'))
            .map(|token| token.replace('\\', "/").to_lowercase())
            .unwrap_or_default();
        name = file_stem(&script).to_string();
        for (marker, harness) in [
            ("claude-code", "Claude Code"),
            ("oh-my-pi", "OMP"),
            ("pi-coding-agent", "Pi"),
            ("deepseek-harness", "DeepSeek CLI"),
            ("opencode", "opencode"),
            ("gemini-cli", "Gemini CLI"),
            ("qwen-code", "Qwen Code"),
        ] {
            if script.contains(marker) {
                return Some(harness);
            }
        }
    }
    Some(match name.as_str() {
        "claude" | "claude-code" => "Claude Code",
        "codex" => "Codex",
        "omp" => "OMP",
        "pi" => "Pi",
        "opencode" => "opencode",
        "dsh" => "DeepSeek CLI",
        "gemini" => "Gemini CLI",
        "qwen" => "Qwen Code",
        "aider" => "Aider",
        "goose" | "goosed" => "Goose",
        "crush" => "Crush",
        _ => return None,
    })
}

struct Signature {
    harness: &'static str,
    commands: &'static [&'static str],
    // Relative to a home folder.
    paths: &'static [&'static str],
}

// Works out which harnesses are actually on this machine, so the app only offers ones the user could
// be running. Looked up once at launch and again now and then, since a harness can be installed while
// the app is open. A harness counts as installed when its command is found where commands live, or one
// of its known folders exists. Session logs and running processes are separate evidence, added by
// the tracker. Mirrors HarnessPresence.swift.
//
// Names match what `classify` returns for the running process.
const SIGNATURES: &[Signature] = &[
    Signature {
        harness: "Claude Code",
        commands: &["claude"],
        paths: &[
            "AppData/Roaming/Claude/claude-code",
            ".claude/local/claude",
            ".local/share/claude/versions",
        ],
    },
    Signature {
        harness: "Codex",
        commands: &["codex"],
        paths: &[".codex/packages"],
    },
    Signature {
        harness: "OMP",
        commands: &["omp"],
        paths: &[],
    },
    Signature {
        harness: "Pi",
        commands: &["pi"],
        paths: &[],
    },
    Signature {
        harness: "opencode",
        commands: &["opencode"],
        paths: &[".opencode/bin"],
    },
    Signature {
        harness: "Gemini CLI",
        commands: &["gemini"],
        paths: &[],
    },
    Signature {
        harness: "Antigravity",
        commands: &["agy"],
        paths: &["AppData/Local/Programs/Antigravity"],
    },
    Signature {
        harness: "DeepSeek CLI",
        commands: &["dsh"],
        paths: &[],
    },
    Signature {
        harness: "Qwen Code",
        commands: &["qwen"],
        paths: &[],
    },
    Signature {
        harness: "Aider",
        commands: &["aider"],
        paths: &[],
    },
    Signature {
        harness: "Goose",
        commands: &["goose"],
        paths: &[],
    },
    Signature {
        harness: "Crush",
        commands: &["crush"],
        paths: &[],
    },
];

/// Each harness with the commands that start it, for checking the scan and the process classifier agree.
pub fn commands() -> Vec<(&'static str, &'static str)> {
    SIGNATURES
        .iter()
        .flat_map(|signature| {
            signature
                .commands
                .iter()
                .map(|command| (signature.harness, *command))
        })
        .collect()
}

/// Every harness this app can name, installed or not.
pub fn known_harnesses() -> Vec<&'static str> {
    SIGNATURES
        .iter()
        .map(|signature| signature.harness)
        .collect()
}

fn relative(home: &Path, path: &str) -> PathBuf {
    path.split('/')
        .fold(home.to_path_buf(), |joined, part| joined.join(part))
}

/// Harnesses found installed under any of the given home folders (the Windows profile, and WSL homes
/// when their file systems are mounted). `search_path` is the PATH to honour on top of the usual
/// per-user and package-manager folders.
pub fn installed(homes: &[PathBuf], search_path: Option<&str>) -> BTreeSet<String> {
    let mut roots: Vec<&PathBuf> = Vec::new();
    for home in homes {
        if !home.as_os_str().is_empty() && !roots.iter().any(|known| same_folder(known, home)) {
            roots.push(home);
        }
    }
    let folders = command_folders(&roots, search_path);
    // Windows runs a command through any of these; a WSL home holds plain files.
    let extensions = ["", ".exe", ".cmd", ".bat", ".ps1"];
    let mut found = BTreeSet::new();
    for signature in SIGNATURES {
        let command = signature.commands.iter().any(|name| {
            folders.iter().any(|folder| {
                extensions
                    .iter()
                    .any(|extension| folder.join(format!("{name}{extension}")).is_file())
            })
        });
        let path = signature
            .paths
            .iter()
            .any(|path| roots.iter().any(|home| relative(home, path).exists()));
        if command || path {
            found.insert(signature.harness.to_string());
        }
    }
    found
}

fn same_folder(first: &Path, second: &Path) -> bool {
    first
        .to_string_lossy()
        .eq_ignore_ascii_case(&second.to_string_lossy())
}

// Where commands are installed for these users, without duplicates.
fn command_folders(homes: &[&PathBuf], search_path: Option<&str>) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = Vec::new();
    let owned;
    let search_path = match search_path {
        Some(path) => path,
        None => {
            owned = std::env::var("PATH").unwrap_or_default();
            &owned
        }
    };
    for entry in std::env::split_paths(search_path) {
        let text = entry.to_string_lossy();
        let text = text.trim();
        let rooted = text.starts_with(['/', '\\']) || text.as_bytes().get(1) == Some(&b':');
        if rooted {
            folders.push(PathBuf::from(text));
        }
    }
    for home in homes {
        for folder in [
            ".local/bin",
            ".bun/bin",
            "bin",
            ".npm-global/bin",
            ".cargo/bin",
            ".volta/bin",
            ".deno/bin",
            ".opencode/bin",
            "go/bin",
            "AppData/Roaming/npm",
            "AppData/Local/pnpm",
            "AppData/Local/Microsoft/WinGet/Links",
            "scoop/shims",
            ".asdf/shims",
        ] {
            folders.push(relative(home, folder));
        }
        // Node version managers keep one folder per installed version.
        for versions in [
            ".nvm/versions/node",
            "AppData/Roaming/nvm",
            "AppData/Roaming/fnm/node-versions",
            ".local/share/fnm/node-versions",
        ] {
            for version in discovery::directories(&relative(home, versions)) {
                folders.push(version.join("bin"));
                folders.push(version.join("installation"));
                folders.push(version.join("installation").join("bin"));
                folders.push(version);
            }
        }
    }
    let mut unique: Vec<PathBuf> = Vec::new();
    for folder in folders {
        if !unique.iter().any(|known| same_folder(known, &folder)) {
            unique.push(folder);
        }
    }
    unique
}
