using System.Diagnostics;
using System.Net;
using System.Net.Http.Headers;
using System.Text.Json;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.Hosting;
using Microsoft.Extensions.Logging;
using SpeedTracker.Core;

namespace SpeedTracker.Proxy;

/// Optional, local-only HTTP reverse proxy. Requests are forwarded untouched, never logged.
public sealed class ProxyService : IAsyncDisposable
{
    private readonly TrackerService tracker;
    private readonly Dictionary<string, string> routes;
    private readonly HttpClient client = new(new SocketsHttpHandler { AllowAutoRedirect = false, AutomaticDecompression = DecompressionMethods.All, UseCookies = false }) { Timeout = Timeout.InfiniteTimeSpan };
    private WebApplication? app;
    public int Port { get; }
    public string Status { get; private set; } = "Stopped";
    private static readonly HashSet<string> HopHeaders = new(StringComparer.OrdinalIgnoreCase) { "Host", "Connection", "Keep-Alive", "Proxy-Authenticate", "Proxy-Authorization", "TE", "Trailer", "Transfer-Encoding", "Upgrade", "Content-Length" };
    public static IReadOnlyDictionary<string, string> DefaultRoutes { get; } = new Dictionary<string, string>
    {
        ["anthropic"]="https://api.anthropic.com", ["openai"]="https://api.openai.com", ["chatgpt"]="https://chatgpt.com/backend-api/codex",
        ["deepseek"]="https://api.deepseek.com", ["openrouter"]="https://openrouter.ai/api", ["gemini"]="https://generativelanguage.googleapis.com",
        ["xai"]="https://api.x.ai", ["groq"]="https://api.groq.com/openai", ["cerebras"]="https://api.cerebras.ai", ["mistral"]="https://api.mistral.ai",
        ["moonshot"]="https://api.moonshot.ai", ["zai"]="https://api.z.ai", ["ollama"]="http://127.0.0.1:11434", ["lmstudio"]="http://127.0.0.1:1234"
    };

    public ProxyService(TrackerService tracker, int port = 4141, IReadOnlyDictionary<string, string>? extraRoutes = null)
    {
        this.tracker = tracker; Port = port; routes = new(DefaultRoutes);
        if (extraRoutes != null) foreach (var pair in extraRoutes) routes[pair.Key] = pair.Value;
    }

    public async Task StartAsync(CancellationToken cancellationToken = default)
    {
        if (app != null) return;
        var builder = WebApplication.CreateSlimBuilder(new WebApplicationOptions { ApplicationName = typeof(ProxyService).Assembly.GetName().Name, Args = Array.Empty<string>() });
        builder.Logging.ClearProviders();
        builder.WebHost.ConfigureKestrel(server => server.Listen(IPAddress.Loopback, Port));
        var server = builder.Build();
        server.Run(Forward);
        try { await server.StartAsync(cancellationToken); app = server; Status = $"Optional proxy on 127.0.0.1:{Port}"; }
        catch { await server.DisposeAsync(); Status = "Optional proxy unavailable; passive detection continues"; throw; }
    }

    private async Task Forward(HttpContext context)
    {
        var request = context.Request;
        if (!IPAddress.IsLoopback(context.Connection.RemoteIpAddress ?? IPAddress.None) || request.Headers.ContainsKey("Origin")
            || !(request.Host.Host is "127.0.0.1" or "localhost" or "[::1]"))
        { context.Response.StatusCode = 403; return; }
        if (request.Headers.ContainsKey("Upgrade")) { context.Response.StatusCode = 400; return; }
        var path = request.Path.Value ?? "/";
        if (path == "/health") { await context.Response.WriteAsJsonAsync(new { port = Port }); return; }
        var slash = path.IndexOf('/', 1);
        var routeTag = path[1..(slash < 0 ? path.Length : slash)];
        var split = routeTag.Split('@', 2);
        var route = split[0];
        var rest = slash < 0 ? "/" : path[slash..];
        string? upstream;
        if (route == "_")
        {
            var hostEnd = rest.IndexOf('/', 1);
            var host = rest[1..(hostEnd < 0 ? rest.Length : hostEnd)];
            if (Uri.CheckHostName(host) == UriHostNameType.Unknown) { context.Response.StatusCode = 400; return; }
            upstream = "https://" + host;
            route = host;
            rest = hostEnd < 0 ? "/" : rest[hostEnd..];
        }
        else if (!routes.TryGetValue(route, out upstream)) { context.Response.StatusCode = 404; return; }
        if (!Uri.TryCreate(upstream.TrimEnd('/') + rest + request.QueryString.Value, UriKind.Absolute, out var destination)
            || destination.Scheme is not ("http" or "https") || destination.UserInfo.Length > 0)
        { context.Response.StatusCode = 400; return; }
        var harness = split.Length == 2 ? NormalizeHarness(split[1]) : GuessHarness(request.Headers.UserAgent.ToString());
        var id = Guid.NewGuid(); var key = "proxy:" + id; var started = DateTimeOffset.UtcNow; var clock = Stopwatch.StartNew();
        var meter = new StreamMeasurement();
        var status = 502; double? headersAt = null; var aborted = false; var isGeneration = request.Method == "POST";
        using var message = new HttpRequestMessage(new HttpMethod(request.Method), destination);
        if (request.ContentLength > 0 || request.Headers.ContainsKey("Transfer-Encoding")) message.Content = new StreamContent(request.Body);
        foreach (var header in request.Headers)
        {
            if (HopHeaders.Contains(header.Key) || header.Key.Equals("Accept-Encoding", StringComparison.OrdinalIgnoreCase)) continue;
            if (!message.Headers.TryAddWithoutValidation(header.Key, header.Value.ToArray())) message.Content?.Headers.TryAddWithoutValidation(header.Key, header.Value.ToArray());
        }
        if (isGeneration) tracker.UpdateProxy(new LiveCall(key, harness, "unknown", route, "Waiting", started, started));
        try
        {
            using var response = await client.SendAsync(message, HttpCompletionOption.ResponseHeadersRead, context.RequestAborted);
            headersAt = clock.Elapsed.TotalSeconds; status = (int)response.StatusCode;
            context.Response.StatusCode = status;
            foreach (var header in response.Headers.Concat(response.Content.Headers))
                if (!HopHeaders.Contains(header.Key) && !header.Key.Equals("Content-Encoding", StringComparison.OrdinalIgnoreCase)) context.Response.Headers[header.Key] = header.Value.ToArray();
            var type = response.Content.Headers.ContentType?.MediaType ?? "";
            meter.SetContentType(type);
            await using var stream = await response.Content.ReadAsStreamAsync(context.RequestAborted);
            var buffer = new byte[16 * 1024];
            while (true)
            {
                var count = await stream.ReadAsync(buffer, context.RequestAborted);
                if (count == 0) break;
                var elapsed = clock.Elapsed.TotalSeconds;
                meter.Ingest(buffer.AsSpan(0, count), elapsed);
                await context.Response.Body.WriteAsync(buffer.AsMemory(0, count), context.RequestAborted);
                await context.Response.Body.FlushAsync(context.RequestAborted);
                if (isGeneration && meter.Recognized)
                    tracker.UpdateProxy(new LiveCall(key, harness, meter.Model, route, meter.First == null ? "Waiting" : meter.Visible == null ? "Thinking" : "Streaming", started, DateTimeOffset.UtcNow, meter.First, meter.LiveRate, meter.Output, meter.Estimated));
            }
            meter.Complete(clock.Elapsed.TotalSeconds);
        }
        catch (Exception error) when (error is HttpRequestException or IOException or OperationCanceledException)
        {
            aborted = true;
            if (!context.Response.HasStarted) context.Response.StatusCode = 502;
        }
        finally
        {
            RequestRecord? record = null;
            if (isGeneration && meter.Recognized)
            {
                var total = clock.Elapsed.TotalSeconds;
                var generation = meter.First.HasValue && meter.Last > meter.First ? meter.Last - meter.First : null;
                var rate = generation >= .05 ? meter.RateTokens / generation : total >= .05 ? meter.Output / total : null;
                record = new RequestRecord { Id = id, StartedAt = started, Harness = harness, Route = route, UpstreamHost = destination.Host, Format = meter.Format, Model = meter.Model,
                    Streamed = meter.Streamed, Status = status, Ttft = meter.Streamed ? meter.First : null, FirstVisible = meter.Streamed ? meter.Visible : null, Ttfb = headersAt,
                    Generation = generation, Total = total, InputTokens = meter.Input, CachedInputTokens = meter.Cache, OutputTokens = meter.Output, ReasoningTokens = meter.Reasoning,
                    TokensEstimated = meter.Estimated, Tps = rate, Aborted = aborted || meter.Failed, Source = "proxy", SourceKey = key };
            }
            if (isGeneration)
            {
                try { tracker.FinishProxy(key, record); }
                catch (IOException) { Status = "Proxy measurement is queued; history is temporarily unavailable"; }
            }
        }
    }

    private static string NormalizeHarness(string tag) => tag.ToLowerInvariant() switch { "claude" or "claude-code" => "Claude Code", "codex" => "Codex", "omp" => "OMP", "pi" => "Pi", "dsh" or "deepseek" => "DeepSeek CLI", "opencode" => "opencode", _ => tag };
    private static string GuessHarness(string agent)
    {
        var text = agent.ToLowerInvariant();
        foreach (var name in new[] { "claude-code", "claude-cli", "codex", "opencode", "deepseek", "dsh", "omp", "pi-coding-agent" })
            if (text.Contains(name, StringComparison.Ordinal)) return name switch { "claude-cli" => "Claude Code", "pi-coding-agent" => "Pi", _ => NormalizeHarness(name) };
        return "Unknown";
    }

    public async ValueTask DisposeAsync()
    {
        if (app != null) { await app.StopAsync(); await app.DisposeAsync(); app = null; }
        client.Dispose();
    }
}
