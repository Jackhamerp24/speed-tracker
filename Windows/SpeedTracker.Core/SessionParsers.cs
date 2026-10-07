using System.Globalization;
using System.Text.Json;

namespace SpeedTracker.Core;

internal static class J
{
    public static JsonElement Get(this JsonElement value, string name) => value.ValueKind == JsonValueKind.Object && value.TryGetProperty(name, out var child) ? child : default;
    public static string? Text(this JsonElement value) => value.ValueKind == JsonValueKind.String ? value.GetString() : null;
    public static string? S(this JsonElement value, string name) => value.Get(name).Text();
    public static double? Number(this JsonElement value) => value.ValueKind == JsonValueKind.Number && value.TryGetDouble(out var number) && double.IsFinite(number) ? number : null;
    public static double? N(this JsonElement value, string name) => value.Get(name).Number();
    public static int? I(this JsonElement value, string name) => value.Get(name).ValueKind == JsonValueKind.Number && value.Get(name).TryGetInt32(out var number) && number >= 0 ? number : null;
    public static bool B(this JsonElement value, string name) => value.Get(name).ValueKind == JsonValueKind.True;
    public static IEnumerable<JsonElement> Items(this JsonElement value) => value.ValueKind == JsonValueKind.Array ? value.EnumerateArray() : Enumerable.Empty<JsonElement>();
    public static DateTimeOffset? Date(this JsonElement value)
    {
        if (value.Text() is string text && DateTimeOffset.TryParse(text, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var date)) return date;
        return value.Number() is double number && number > 0 && number < 253402300799999 ? DateTimeOffset.FromUnixTimeMilliseconds((long)number) : null;
    }
    public static DateTimeOffset? Min(DateTimeOffset? a, DateTimeOffset? b) => a == null ? b : b == null ? a : a < b ? a : b;
    public static int? Add(int? first, int? second = null, int? third = null)
    {
        if (first == null && second == null && third == null || first < 0 || second < 0 || third < 0) return null;
        var sum = (long)(first ?? 0) + (second ?? 0) + (third ?? 0);
        return sum <= int.MaxValue ? (int)sum : null;
    }
}

public sealed record LogRecord(string Key, string Harness, string Model, string Provider, DateTimeOffset? RequestStart, DateTimeOffset? FirstToken, DateTimeOffset? FirstVisible, DateTimeOffset End, int OutputTokens, int? ReasoningTokens = null, int? InputTokens = null, int? CachedInputTokens = null, bool Aborted = false, double? Speed = null)
{
    public RequestRecord MakeRecord()
    {
        double? ttft = RequestStart.HasValue && FirstToken.HasValue ? (FirstToken.Value - RequestStart.Value).TotalSeconds : null;
        if (ttft is < 0 or >= 3600) ttft = null;
        double? generation = FirstToken.HasValue && (RequestStart == null || ttft != null) ? Math.Max(0, (End - FirstToken.Value).TotalSeconds) : null;
        var total = RequestStart.HasValue ? Math.Max(0, (End - RequestStart.Value).TotalSeconds) : generation ?? 0;
        double? rate = generation >= .05 ? OutputTokens / generation : total >= .05 ? OutputTokens / total : null;
        // A harness that measured the call itself, where "all output between first token and end" does not fit.
        if (Speed is double measured && double.IsFinite(measured) && measured > 0) rate = measured;
        return new RequestRecord
        {
            StartedAt = RequestStart ?? FirstToken ?? End, Harness = Harness, Route = Provider, UpstreamHost = Provider,
            Format = "unknown", Model = Model, Streamed = true, Status = Aborted ? 499 : 200,
            Ttft = ttft, FirstVisible = RequestStart.HasValue && FirstVisible >= RequestStart ? (FirstVisible.Value - RequestStart.Value).TotalSeconds : null,
            Generation = generation, Total = total, InputTokens = InputTokens, CachedInputTokens = CachedInputTokens,
            OutputTokens = Math.Max(0, OutputTokens), ReasoningTokens = ReasoningTokens, TokensEstimated = false, Tps = rate,
            Aborted = Aborted, Source = "log", SourceKey = Key
        };
    }
}

public abstract class SessionLogParser
{
    public abstract string Harness { get; }
    public string? CurrentModel { get; protected set; }
    public string CurrentProvider { get; protected set; } = "";
    public bool AwaitingResponse { get; protected set; }
    public DateTimeOffset? LastRequestAt { get; protected set; }
    public DateTimeOffset? FirstTokenAt { get; protected set; }
    public DateTimeOffset? FirstVisibleAt { get; protected set; }
    public bool Supported { get; protected set; }
    public string? Limitation { get; protected set; }
    public abstract IReadOnlyList<LogRecord> Ingest(JsonElement entry);
    public IReadOnlyList<LogRecord> Ingest(string json)
    {
        using var document = JsonDocument.Parse(json);
        return Ingest(document.RootElement);
    }
    public virtual IReadOnlyList<LogRecord> Flush(TimeSpan idle) => Array.Empty<LogRecord>();
}

public sealed class ClaudeCodeLogParser : SessionLogParser
{
    public override string Harness => "Claude Code";
    private sealed record Pending(string Id, string Model, DateTimeOffset? Start, DateTimeOffset? First, DateTimeOffset End, int Output = 0, int? Input = null, int? Cache = null);
    private readonly Dictionary<bool, Pending> pending = new();
    private readonly Dictionary<bool, DateTimeOffset> input = new();
    public override IReadOnlyList<LogRecord> Ingest(JsonElement entry)
    {
        var side = entry.B("isSidechain"); var type = entry.S("type"); var time = entry.Get("timestamp").Date();
        if (type == "user")
        {
            Supported = true;
            if (time.HasValue) input[side] = time.Value;
            if (!side) { AwaitingResponse = true; LastRequestAt = time; FirstTokenAt = null; FirstVisibleAt = null; }
            return Array.Empty<LogRecord>();
        }
        if (type == "system") { if (!side) AwaitingResponse = false; return Finalize(side); }
        var message = entry.Get("message");
        if (type != "assistant" || time == null || message.S("id") is not string id || message.S("model") is not string model || model == "<synthetic>") return Array.Empty<LogRecord>();
        Supported = true;
        var finished = pending.TryGetValue(side, out var previous) && previous.Id != id ? Finalize(side) : Array.Empty<LogRecord>();
        var firstLine = !pending.ContainsKey(side);
        var current = pending.GetValueOrDefault(side) ?? new Pending(id, model, input.TryGetValue(side, out var start) ? start : null, null, time.Value);
        var usage = message.Get("usage");
        var first = current.First;
        if (firstLine && message.Get("content").Items().FirstOrDefault().S("type") == "thinking" && entry.N("thinkingDurationMs") is double duration) first = time.Value.AddMilliseconds(-duration);
        pending[side] = current with { End = time > current.End ? time.Value : current.End, First = first, Output = Math.Max(current.Output, usage.I("output_tokens") ?? 0), Input = J.Add(usage.I("input_tokens"), usage.I("cache_read_input_tokens"), usage.I("cache_creation_input_tokens")) ?? current.Input, Cache = usage.I("cache_read_input_tokens") ?? current.Cache };
        if (!side) { CurrentModel = model; CurrentProvider = "anthropic"; AwaitingResponse = false; }
        return finished;
    }
    public override IReadOnlyList<LogRecord> Flush(TimeSpan idle) => idle.TotalSeconds >= 3 ? Finalize(false).Concat(Finalize(true)).ToArray() : Array.Empty<LogRecord>();
    private IReadOnlyList<LogRecord> Finalize(bool side)
    {
        if (!pending.Remove(side, out var done) || done.Output <= 0) return Array.Empty<LogRecord>();
        return new[] { new LogRecord("claude:" + done.Id, Harness, done.Model, "anthropic", done.Start, done.First < done.Start ? null : done.First, null, done.End, done.Output, InputTokens: done.Input, CachedInputTokens: done.Cache) };
    }
}

public sealed class CodexLogParser : SessionLogParser
{
    public override string Harness => "Codex";
    private string session = "unknown";
    private bool sawUsage;
    private int? lastTotal;
    private int index;
    public CodexLogParser() { CurrentProvider = "openai"; }
    public override IReadOnlyList<LogRecord> Ingest(JsonElement entry)
    {
        var type = entry.S("type"); var payload = entry.Get("payload"); var time = entry.Get("timestamp").Date();
        switch (type)
        {
            case "session_meta":
                Supported = true; session = payload.S("session_id") ?? payload.S("id") ?? session; CurrentProvider = payload.S("model_provider") ?? CurrentProvider; break;
            case "turn_context": Supported = true; CurrentModel = payload.S("model") ?? CurrentModel; break;
            case "response_item":
                var kind = payload.S("type") ?? "";
                if ((kind == "message" && payload.S("role") == "user") || kind.EndsWith("_output", StringComparison.Ordinal))
                { Supported = true; AwaitingResponse = true; LastRequestAt = time; FirstTokenAt = null; FirstVisibleAt = null; }
                break;
            case "event_msg":
                if (payload.S("type") == "item_completed")
                {
                    var item = payload.Get("item").S("type")?.ToLowerInvariant();
                    var started = payload.Get("started_at_ms").Date();
                    if (item is "reasoning" or "agentmessage" or "agent_message" && started != null && (LastRequestAt == null || started >= LastRequestAt.Value.AddSeconds(-.5)))
                    { FirstTokenAt = J.Min(FirstTokenAt, started); if (item != "reasoning") FirstVisibleAt = J.Min(FirstVisibleAt, started); }
                }
                else if (payload.S("type") is "task_complete" or "turn_aborted") AwaitingResponse = false;
                else if (payload.S("type") == "token_count" && !sawUsage)
                {
                    var info = payload.Get("info"); var total = info.Get("total_token_usage").I("total_tokens");
                    if (total != lastTotal) { lastTotal = total; return Emit(info.Get("last_token_usage"), $"codex:{session}:{++index}", time); }
                }
                break;
            case "token_usage_record":
                Supported = true; sawUsage = true;
                return Emit(payload.Get("usage"), "codex:" + (payload.S("response_id") ?? $"{session}:{++index}"), time);
        }
        return Array.Empty<LogRecord>();
    }
    private IReadOnlyList<LogRecord> Emit(JsonElement usage, string key, DateTimeOffset? time)
    {
        var result = time.HasValue && usage.I("output_tokens") is int output && output > 0 ? new[] { new LogRecord(key, Harness, CurrentModel ?? "unknown", CurrentProvider, AwaitingResponse ? LastRequestAt : null, FirstTokenAt, FirstVisibleAt, time.Value, output, usage.I("reasoning_output_tokens"), usage.I("input_tokens"), usage.I("cached_input_tokens")) } : Array.Empty<LogRecord>();
        AwaitingResponse = false; FirstTokenAt = null; FirstVisibleAt = null;
        return result;
    }
}

public sealed class OMPLogParser : SessionLogParser
{
    public override string Harness { get; }
    public OMPLogParser(string harness = "OMP") { Harness = harness; }
    public override IReadOnlyList<LogRecord> Ingest(JsonElement entry)
    {
        var type = entry.S("type");
        if (type == "model_change") { Supported = true; CurrentModel = (entry.S("model") ?? entry.S("modelId"))?.Split('/').Last(); CurrentProvider = entry.S("provider") ?? CurrentProvider; return Array.Empty<LogRecord>(); }
        if (type != "message") return Array.Empty<LogRecord>();
        var message = entry.Get("message"); var role = message.S("role");
        if (role == null) return Array.Empty<LogRecord>();
        Supported = true; AwaitingResponse = role is "user" or "toolResult";
        if (AwaitingResponse) { LastRequestAt = entry.Get("timestamp").Date(); FirstTokenAt = null; FirstVisibleAt = null; }
        var usage = message.Get("usage"); var output = usage.I("output");
        if (role != "assistant" || output is not > 0) return Array.Empty<LogRecord>();
        CurrentModel = message.S("model") ?? CurrentModel ?? "unknown"; CurrentProvider = message.S("provider") ?? "";
        var written = entry.Get("timestamp").Date(); var duration = message.N("duration"); var start = message.Get("timestamp").Date();
        if (start == null && written.HasValue && duration.HasValue) start = written.Value.AddMilliseconds(-duration.Value);
        var end = start.HasValue && duration.HasValue ? start.Value.AddMilliseconds(duration.Value) : written;
        if (end == null) return Array.Empty<LogRecord>();
        var first = start.HasValue && message.N("ttft") is double ttft ? start.Value.AddMilliseconds(ttft) : (DateTimeOffset?)null;
        var id = entry.S("id") ?? message.S("responseId") ?? end.Value.ToUnixTimeMilliseconds().ToString(CultureInfo.InvariantCulture);
        return new[] { new LogRecord(Harness.ToLowerInvariant() + ":" + id, Harness, CurrentModel, CurrentProvider, start, first, null, end.Value, output.Value, usage.I("reasoningTokens"), J.Add(usage.I("input"), usage.I("cacheRead"), usage.I("cacheWrite")), usage.I("cacheRead"), message.S("stopReason") is "aborted" or "error") };
    }
}

// Gemini CLI: ~/.gemini/tmp/<project>/chats/session-*.jsonl. The first line is session metadata and
// {"$set": …} lines update it. Every other line is a message, written again whole each time it changes.
// A gemini message is created when its stream ends, so its timestamp is the end of the response; each
// thought carries its arrival time. Requests after tool calls go out when the tools finish.
public sealed class GeminiCliLogParser : SessionLogParser
{
    public override string Harness => "Gemini CLI";
    private readonly HashSet<string> emitted = new(StringComparer.Ordinal);
    // The last request time has been matched to a response; the next response needs a new one.
    private bool requestUsed = true;
    public GeminiCliLogParser() { CurrentProvider = "google"; }
    public override IReadOnlyList<LogRecord> Ingest(JsonElement entry)
    {
        var update = entry.Get("$set");
        if (update.ValueKind == JsonValueKind.Object) { Supported = true; return update.Get("messages").Items().SelectMany(Message).ToArray(); }
        if (entry.Get("$rewindTo").ValueKind == JsonValueKind.String) { AwaitingResponse = false; requestUsed = true; return Array.Empty<LogRecord>(); }
        if (entry.S("sessionId") != null && entry.S("id") == null) { Supported = true; return Array.Empty<LogRecord>(); }
        return Message(entry);
    }
    private IReadOnlyList<LogRecord> Message(JsonElement message)
    {
        var id = message.S("id"); var type = message.S("type");
        if (id == null || type == null || message.Get("timestamp").Date() is not DateTimeOffset time) return Array.Empty<LogRecord>();
        Supported = true;
        if (type == "user") { NoteRequest(time); return Array.Empty<LogRecord>(); }
        if (type == "error") { AwaitingResponse = false; return Array.Empty<LogRecord>(); }
        if (type != "gemini") return Array.Empty<LogRecord>();
        if (message.S("model") is { Length: > 0 } model) CurrentModel = model;
        var records = Array.Empty<LogRecord>();
        if (!emitted.Contains(id) && Record(id, message, time) is LogRecord record) { emitted.Add(id); requestUsed = true; records = new[] { record }; }
        // The message is written again once its tools have run; the next request leaves then.
        DateTimeOffset? finished = null;
        foreach (var call in message.Get("toolCalls").Items()) if (call.Get("timestamp").Date() is DateTimeOffset done && (finished == null || done > finished)) finished = done;
        if (finished >= time) { if (LastRequestAt == null || finished > LastRequestAt) NoteRequest(finished.Value); }
        else AwaitingResponse = false;
        return records;
    }
    private void NoteRequest(DateTimeOffset time) { LastRequestAt = time; requestUsed = false; AwaitingResponse = true; FirstTokenAt = null; FirstVisibleAt = null; }
    private LogRecord? Record(string id, JsonElement message, DateTimeOffset end)
    {
        // Tokens can be missing on the first write of a message and arrive with a later one.
        var tokens = message.Get("tokens");
        if (tokens.ValueKind != JsonValueKind.Object) return null;
        var thoughts = Math.Max(0, tokens.I("thoughts") ?? 0);
        // Gemini counts thinking apart from the visible reply; the response produced both.
        var output = Math.Max(0, tokens.I("output") ?? 0) + thoughts;
        if (output <= 0) return null;
        DateTimeOffset? start = !requestUsed && LastRequestAt is DateTimeOffset requested && requested <= end && end - requested < TimeSpan.FromHours(1) ? requested : null;
        DateTimeOffset? first = null;
        foreach (var thought in message.Get("thoughts").Items()) first = J.Min(first, thought.Get("timestamp").Date());
        if (first > end || (start != null && first < start)) first = null;
        return new LogRecord("gemini:" + id, Harness, CurrentModel ?? "unknown", "google", start, first, null, end, output, thoughts > 0 ? thoughts : null, tokens.I("input"), tokens.I("cached"));
    }
}

// Official deepseek-ai/deepseek-harness (dsh), not a guess based on model vendor.
public sealed class DeepSeekLogParser : SessionLogParser
{
    public override string Harness => "DeepSeek CLI";
    public int? Version { get; private set; }
    public bool HasSupportedHeader { get; private set; }
    public long? InheritedCut { get; private set; }
    public bool NeedsInheritedScan => HasSupportedHeader && seeded && Version >= 2 && knownInheritedCut == null;
    private readonly long? knownInheritedCut;
    private readonly int? expectedVersion;
    private string session = "";
    private bool seeded;
    private long lastSequence = -1;
    private (Step Step, DateTimeOffset Time)? pending;
    private Step? historicalStep;
    private Timing historicalTiming = new();
    private readonly record struct Step(int Turn, int Index)
    {
        public static Step? Read(JsonElement data) => data.I("turn") is int turn && data.I("step") is int step ? new(turn, step) : null;
    }

    public DeepSeekLogParser(long? inheritedEventCount = null, int? expectedVersion = null)
    {
        knownInheritedCut = inheritedEventCount;
        this.expectedVersion = expectedVersion;
    }

    public override IReadOnlyList<LogRecord> Ingest(JsonElement entry)
    {
        var type = entry.S("type");
        if (type == "session")
        {
            if (Version != null) return Array.Empty<LogRecord>();
            if (entry.I("version") is not int version || string.IsNullOrEmpty(entry.S("id")) || entry.Get("createdAt").Date() == null)
            { Limitation = "Invalid DeepSeek session header."; return Array.Empty<LogRecord>(); }
            Version = version;
            if (version > 4 || expectedVersion != null && expectedVersion != version)
            { Limitation = $"Unsupported DeepSeek session generation v{version}."; return Array.Empty<LogRecord>(); }
            if (version >= 2)
            {
                if (entry.Get("isSeeded").ValueKind is not (JsonValueKind.True or JsonValueKind.False))
                { Limitation = "DeepSeek session header lacks isSeeded."; return Array.Empty<LogRecord>(); }
                seeded = entry.B("isSeeded");
                InheritedCut = knownInheritedCut;
            }
            else
            {
                seeded = entry.Get("seedLength").ValueKind != JsonValueKind.Undefined;
                if (seeded && Count(entry.Get("seedLength")) == null)
                { Limitation = "Invalid DeepSeek inherited seed length."; return Array.Empty<LogRecord>(); }
                InheritedCut = Count(entry.Get("seedLength")) ?? 0;
            }
            session = entry.S("id")!;
            HasSupportedHeader = true;
            if (NeedsInheritedScan) Limitation = "DeepSeek seeded log has no complete inherited boundary.";
            return Array.Empty<LogRecord>();
        }
        if (!HasSupportedHeader || entry.Get("data").ValueKind != JsonValueKind.Object) return Array.Empty<LogRecord>();
        var data = entry.Get("data");
        var packed = type is "text-chunks" or "reasoning-chunks" or "tool-call-chunks";
        var sequence = Count(entry.Get(packed ? "seq0" : "seq"));
        var time = entry.Get(packed ? "time0" : "time").Date();
        if (sequence == null || sequence <= lastSequence || time == null) return Array.Empty<LogRecord>();
        var members = data.Get(type == "tool-call-chunks" ? "args" : "texts");
        var packedCount = 1;
        if (packed)
        {
            if (Version >= 2 || members.ValueKind != JsonValueKind.Array || members.GetArrayLength() == 0 || members.Items().Any(v => v.ValueKind != JsonValueKind.String)) return Array.Empty<LogRecord>();
            packedCount = members.GetArrayLength();
        }
        lastSequence = sequence.Value + packedCount - 1;
        if (type == "session/end-seed" && data.B("inherited")) InheritedCut = Math.Max(InheritedCut ?? 0, sequence.Value);
        if (NeedsInheritedScan || InheritedCut is long cut && sequence + packedCount <= cut) return Array.Empty<LogRecord>();
        var step = Step.Read(data);
        switch (type)
        {
            case "step/start" when step != null:
                pending = (step.Value, time.Value);
                historicalStep = step;
                historicalTiming = new();
                LastRequestAt = time;
                FirstTokenAt = null;
                FirstVisibleAt = null;
                AwaitingResponse = true;
                Supported = true;
                break;
            case "request/header":
                SetModel(data.Get("header").Get("config"));
                break;
            case "request/context":
                SetModel(data);
                break;
            case "assistant/chunk" when Version < 2 && step != null:
                if (historicalStep != step) { historicalTiming = new(); historicalStep = step; }
                historicalTiming.Chunk(data.Get("chunk"), time.Value);
                UpdateLiveTiming(step.Value);
                break;
            case "text-chunks" or "reasoning-chunks" or "tool-call-chunks" when step != null:
                if (historicalStep != step) { historicalTiming = new(); historicalStep = step; }
                historicalTiming.Packed(type!, entry.Get("time0"), data, Math.Max(0, (InheritedCut ?? 0) - sequence.Value));
                UpdateLiveTiming(step.Value);
                break;
            case "assistant/attempt" or "llm/retry":
                if (step != null && pending?.Step == step) ClearPending();
                historicalTiming = new();
                historicalStep = null;
                Supported = true;
                break;
            case "assistant/message" when step != null:
                var request = pending?.Step == step ? pending?.Time : null;
                if (pending?.Step == step) ClearPending();
                var timing = Version < 2 && historicalStep == step ? historicalTiming : new Timing();
                historicalTiming = new();
                historicalStep = null;
                foreach (var row in data.Get("stream").Items()) timing.StreamRecord(row);
                var message = data.Get("message");
                var source = message.Get("source");
                string? id;
                if (Version == 0 && message.ValueKind != JsonValueKind.Object && data.Get("provenance").ValueKind == JsonValueKind.Object)
                {
                    source = data.Get("provenance");
                    id = $"legacy-message:{session}:{sequence}";
                }
                else
                {
                    if (message.S("role") != "assistant" || source.S("kind") != "model") return Array.Empty<LogRecord>();
                    id = message.S("id");
                }
                if (string.IsNullOrEmpty(id) || string.IsNullOrEmpty(source.S("model")) || source.S("provider") == null ||
                    entry.Get("surfaceOp").ValueKind != JsonValueKind.Undefined && entry.S("surfaceOp") != "append") return Array.Empty<LogRecord>();
                CurrentModel = source.S("model");
                CurrentProvider = source.S("provider")!;
                Supported = true;
                var usage = data.Get("usage").ValueKind == JsonValueKind.Object ? Usage.Read(data.Get("usage")) : timing.Usage;
                if (usage?.Output == null) return Array.Empty<LogRecord>();
                var end = timing.Finish <= time ? timing.Finish!.Value : time.Value;
                return new[] { new LogRecord("deepseek:" + id, Harness, CurrentModel!, CurrentProvider,
                    request <= end ? request : null, ValidTiming(timing.First, request, end), ValidTiming(timing.Visible, request, end),
                    end, usage.Output.Value, usage.Reasoning, usage.Input, usage.Cache, data.B("interrupted") || timing.Aborted) };
            case "step/end":
                if (step != null && pending?.Step == step) ClearPending();
                historicalTiming = new();
                historicalStep = null;
                break;
            case "turn/end" or "session/end-seed":
                ClearPending();
                historicalTiming = new();
                historicalStep = null;
                break;
        }
        return Array.Empty<LogRecord>();
    }

    private void SetModel(JsonElement data)
    {
        if (data.S("model") is string model && model.Length > 0) CurrentModel = model;
        if (data.S("provider") is string provider) CurrentProvider = provider;
    }
    private void ClearPending() { pending = null; AwaitingResponse = false; FirstTokenAt = null; FirstVisibleAt = null; }
    private void UpdateLiveTiming(Step step)
    {
        if (pending?.Step != step) return;
        FirstTokenAt = historicalTiming.First >= pending?.Time ? historicalTiming.First : null;
        FirstVisibleAt = historicalTiming.Visible >= pending?.Time ? historicalTiming.Visible : null;
    }
    private static DateTimeOffset? ValidTiming(DateTimeOffset? date, DateTimeOffset? start, DateTimeOffset end) => date <= end && (start == null || date >= start) ? date : null;
    private static long? Count(JsonElement value) => value.ValueKind == JsonValueKind.Number && value.TryGetInt64(out var number) && number >= 0 && number <= 9_007_199_254_740_991 ? number : null;
    private sealed record Usage(int? Output, int? Reasoning, int? Input, int? Cache)
    {
        public static Usage Read(JsonElement value)
        {
            var input = value.I("inputTokens");
            var cache = value.I("cacheReadTokens");
            return new(value.I("outputTokens"), value.I("reasoningTokens"), input == null ? null : J.Add(input, cache, value.I("cacheWriteTokens")), cache);
        }
    }
    private sealed class Timing
    {
        private DateTimeOffset? first, visible, blockFirst, blockVisible;
        private bool sawDeltas, sawTextDeltas;
        public DateTimeOffset? First => sawDeltas ? first : blockFirst;
        public DateTimeOffset? Visible => sawTextDeltas ? visible : blockVisible;
        public DateTimeOffset? Finish { get; private set; }
        public Usage? Usage { get; private set; }
        public bool Aborted { get; private set; }
        public void Chunk(JsonElement chunk, DateTimeOffset date)
        {
            switch (chunk.S("type"))
            {
                case "text-delta":
                    sawDeltas = true; sawTextDeltas = true;
                    if (chunk.S("text") is string text)
                    {
                        if (text.Length > 0) first = J.Min(first, date);
                        if (!string.IsNullOrWhiteSpace(text)) visible = J.Min(visible, date);
                    }
                    break;
                case "reasoning-delta":
                    sawDeltas = true;
                    if (chunk.S("text") is string reasoning && reasoning.Length > 0) first = J.Min(first, date);
                    break;
                case "tool-call-delta":
                    sawDeltas = true;
                    if (!string.IsNullOrEmpty(chunk.S("argumentsDelta")) || chunk.S("name") != null) first = J.Min(first, date);
                    break;
                case "block-start":
                    if (chunk.S("blockType") is "reasoning" or "text" or "tool-call") blockFirst = J.Min(blockFirst, date);
                    if (chunk.S("blockType") == "text") blockVisible = J.Min(blockVisible, date);
                    break;
                case "usage": Usage = DeepSeekLogParser.Usage.Read(chunk.Get("usage")); break;
                case "finish": Finish = date; Aborted = chunk.Get("reason").S("kind") is "aborted" or "error"; break;
            }
        }
        public void StreamRecord(JsonElement row)
        {
            if (row.S("type") == "chunk" && row.Get("time").Date() is DateTimeOffset time) Chunk(row.Get("chunk"), time);
            else if (row.S("type") is "text-chunks" or "reasoning-chunks" or "tool-call-chunks") Packed(row.S("type")!, row.Get("time0"), row, 0);
        }
        public void Packed(string type, JsonElement time, JsonElement data, long skip)
        {
            var members = data.Get(type == "tool-call-chunks" ? "args" : "texts");
            var gaps = data.Get("dt");
            var stamp = time.Number();
            if (stamp is not > 0 || stamp > 9_007_199_254_740_991 || Math.Truncate(stamp.Value) != stamp ||
                members.ValueKind != JsonValueKind.Array || members.GetArrayLength() == 0 || gaps.ValueKind != JsonValueKind.Array ||
                gaps.GetArrayLength() != members.GetArrayLength() - 1 || members.Items().Any(v => v.ValueKind != JsonValueKind.String)) return;
            sawDeltas = true;
            if (type == "text-chunks") sawTextDeltas = true;
            for (var index = 0; index < members.GetArrayLength(); index++)
            {
                if (index > 0)
                {
                    var gap = gaps[index - 1].Number();
                    if (gap == null || Math.Truncate(gap.Value) != gap || Math.Abs(gap.Value) > 9_007_199_254_740_991) return;
                    stamp += gap;
                }
                if (index < skip || stamp is not > 0 || stamp >= 253402300799999) continue;
                var date = DateTimeOffset.FromUnixTimeMilliseconds((long)stamp.Value);
                var text = members[index].GetString()!;
                if (text.Length > 0 || type == "tool-call-chunks" && data.S("name") != null) first = J.Min(first, date);
                if (type == "text-chunks" && !string.IsNullOrWhiteSpace(text)) visible = J.Min(visible, date);
            }
        }
    }
}
