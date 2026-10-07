using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Text.Json;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Http;
using Microsoft.Extensions.Logging;
using SpeedTracker.Core;
using SpeedTracker.Proxy;

internal static class ProxyRegression
{
    public static async Task Run(Action<bool, string, string?> check)
    {
        Console.WriteLine("scenario: optional loopback proxy forwards streams and rejects browser requests");
        var root = Path.Combine(Path.GetTempPath(), "speedtracker-proxy-smoke-" + Guid.NewGuid().ToString("N"));
        var upstreamPort = FreePort(); var proxyPort = FreePort();
        while (proxyPort == upstreamPort) proxyPort = FreePort();
        var builder = WebApplication.CreateSlimBuilder(); builder.Logging.ClearProviders();
        builder.WebHost.ConfigureKestrel(server => server.Listen(IPAddress.Loopback, upstreamPort));
        var upstream = builder.Build();
        var forwarded = false;
        upstream.Run(async context =>
        {
            if (context.Request.Path != "/v1/chat/completions") { context.Response.StatusCode = 404; return; }
            using var body = await JsonDocument.ParseAsync(context.Request.Body);
            forwarded = body.RootElement.GetProperty("model").GetString() == "smoke-model" && context.Request.Headers.Authorization == "Bearer smoke-only";
            context.Response.ContentType = "text/event-stream";
            await Task.Delay(150);
            for (var index = 0; index < 4; index++)
            {
                var row = JsonSerializer.Serialize(new { model = "smoke-model", choices = new[] { new { delta = new { content = "token " } } } });
                await context.Response.WriteAsync("data: " + row + "\n\n");
                await context.Response.Body.FlushAsync(); await Task.Delay(80);
            }
            await context.Response.WriteAsync("data: {\"model\":\"smoke-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":40}}\n\ndata: [DONE]\n\n");
        });
        var history = new HistoryStore(root);
        using var tracker = new TrackerService(history, sources: Array.Empty<LogSource>());
        await using var proxy = new ProxyService(tracker, proxyPort, new Dictionary<string, string> { ["smoke"] = $"http://127.0.0.1:{upstreamPort}" });
        try
        {
            await upstream.StartAsync(); await proxy.StartAsync();
            using var client = new HttpClient();
            using var browser = new HttpRequestMessage(HttpMethod.Post, $"http://127.0.0.1:{proxyPort}/smoke@opencode/v1/chat/completions") { Content = new StringContent("{}") };
            browser.Headers.Add("Origin", "https://untrusted.example");
            using var denied = await client.SendAsync(browser);
            check(denied.StatusCode == HttpStatusCode.Forbidden, "proxy rejects browser Origin", null);
            using var request = new HttpRequestMessage(HttpMethod.Post, $"http://127.0.0.1:{proxyPort}/smoke@opencode/v1/chat/completions") { Content = new StringContent("{\"model\":\"smoke-model\",\"stream\":true}", Encoding.UTF8, "application/json") };
            request.Headers.Authorization = new("Bearer", "smoke-only");
            using var response = await client.SendAsync(request);
            var reply = await response.Content.ReadAsStringAsync();
            for (var attempt = 0; attempt < 20 && history.Read().Count == 0; attempt++) await Task.Delay(25);
            var record = history.Read().SingleOrDefault();
            check(forwarded && response.IsSuccessStatusCode && reply.Contains("[DONE]"), "proxy forwards request body/auth and unmodified SSE response", null);
            check(record is { Source: "proxy", Harness: "opencode", Model: "smoke-model", OutputTokens: 40, InputTokens: 10, TokensEstimated: false, Aborted: false }, "proxy persists measured usage without credentials or text", record?.ToString());
            check(record?.Ttft >= .1 && record?.Ttft < 2 && record.Generation >= .15 && record.Tps > 0, "proxy measures first token and streaming generation", $"TTFT={record?.Ttft}, generation={record?.Generation}, TPS={record?.Tps}");
            check(tracker.Active.Count == 0 && tracker.HeldRate > 0, "proxy completion leaves held speed and no false live call", null);
        }
        finally { await upstream.StopAsync(); await upstream.DisposeAsync(); if (Directory.Exists(root)) Directory.Delete(root, true); }
    }
    private static int FreePort()
    {
        var listener = new TcpListener(IPAddress.Loopback, 0); listener.Start();
        var port = ((IPEndPoint)listener.LocalEndpoint).Port; listener.Stop(); return port;
    }
}
