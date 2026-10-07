using System.Text;
using System.Text.Json;

namespace SpeedTracker.Proxy;

internal sealed class StreamMeasurement
{
    private const int Limit = 2 * 1024 * 1024;
    private readonly MemoryStream pending = new();
    private bool whole, ignoring, reasoningSeen;
    private long chars;
    private int? reported;
    public string Model { get; private set; } = "unknown";
    public string Format { get; private set; } = "unknown";
    public bool Recognized => Format != "unknown";
    public bool Streamed { get; private set; }
    public bool Failed { get; private set; }
    public double? First { get; private set; }
    public double? Visible { get; private set; }
    public double? Last { get; private set; }
    public int? Input { get; private set; }
    public int? Cache { get; private set; }
    public int? Reasoning { get; private set; }
    public int Output => reported ?? (int)Math.Min(int.MaxValue, Math.Ceiling(chars / 4d));
    public bool Estimated => !reported.HasValue;
    public double RateTokens => !reasoningSeen && Reasoning is > 0 && Reasoning < Output ? Output - Reasoning.Value : Output;
    public double? LiveRate => First.HasValue && Last - First >= .05 ? RateTokens / (Last - First) : null;

    public void SetContentType(string type) { whole = type.Contains("json", StringComparison.OrdinalIgnoreCase) && !type.Contains("ndjson", StringComparison.OrdinalIgnoreCase); }
    public void Ingest(ReadOnlySpan<byte> bytes, double time)
    {
        if (ignoring) return;
        if (pending.Length == 0 && bytes.Length > 0 && bytes[0] is (byte)'d' or (byte)'e') whole = false;
        if (whole)
        {
            if (pending.Length + bytes.Length > Limit) { ignoring = true; pending.SetLength(0); return; }
            pending.Write(bytes); return;
        }
        foreach (var value in bytes)
        {
            if (value == '\n') { Consume(time); pending.SetLength(0); }
            else if (pending.Length < Limit) pending.WriteByte(value);
            else { ignoring = true; pending.SetLength(0); return; }
        }
    }
    public void Complete(double time)
    {
        if (ignoring || pending.Length == 0) return;
        if (whole) Parse(pending.ToArray(), null); else Consume(time);
        pending.SetLength(0);
    }
    private void Consume(double time)
    {
        var line = Encoding.UTF8.GetString(pending.GetBuffer(), 0, (int)pending.Length).Trim();
        if (line.StartsWith("data:", StringComparison.Ordinal)) line = line[5..].TrimStart();
        if (line.StartsWith('{')) { Streamed = true; Parse(Encoding.UTF8.GetBytes(line), time); }
    }
    private void Mark(string? value, double? time, bool reasoning = false, bool signal = false)
    {
        var count = value?.Length ?? 0;
        if (count == 0 && !signal) return;
        chars += count; reasoningSeen |= reasoning;
        if (!time.HasValue) return;
        First ??= time; Last = time;
        if (!reasoning) Visible ??= time;
    }
    private void Usage(JsonElement usage, string input = "input_tokens", string output = "output_tokens")
    {
        Input = Int(usage, input) ?? Input; reported = Int(usage, output) ?? reported;
        Cache = Int(Get(usage, "input_tokens_details"), "cached_tokens") ?? Int(Get(usage, "prompt_tokens_details"), "cached_tokens") ?? Int(usage, "cache_read_input_tokens") ?? Int(usage, "prompt_cache_hit_tokens") ?? Cache;
        Reasoning = Int(Get(usage, "output_tokens_details"), "reasoning_tokens") ?? Int(Get(usage, "completion_tokens_details"), "reasoning_tokens") ?? Reasoning;
    }
    private void Parse(byte[] bytes, double? time)
    {
        try
        {
            using var doc = JsonDocument.Parse(bytes);
            var row = doc.RootElement; var type = Text(row, "type") ?? "";
            Model = Text(row, "model") ?? Model;
            if (Get(row, "error").ValueKind is not (JsonValueKind.Undefined or JsonValueKind.Null)) Failed = true;
            if (type.StartsWith("response.", StringComparison.Ordinal) || Text(row, "object") == "response")
            {
                Format = "openai-responses";
                var response = Get(row, "response"); if (response.ValueKind == JsonValueKind.Undefined) response = row;
                Model = Text(response, "model") ?? Model; Usage(Get(response, "usage"));
                if (type == "response.output_item.added")
                {
                    var item = Get(row, "item"); var itemType = Text(item, "type");
                    if (itemType == "reasoning") Mark(null, time, true, true);
                    else if (itemType?.Contains("call") == true) Mark(Text(item, "name"), time, signal: true);
                }
                else if (type.EndsWith(".delta", StringComparison.Ordinal) && (!type.Contains("audio") || type.Contains("transcript"))) Mark(Text(row, "delta"), time, type.Contains("reasoning"));
                if (type == "response.failed") Failed = true;
            }
            else if (type is "message_start" or "content_block_start" or "content_block_delta" or "message_delta" or "message_stop" || Text(row, "type") == "message")
            {
                Format = "anthropic";
                var message = type == "message_start" ? Get(row, "message") : row;
                Model = Text(message, "model") ?? Model;
                Usage(Get(message, "usage")); Usage(Get(row, "usage"));
                if (type == "content_block_start")
                {
                    var block = Get(row, "content_block"); var kind = Text(block, "type");
                    Mark(Text(block, "text") ?? Text(block, "thinking") ?? Text(block, "name"), time, kind is "thinking" or "redacted_thinking", true);
                }
                if (type == "content_block_delta")
                {
                    var delta = Get(row, "delta"); Mark(Text(delta, "text") ?? Text(delta, "thinking") ?? Text(delta, "partial_json"), time, Text(delta, "type") == "thinking_delta");
                }
                if (time == null) foreach (var part in Items(Get(row, "content"))) Mark(Text(part, "text") ?? Text(part, "thinking"), null, Text(part, "type") == "thinking");
            }
            else if (Get(row, "choices").ValueKind == JsonValueKind.Array)
            {
                Format = "openai-chat";
                foreach (var choice in Items(Get(row, "choices")))
                {
                    var delta = Get(choice, "delta"); if (delta.ValueKind == JsonValueKind.Undefined) delta = Get(choice, "message");
                    Mark(Text(delta, "reasoning_content") ?? Text(delta, "reasoning"), time, true);
                    Mark(Text(delta, "content") ?? Text(choice, "text"), time);
                    foreach (var tool in Items(Get(delta, "tool_calls"))) { var function = Get(tool, "function"); Mark((Text(function, "name") ?? "") + (Text(function, "arguments") ?? ""), time); }
                }
                Usage(Get(row, "usage"), "prompt_tokens", "completion_tokens");
            }
            else if (Get(row, "candidates").ValueKind == JsonValueKind.Array || Get(row, "usageMetadata").ValueKind == JsonValueKind.Object)
            {
                Format = "gemini"; Model = Text(row, "modelVersion") ?? Model;
                foreach (var candidate in Items(Get(row, "candidates"))) foreach (var part in Items(Get(Get(candidate, "content"), "parts")))
                {
                    Mark(Text(part, "text"), time, Get(part, "thought").ValueKind == JsonValueKind.True);
                    if (Get(part, "functionCall").ValueKind == JsonValueKind.Object) Mark(Get(part, "functionCall").GetRawText(), time);
                }
                var usage = Get(row, "usageMetadata"); Input = Int(usage, "promptTokenCount") ?? Input; Cache = Int(usage, "cachedContentTokenCount") ?? Cache;
                Reasoning = Int(usage, "thoughtsTokenCount") ?? Reasoning;
                if (Int(usage, "candidatesTokenCount") is int count) reported = (int)Math.Min(int.MaxValue, (long)count + (Reasoning ?? 0));
            }
            else if (Get(row, "done").ValueKind is JsonValueKind.True or JsonValueKind.False)
            {
                Format = "ollama"; var message = Get(row, "message"); Mark(Text(message, "thinking"), time, true); Mark(Text(message, "content") ?? Text(row, "response"), time);
                Input = Int(row, "prompt_eval_count") ?? Input; reported = Int(row, "eval_count") ?? reported;
            }
        }
        catch (JsonException) { }
    }
    private static JsonElement Get(JsonElement row, string key) => row.ValueKind == JsonValueKind.Object && row.TryGetProperty(key, out var value) ? value : default;
    private static string? Text(JsonElement row, string key) => Get(row, key).ValueKind == JsonValueKind.String ? Get(row, key).GetString() : null;
    private static int? Int(JsonElement row, string key) => Get(row, key).TryGetInt32Safe();
    private static IEnumerable<JsonElement> Items(JsonElement row) => row.ValueKind == JsonValueKind.Array ? row.EnumerateArray() : Enumerable.Empty<JsonElement>();
}
internal static class JsonNumbers
{
    internal static int? TryGetInt32Safe(this JsonElement value) => value.ValueKind == JsonValueKind.Number && value.TryGetInt32(out var number) && number >= 0 ? number : null;
}
