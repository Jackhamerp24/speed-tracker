using System.Text.Json;
using System.Text.Json.Serialization;

namespace SpeedTracker.Core;

public sealed record RequestRecord
{
    public Guid Id { get; init; } = Guid.NewGuid();
    public DateTimeOffset StartedAt { get; init; }
    public string Harness { get; init; } = "Unknown";
    public string Route { get; init; } = "";
    public string UpstreamHost { get; init; } = "";
    public string Format { get; init; } = "unknown";
    public string Model { get; init; } = "unknown";
    public bool Streamed { get; init; }
    public int Status { get; init; } = 200;
    public double? Ttft { get; init; }
    public double? FirstVisible { get; init; }
    public double? Ttfb { get; init; }
    public double? Generation { get; init; }
    public double Total { get; init; }
    public int? InputTokens { get; init; }
    public int? CachedInputTokens { get; init; }
    public int OutputTokens { get; init; }
    public int? ReasoningTokens { get; init; }
    public bool TokensEstimated { get; init; }
    public double? Tps { get; init; }
    public bool Aborted { get; init; }
    public string? Source { get; init; }
    public string? SourceKey { get; init; }
    [JsonIgnore] internal string DedupKey => string.IsNullOrEmpty(SourceKey) ? "id:" + Id : "source:" + SourceKey;
    public static JsonSerializerOptions JsonOptions { get; } = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        PropertyNameCaseInsensitive = true,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull
    };
}

public sealed record ProviderIdentity(string Key, string Name, string? Host, string Evidence, bool Unverified)
{
    private static readonly Dictionary<string, (string Key, string Name)> Aliases = new(StringComparer.OrdinalIgnoreCase)
    {
        ["anthropic"] = ("anthropic", "Anthropic"), ["claude"] = ("anthropic", "Anthropic"),
        ["openai"] = ("openai", "OpenAI"), ["open-ai"] = ("openai", "OpenAI"),
        ["openai-codex"] = ("openai-codex", "OpenAI Codex"), ["openai_codex"] = ("openai-codex", "OpenAI Codex"),
        ["chatgpt"] = ("openai-codex", "OpenAI Codex"), ["codex"] = ("openai-codex", "OpenAI Codex"),
        ["google"] = ("gemini", "Google Gemini"), ["gemini"] = ("gemini", "Google Gemini"),
        ["google-gemini"] = ("gemini", "Google Gemini"), ["google-vertex"] = ("gemini", "Google Gemini"),
        ["vertex"] = ("gemini", "Google Gemini"), ["vertexai"] = ("gemini", "Google Gemini"),
        ["deepseek"] = ("deepseek", "DeepSeek"), ["openrouter"] = ("openrouter", "OpenRouter"),
        ["open-router"] = ("openrouter", "OpenRouter"), ["xai"] = ("xai", "xAI"), ["x-ai"] = ("xai", "xAI"), ["x.ai"] = ("xai", "xAI"),
        ["groq"] = ("groq", "Groq"), ["cerebras"] = ("cerebras", "Cerebras"), ["mistral"] = ("mistral", "Mistral"),
        ["moonshot"] = ("moonshot", "Moonshot"), ["kimi"] = ("moonshot", "Moonshot"),
        ["zai"] = ("zai", "Z.ai"), ["z-ai"] = ("zai", "Z.ai"), ["z.ai"] = ("zai", "Z.ai"), ["zhipu"] = ("zai", "Z.ai"),
        ["together"] = ("together", "Together AI"), ["togetherai"] = ("together", "Together AI"), ["together-ai"] = ("together", "Together AI"),
        ["fireworks"] = ("fireworks", "Fireworks AI"), ["fireworks-ai"] = ("fireworks", "Fireworks AI"),
        ["dashscope"] = ("dashscope", "Alibaba DashScope"), ["alibaba"] = ("dashscope", "Alibaba DashScope"), ["qwen"] = ("dashscope", "Alibaba DashScope"),
        ["ollama"] = ("ollama", "Ollama (reported local)"), ["lmstudio"] = ("lmstudio", "LM Studio (reported local)"),
        ["lm-studio"] = ("lmstudio", "LM Studio (reported local)"), ["lm_studio"] = ("lmstudio", "LM Studio (reported local)")
    };
    private static readonly Dictionary<string, string> Hosts = new(StringComparer.OrdinalIgnoreCase)
    {
        ["api.anthropic.com"] = "anthropic", ["api.openai.com"] = "openai", ["chatgpt.com"] = "openai-codex", ["www.chatgpt.com"] = "openai-codex", ["chat.openai.com"] = "openai-codex",
        ["generativelanguage.googleapis.com"] = "gemini", ["cloudcode-pa.googleapis.com"] = "gemini", ["aiplatform.googleapis.com"] = "gemini",
        ["api.deepseek.com"] = "deepseek", ["openrouter.ai"] = "openrouter", ["api.openrouter.ai"] = "openrouter", ["api.x.ai"] = "xai",
        ["api.groq.com"] = "groq", ["api.cerebras.ai"] = "cerebras", ["api.mistral.ai"] = "mistral", ["api.moonshot.ai"] = "moonshot", ["api.moonshot.cn"] = "moonshot", ["api.kimi.com"] = "moonshot",
        ["api.z.ai"] = "zai", ["open.bigmodel.cn"] = "zai", ["api.together.xyz"] = "together", ["api.together.ai"] = "together",
        ["api.fireworks.ai"] = "fireworks", ["dashscope.aliyuncs.com"] = "dashscope", ["dashscope-intl.aliyuncs.com"] = "dashscope"
    };
    private static string Label(string? text)
    {
        var value = (text ?? "").Trim().ToLowerInvariant();
        return value.All(c => char.IsLetterOrDigit(c) || "-_. ".Contains(c)) ? value : "";
    }
    private static Uri? Endpoint(string? text)
    {
        var value = (text ?? "").Trim();
        if (value.Length == 0) return null;
        var scheme = value.Contains("://", StringComparison.Ordinal);
        if (!scheme && (Aliases.ContainsKey(Label(value)) || !(value.Contains('.') || value.Contains(':') || value.Equals("localhost", StringComparison.OrdinalIgnoreCase)))) return null;
        if (!scheme && value.Count(c => c == ':') > 1 && !value.StartsWith('[')) value = "[" + value + "]";
        if (!Uri.TryCreate(scheme ? value : "http://" + value, UriKind.Absolute, out var uri) || string.IsNullOrEmpty(uri.Host)) return null;
        return uri;
    }
    public static ProviderIdentity From(RequestRecord record)
    {
        var endpoint = Endpoint(record.UpstreamHost) ?? Endpoint(record.Route);
        if (endpoint != null)
        {
            var host = endpoint.Host.Trim('[', ']', '.').ToLowerInvariant();
            var local = host == "localhost" || host.EndsWith(".localhost", StringComparison.Ordinal) || host.StartsWith("127.", StringComparison.Ordinal) || host is "::1" or "0.0.0.0" or "::";
            var authority = (host.Contains(':') ? "[" + host + "]" : host) + (endpoint.IsDefaultPort ? "" : ":" + endpoint.Port);
            var known = Hosts.TryGetValue(host, out var alias) ? Aliases[alias] : ("host:" + host, host);
            var evidence = record.Source switch
            {
                "proxy" => $"Observed upstream endpoint host: {authority}. Endpoint identity does not prove the model vendor behind it.",
                "network" => $"Provider host attributed from passive network discovery: {authority}. Shared addresses and gateways do not prove the original host or model vendor.",
                "log" => $"Endpoint host reported in session log: {authority}; not independently verified on the network.",
                _ => $"Recorded endpoint host: {authority}; observation source is unknown, so attribution is unverified."
            };
            return new(local ? "local:" + authority : known.Item1, local ? "Local · " + authority : known.Item2, host, evidence, record.Source != "proxy");
        }
        var upstream = Label(record.UpstreamHost);
        var label = upstream.Length == 0 || upstream == "unknown" ? Label(record.Route) : upstream;
        if (label is "" or "unknown" or "_") return new("unknown", "Unknown", null, "No endpoint host or provider label was recorded; provider is unknown.", true);
        var identity = Aliases.TryGetValue(label, out var provider) ? provider : ("reported:" + label, label);
        return new(identity.Item1, identity.Item2, null, $"Reported provider label: {label}; endpoint unverified. No endpoint host was recorded.", true);
    }
    internal ProviderIdentity Merge(ProviderIdentity other) => this == other ? this : this with
    {
        Host = Host ?? other.Host,
        Unverified = Unverified || other.Unverified,
        Evidence = Unverified || other.Unverified ? "Includes endpoint-unverified provenance; inspect individual calls for reported labels and observations." : "Observed endpoints; inspect individual calls for hosts. Endpoints do not prove model vendors."
    };
}

public sealed record DashboardFilter
{
    public DateTimeOffset? From { get; init; }
    public DateTimeOffset? Through { get; init; }
    public string? Harness { get; init; }
    public string? Provider { get; init; }
    public string? Model { get; init; }
    internal bool IncludesDate(RequestRecord record) => (!From.HasValue || record.StartedAt >= From) && (!Through.HasValue || record.StartedAt < Through);
    public bool Includes(RequestRecord record) => IncludesDate(record) && (Harness == null || Harness == record.Harness) && (Model == null || Model == record.Model) && (Provider == null || Provider == ProviderIdentity.From(record).Key);
}

public sealed record DashboardSummary(int Count, double? MedianTTFT, double? P95TTFT, double? MedianTPS, double? P95TPS, int OutputTokens, int EstimatedCount, int InterruptedCount, int LatencyCount, int SpeedCount, int RoundTripCount)
{
    public static bool ValidLatency(RequestRecord record) => !record.Aborted && record.Status < 400 && record.Ttft is double v && double.IsFinite(v) && v >= 0;
    public static bool ValidSpeed(RequestRecord record) => !record.Aborted && record.Status < 400 && record.Tps is double v && double.IsFinite(v) && v >= 0 && record.Generation is double g && double.IsFinite(g) && g > 0;
    public static DashboardSummary From(IEnumerable<RequestRecord> source)
    {
        var count = 0; var tokens = 0; var estimated = 0; var interrupted = 0; var roundTrip = 0;
        var latency = new List<double>(); var speed = new List<double>();
        foreach (var record in source)
        {
            count++;
            tokens = (int)Math.Min(int.MaxValue, (long)tokens + Math.Max(0, record.OutputTokens));
            if (record.TokensEstimated) estimated++;
            if (record.Aborted || record.Status >= 400) { interrupted++; continue; }
            if (ValidLatency(record)) latency.Add(record.Ttft!.Value);
            if (ValidSpeed(record)) speed.Add(record.Tps!.Value);
            else if (record.Tps is double rate && double.IsFinite(rate) && rate >= 0 && (record.Generation == null || record.Generation == 0) && double.IsFinite(record.Total) && record.Total > 0) roundTrip++;
        }
        latency.Sort(); speed.Sort();
        return new(count, Percentile(latency, .5), Percentile(latency, .95), Percentile(speed, .5), Percentile(speed, .95), tokens, estimated, interrupted, latency.Count, speed.Count, roundTrip);
    }
    private static double? Percentile(List<double> sorted, double probability)
    {
        if (sorted.Count == 0) return null;
        var position = (sorted.Count - 1) * probability; var lower = (int)position; var upper = Math.Min(lower + 1, sorted.Count - 1);
        return sorted[lower] + (sorted[upper] - sorted[lower]) * (position - lower);
    }
}
public sealed record DashboardGroup(ProviderIdentity Provider, string Harness, DashboardSummary Summary);
public sealed record DashboardBucket(DateTimeOffset Date, DashboardSummary Summary);
public sealed record DashboardReport(IReadOnlyList<RequestRecord> Records, IReadOnlyList<ProviderIdentity> Providers, IReadOnlyList<string> Harnesses, IReadOnlyList<string> Models, DashboardSummary Summary, IReadOnlyList<DashboardGroup> Groups, IReadOnlyList<DashboardBucket> Trends)
{
    // What each filter can still be set to, given the other two. Choosing a harness narrows the models
    // and providers to the ones it has calls with, and the same in every direction, so no offered choice
    // leads to an empty result. A dimension's own filter does not narrow it. Mirrors Dashboard.swift.
    public IReadOnlyList<string> HarnessOptions { get; init; } = Array.Empty<string>();
    public IReadOnlyList<ProviderIdentity> ProviderOptions { get; init; } = Array.Empty<ProviderIdentity>();
    public IReadOnlyList<string> ModelOptions { get; init; } = Array.Empty<string>();
    public static DashboardReport Create(IEnumerable<RequestRecord> source, DashboardFilter? filter = null)
    {
        filter ??= new();
        var seen = new HashSet<string>(StringComparer.Ordinal);
        var window = source.Where(r => seen.Add(r.DedupKey) && filter.IncludesDate(r)).ToArray();
        var identities = window.Select(ProviderIdentity.From).GroupBy(p => p.Key).Select(g => g.Aggregate((a, b) => a.Merge(b))).OrderBy(p => p.Name, StringComparer.Ordinal).ThenBy(p => p.Key, StringComparer.Ordinal).ToArray();
        var records = window.Where(filter.Includes).OrderByDescending(r => r.StartedAt).ThenBy(r => r.Id).ToArray();
        var groups = records.GroupBy(r => (ProviderIdentity.From(r).Key, r.Harness)).Select(g => new DashboardGroup(g.Select(ProviderIdentity.From).Aggregate((a, b) => a.Merge(b)), g.Key.Harness, DashboardSummary.From(g))).OrderBy(g => g.Provider.Name, StringComparer.Ordinal).ThenBy(g => g.Harness, StringComparer.Ordinal).ToArray();
        var trends = Array.Empty<DashboardBucket>();
        if (records.Length > 0)
        {
            var span = (records[0].StartedAt - records[^1].StartedAt).TotalSeconds;
            var interval = span <= 48 * 3600 ? 3600 : span <= 119 * 86400 ? 86400 : Math.Max(1, Math.Ceiling((span / (7 * 86400) + 1) / 119)) * 7 * 86400;
            var anchor = interval > 86400 ? 4 * 86400 : 0;
            trends = records.GroupBy(r => DateTimeOffset.FromUnixTimeSeconds((long)(anchor + Math.Floor((r.StartedAt.ToUnixTimeSeconds() - anchor) / interval) * interval))).OrderBy(g => g.Key).Select(g => new DashboardBucket(g.Key, DashboardSummary.From(g))).ToArray();
        }
        bool Harness(RequestRecord r) => filter.Harness == null || filter.Harness == r.Harness;
        bool Provider(RequestRecord r) => filter.Provider == null || filter.Provider == ProviderIdentity.From(r).Key;
        bool Model(RequestRecord r) => filter.Model == null || filter.Model == r.Model;
        var providerFacet = window.Where(r => Harness(r) && Model(r)).Select(r => ProviderIdentity.From(r).Key).ToHashSet(StringComparer.Ordinal);
        return new(Array.AsReadOnly(records), Array.AsReadOnly(identities), Array.AsReadOnly(window.Select(r => r.Harness).Distinct().Order(StringComparer.Ordinal).ToArray()), Array.AsReadOnly(window.Select(r => r.Model).Distinct().Order(StringComparer.Ordinal).ToArray()), DashboardSummary.From(records), Array.AsReadOnly(groups), Array.AsReadOnly(trends))
        {
            HarnessOptions = Array.AsReadOnly(window.Where(r => Provider(r) && Model(r)).Select(r => r.Harness).Distinct().Order(StringComparer.Ordinal).ToArray()),
            ProviderOptions = Array.AsReadOnly(identities.Where(p => providerFacet.Contains(p.Key)).ToArray()),
            ModelOptions = Array.AsReadOnly(window.Where(r => Harness(r) && Provider(r)).Select(r => r.Model).Distinct().Order(StringComparer.Ordinal).ToArray())
        };
    }
}

public sealed record LiveCall(string Id, string Harness, string Model, string Provider, string Phase, DateTimeOffset StartedAt, DateTimeOffset LastActivity, double? TTFT = null, double? Rate = null, int? OutputTokens = null, bool Estimated = false);
public sealed record HarnessStatus(string Name, bool HasLogs, bool IsRunning, string? Limitation = null, bool IsInstalled = false);
public sealed record FlowSample(int Pid, string Harness, string Host, ulong Received, ulong Sent, string? ConnectionID = null);
public interface IFlowSampler : IDisposable
{
    bool Available { get; }
    string Status { get; }
    IReadOnlyList<FlowSample> Sample();
}
