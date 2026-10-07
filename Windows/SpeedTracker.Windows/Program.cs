using System;
using System.IO;
using System.Linq;
using System.Security.Principal;
using System.Threading;
using System.Windows;
using SpeedTracker.Core;
using SpeedTracker.Proxy;
using System.Text.Json;

namespace SpeedTracker.Windows;

internal static class Program
{
    [STAThread]
    private static int Main(string[] args)
    {
        var sid = WindowsIdentity.GetCurrent().User?.Value ?? Environment.UserName;
        using var mutex = new Mutex(true, "Local\\SpeedTracker." + sid, out var firstInstance);
        using var reopen = new EventWaitHandle(false, EventResetMode.AutoReset, "Local\\SpeedTracker.Open." + sid);
        if (!firstInstance) { reopen.Set(); return 0; }
        try
        {
            var history = new HistoryStore();
            using var network = new EnhancedNetworkSampler();
            using var tracker = new TrackerService(history, network);
            var proxyOptions = LoadProxyOptions(history.Home);
            var proxy = new ProxyService(tracker, proxyOptions.Port, proxyOptions.Routes);
            try { proxy.StartAsync().GetAwaiter().GetResult(); }
            catch (IOException) { /* A port conflict must not disable passive detection. */ }
            TrayApplication? app = null;
            var changingNetwork = false;
            app = new TrayApplication(tracker, history, async enabled =>
            {
                if (!enabled)
                {
                    tracker.SetEnhancedNetwork(false);
                    network.Dispose();
                    return;
                }
                if (changingNetwork) return;
                changingNetwork = true;
                try
                {
                    await network.EnableAsync();
                    tracker.SetEnhancedNetwork(true);
                }
                catch (Exception error) when (error is System.ComponentModel.Win32Exception or IOException or OperationCanceledException or InvalidOperationException)
                {
                    tracker.SetEnhancedNetwork(false);
                    MessageBox.Show("Enhanced collection was not enabled. Passive log detection continues.\n\n" + error.Message,
                        "Speed Tracker", MessageBoxButton.OK, MessageBoxImage.Information);
                }
                finally { changingNetwork = false; }
            });
            var registration = ThreadPool.RegisterWaitForSingleObject(reopen, (_, _) =>
                app.Dispatcher.BeginInvoke(new Action(app.OpenDashboard)), null, Timeout.Infinite, false);
            try
            {
                if (args.Contains("--dashboard")) app.OpenDashboard();
                return app.Run();
            }
            finally { registration.Unregister(null); proxy.DisposeAsync().AsTask().GetAwaiter().GetResult(); }
        }
        catch (Exception error)
        {
            MessageBox.Show("Speed Tracker could not start.\n\n" + error.Message, "Speed Tracker", MessageBoxButton.OK, MessageBoxImage.Error);
            return 1;
        }
        finally { mutex.ReleaseMutex(); }
    }

    private sealed record ProxyOptions(int Port = 4141, System.Collections.Generic.Dictionary<string, string>? Routes = null);
    private static ProxyOptions LoadProxyOptions(string home)
    {
        var path = Path.Combine(home, "config.json");
        if (!File.Exists(path)) return new();
        try
        {
            if (new FileInfo(path).Length > 1024 * 1024) return new();
            var config = JsonSerializer.Deserialize<ProxyOptions>(File.ReadAllText(path), new JsonSerializerOptions { PropertyNameCaseInsensitive = true });
            return config is { Port: > 0 and <= 65535 } ? config : new();
        }
        catch (Exception error) when (error is IOException or JsonException or UnauthorizedAccessException) { return new(); }
    }
}
