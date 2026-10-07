using System.Text.RegularExpressions;

namespace SpeedTracker.Core;

public static class HarnessNames
{
    // Command-line matching uses only the interpreter's script path, not prompt arguments.
    public static string? Classify(string executable, string? arguments = null)
    {
        var path = executable.Replace('\\', '/');
        var rawName = path.Split('/').Last();
        var name = Path.GetFileNameWithoutExtension(rawName).ToLowerInvariant();
        // Antigravity's agent runs in a language server beside the editor, or as the agy terminal agent.
        // Checked before the host-app rule: the server lives inside the editor's own bundle.
        if (name == "agy" || (path.Contains("antigravity", StringComparison.OrdinalIgnoreCase) && (name.StartsWith("language_server", StringComparison.Ordinal) || name == "agentapi"))) return "Antigravity";
        if (name.StartsWith("speedtracker", StringComparison.Ordinal) || name.StartsWith("codexbar", StringComparison.Ordinal) || name.Contains("electron", StringComparison.Ordinal) || path.Contains("Claude.app/", StringComparison.Ordinal) || path.Contains("ChatGPT.app/", StringComparison.Ordinal) || path.Contains("Codex.app/", StringComparison.Ordinal) || path.Contains("Antigravity.app/", StringComparison.Ordinal)) return null;
        if (name is "node" or "bun" or "deno" or "python" or "python3" or "ruby" || name.StartsWith("python3.", StringComparison.Ordinal))
        {
            var tokens = Regex.Matches(arguments ?? "", "\"([^\"]*)\"|'([^']*)'|([^\\s]+)").Select(m => m.Groups[1].Success ? m.Groups[1].Value : m.Groups[2].Success ? m.Groups[2].Value : m.Groups[3].Value).ToArray();
            var start = tokens.Length > 0 && Path.GetFileNameWithoutExtension(tokens[0].Replace('\\', '/').Split('/').Last()).Equals(name, StringComparison.OrdinalIgnoreCase) ? 1 : 0;
            var script = tokens.Skip(start).FirstOrDefault(t => !t.StartsWith('-'))?.Replace('\\', '/').ToLowerInvariant() ?? "";
            name = Path.GetFileNameWithoutExtension(script.Split('/').Last());
            if (script.Contains("claude-code", StringComparison.Ordinal)) return "Claude Code";
            if (script.Contains("oh-my-pi", StringComparison.Ordinal)) return "OMP";
            if (script.Contains("pi-coding-agent", StringComparison.Ordinal)) return "Pi";
            if (script.Contains("deepseek-harness", StringComparison.Ordinal)) return "DeepSeek CLI";
            if (script.Contains("opencode", StringComparison.Ordinal)) return "opencode";
            if (script.Contains("gemini-cli", StringComparison.Ordinal)) return "Gemini CLI";
            if (script.Contains("qwen-code", StringComparison.Ordinal)) return "Qwen Code";
        }
        return name switch
        {
            "claude" or "claude-code" => "Claude Code", "codex" => "Codex", "omp" => "OMP", "pi" => "Pi", "opencode" => "opencode",
            "dsh" => "DeepSeek CLI", "gemini" => "Gemini CLI", "qwen" => "Qwen Code", "aider" => "Aider", "goose" or "goosed" => "Goose", "crush" => "Crush", _ => null
        };
    }
}
