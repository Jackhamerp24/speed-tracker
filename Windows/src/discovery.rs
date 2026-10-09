use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// Where one harness keeps its session telemetry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogSource {
    pub harness: String,
    pub root: PathBuf,
}

impl LogSource {
    pub fn new(harness: &str, root: impl Into<PathBuf>) -> LogSource {
        LogSource {
            harness: harness.to_string(),
            root: root.into(),
        }
    }
}

fn folder(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The signed-in user's profile folder.
pub fn user_home() -> PathBuf {
    folder(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default()
}

/// The Windows profile, plus the home folders of WSL distributions that are already mounted.
pub fn homes(home: Option<&Path>) -> Vec<PathBuf> {
    let mut homes = vec![home.map(Path::to_path_buf).unwrap_or_else(user_home)];
    // Only already-mounted WSL filesystem views; no wsl.exe call or distro startup.
    let wsl = Path::new(r"\\wsl.localhost");
    if cfg!(windows) && wsl.is_dir() {
        for distro in directories(wsl) {
            homes.extend(directories(&distro.join("home")));
        }
    }
    let mut unique: Vec<PathBuf> = Vec::new();
    for home in homes {
        if !unique.iter().any(|known| {
            known
                .to_string_lossy()
                .eq_ignore_ascii_case(&home.to_string_lossy())
        }) {
            unique.push(home);
        }
    }
    unique
}

pub fn default_sources(home: Option<&Path>) -> Vec<LogSource> {
    let home = home.map(Path::to_path_buf).unwrap_or_else(user_home);
    let mut sources = Vec::new();
    let dsh_override = std::env::var("DSH_HOME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    for user in homes(Some(&home)) {
        sources.push(LogSource::new(
            "Claude Code",
            user.join(".claude").join("projects"),
        ));
        sources.push(LogSource::new(
            "Codex",
            user.join(".codex").join("sessions"),
        ));
        sources.push(LogSource::new(
            "OMP",
            user.join(".omp").join("agent").join("sessions"),
        ));
        sources.push(LogSource::new(
            "Pi",
            user.join(".pi").join("agent").join("sessions"),
        ));
        sources.push(LogSource::new(
            "opencode",
            user.join(".local").join("share").join("opencode"),
        ));
        sources.push(LogSource::new(
            "Gemini CLI",
            user.join(".gemini").join("tmp"),
        ));
        sources.push(LogSource::new(
            "Antigravity",
            user.join(".gemini")
                .join("antigravity")
                .join("conversations"),
        ));
        if user != home || dsh_override.is_none() {
            sources.push(LogSource::new(
                "DeepSeek CLI",
                user.join(".dsh").join("sessions"),
            ));
        }
    }
    if let Some(value) = dsh_override {
        let root = if value == "~" {
            home.clone()
        } else if let Some(rest) = value
            .strip_prefix("~/")
            .or_else(|| value.strip_prefix("~\\"))
        {
            home.join(rest)
        } else {
            PathBuf::from(value)
        };
        sources.push(LogSource::new("DeepSeek CLI", root.join("sessions")));
    }
    if let Some(data) = folder("XDG_DATA_HOME") {
        sources.push(LogSource::new("opencode", data.join("opencode")));
    }
    for variable in ["LOCALAPPDATA", "APPDATA"] {
        if let Some(data) = folder(variable) {
            sources.push(LogSource::new("opencode", data.join("opencode")));
        }
    }
    let mut unique: Vec<LogSource> = Vec::new();
    for source in sources {
        if !unique.contains(&source) {
            unique.push(source);
        }
    }
    unique
}

/// The folders directly inside `root`; none when it cannot be read.
pub fn directories(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()) || entry.path().is_dir())
        .map(|entry| entry.path())
        .collect()
}

/// Which file names a walk yields. Matching ignores letter case, as Windows does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// `*.jsonl`
    Jsonl,
    /// `*.json`
    Json,
    /// `session*.jsonl*`
    DeepSeekSession,
}

impl Pattern {
    fn matches(self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        match self {
            Pattern::Jsonl => name.ends_with(".jsonl"),
            Pattern::Json => name.ends_with(".json"),
            Pattern::DeepSeekSession => name
                .strip_prefix("session")
                .is_some_and(|rest| rest.contains(".jsonl")),
        }
    }
}

// A folder that is a link or junction leads somewhere else; following it could loop or leave the tree.
fn is_link(entry: &std::fs::DirEntry) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const REPARSE_POINT: u32 = 0x400;
        entry
            .metadata()
            .map(|metadata| metadata.file_attributes() & REPARSE_POINT != 0)
            .unwrap_or(true)
    }
    #[cfg(not(windows))]
    {
        entry
            .file_type()
            .map(|kind| kind.is_symlink())
            .unwrap_or(true)
    }
}

/// Every matching file under a root, one folder at a time. The walk keeps a cursor rather than the
/// full list of paths, so it can be paused and resumed across polls of a very large tree.
pub struct FileWalk {
    pattern: Pattern,
    folders: Vec<PathBuf>,
    files: VecDeque<PathBuf>,
}

pub fn files(root: &Path, pattern: Pattern) -> FileWalk {
    FileWalk {
        pattern,
        folders: vec![root.to_path_buf()],
        files: VecDeque::new(),
    }
}

impl Iterator for FileWalk {
    type Item = PathBuf;
    fn next(&mut self) -> Option<PathBuf> {
        loop {
            if let Some(file) = self.files.pop_front() {
                return Some(file);
            }
            let folder = self.folders.pop()?;
            let Ok(entries) = std::fs::read_dir(&folder) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                // A linked folder reports as a link, not a directory, so ask the path as well.
                if kind.is_dir() || (kind.is_symlink() && entry.path().is_dir()) {
                    if !is_link(&entry) {
                        self.folders.push(entry.path());
                    }
                } else if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| self.pattern.matches(name))
                {
                    self.files.push_back(entry.path());
                }
            }
        }
    }
}
