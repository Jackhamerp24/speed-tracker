namespace SpeedTracker.Core;

// Works out which harnesses are actually on this machine, so the app only offers ones the user could
// be running. Looked up once at launch and again now and then, since a harness can be installed while
// the app is open. A harness counts as installed when its command is found where commands live, or one
// of its known folders exists. Session logs and running processes are separate evidence, added by
// TrackerService. Mirrors HarnessPresence.swift.
public static class HarnessPresence
{
    private sealed record Signature(string Harness, string[] Commands, params string[] Paths);

    // Names match what HarnessNames.Classify returns for the running process. Paths are relative to a home folder.
    private static readonly Signature[] Signatures =
    {
        new("Claude Code", new[] { "claude" }, "AppData/Roaming/Claude/claude-code", ".claude/local/claude", ".local/share/claude/versions"),
        new("Codex", new[] { "codex" }, ".codex/packages"),
        new("OMP", new[] { "omp" }),
        new("Pi", new[] { "pi" }),
        new("opencode", new[] { "opencode" }, ".opencode/bin"),
        new("Gemini CLI", new[] { "gemini" }),
        new("Antigravity", new[] { "agy" }, "AppData/Local/Programs/Antigravity"),
        new("DeepSeek CLI", new[] { "dsh" }),
        new("Qwen Code", new[] { "qwen" }),
        new("Aider", new[] { "aider" }),
        new("Goose", new[] { "goose" }),
        new("Crush", new[] { "crush" }),
    };

    // Each harness with the commands that start it, for checking the scan and the process classifier agree.
    public static IEnumerable<(string Harness, string Command)> Commands => Signatures.SelectMany(s => s.Commands.Select(c => (s.Harness, c)));

    // Every harness this app can name, installed or not.
    public static IReadOnlyList<string> KnownHarnesses { get; } = Array.AsReadOnly(Signatures.Select(s => s.Harness).ToArray());

    // Harnesses found installed under any of the given home folders (the Windows profile, and WSL homes
    // when their file systems are mounted). searchPath is the PATH to honour on top of the usual per-user
    // and package-manager folders.
    public static IReadOnlySet<string> Installed(IEnumerable<string>? homes = null, string? searchPath = null)
    {
        var roots = (homes ?? new[] { Environment.GetFolderPath(Environment.SpecialFolder.UserProfile) }).Where(h => !string.IsNullOrWhiteSpace(h)).Distinct(StringComparer.OrdinalIgnoreCase).ToArray();
        searchPath ??= Environment.GetEnvironmentVariable("PATH");
        var folders = CommandFolders(roots, searchPath);
        // Windows runs a command through any of these; a WSL home holds plain files.
        var extensions = new[] { "", ".exe", ".cmd", ".bat", ".ps1" };
        var found = new HashSet<string>(StringComparer.Ordinal);
        foreach (var signature in Signatures)
        {
            var command = signature.Commands.Any(name => folders.Any(folder => extensions.Any(extension => IsFile(Path.Combine(folder, name + extension)))));
            var path = signature.Paths.Any(relative => roots.Any(home => Exists(Path.Combine(home, relative.Replace('/', Path.DirectorySeparatorChar)))));
            if (command || path) found.Add(signature.Harness);
        }
        return found;
    }

    // Where commands are installed for these users, without duplicates.
    private static IReadOnlyList<string> CommandFolders(IReadOnlyList<string> homes, string? searchPath)
    {
        var folders = new List<string>();
        foreach (var entry in (searchPath ?? "").Split(Path.PathSeparator, StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
            if (Path.IsPathRooted(entry)) folders.Add(entry);
        foreach (var home in homes)
        {
            foreach (var relative in new[]
            {
                ".local/bin", ".bun/bin", "bin", ".npm-global/bin", ".cargo/bin", ".volta/bin", ".deno/bin", ".opencode/bin", "go/bin",
                "AppData/Roaming/npm", "AppData/Local/pnpm", "AppData/Local/Microsoft/WinGet/Links", "scoop/shims", ".asdf/shims"
            }) folders.Add(Path.Combine(home, relative.Replace('/', Path.DirectorySeparatorChar)));
            // Node version managers keep one folder per installed version.
            foreach (var versions in new[] { ".nvm/versions/node", "AppData/Roaming/nvm", "AppData/Roaming/fnm/node-versions", ".local/share/fnm/node-versions" })
            {
                foreach (var version in LogDiscovery.Directories(Path.Combine(home, versions.Replace('/', Path.DirectorySeparatorChar))))
                {
                    folders.Add(version); folders.Add(Path.Combine(version, "bin")); folders.Add(Path.Combine(version, "installation")); folders.Add(Path.Combine(version, "installation", "bin"));
                }
            }
        }
        return folders.Distinct(StringComparer.OrdinalIgnoreCase).ToArray();
    }

    private static bool IsFile(string path)
    {
        try { return File.Exists(path); }
        catch (IOException) { return false; }
        catch (UnauthorizedAccessException) { return false; }
    }

    private static bool Exists(string path)
    {
        try { return File.Exists(path) || Directory.Exists(path); }
        catch (IOException) { return false; }
        catch (UnauthorizedAccessException) { return false; }
    }
}
