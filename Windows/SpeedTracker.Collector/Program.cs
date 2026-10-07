using System.Diagnostics;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Text.Json;

namespace SpeedTracker.Collector;

internal static class Program
{
    private static async Task<int> Main(string[] args)
    {
        if (!OperatingSystem.IsWindows() || args.Length != 2 || !args[0].StartsWith("SpeedTracker.Collector.", StringComparison.Ordinal)
            || !Guid.TryParseExact(args[0]["SpeedTracker.Collector.".Length..], "N", out _)
            || !int.TryParse(args[1], out var parentPid)) return 2;
        using var identity = WindowsIdentity.GetCurrent();
        if (!new WindowsPrincipal(identity).IsInRole(WindowsBuiltInRole.Administrator)) return 3;
        try
        {
            using var parent = Process.GetProcessById(parentPid);
            using var pipe = new NamedPipeClientStream(".", args[0], PipeDirection.Out, PipeOptions.Asynchronous);
            await pipe.ConnectAsync(15000);
            if (!GetNamedPipeServerProcessId(pipe.SafePipeHandle.DangerousGetHandle(), out var serverPid) || serverPid != parentPid) return 4;
            using var writer = new StreamWriter(pipe) { AutoFlush = true };
            using var sampler = new WindowsNetworkSampler();
            while (!parent.HasExited && pipe.IsConnected)
            {
                var samples = sampler.Sample();
                await writer.WriteLineAsync(JsonSerializer.Serialize(new { Status = sampler.Status, Samples = samples }));
                await Task.Delay(250);
            }
            return 0;
        }
        catch (Exception error) when (error is IOException or TimeoutException or ArgumentException or InvalidOperationException or UnauthorizedAccessException)
        {
            return 1;
        }
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetNamedPipeServerProcessId(IntPtr pipe, out uint serverProcessId);
}
