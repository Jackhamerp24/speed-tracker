using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Security.AccessControl;
using System.Security.Principal;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using SpeedTracker.Core;

namespace SpeedTracker.Windows;

// The UI never elevates. A one-way user/admin pipe authenticates both endpoints by process ID.
internal sealed class EnhancedNetworkSampler : IFlowSampler
{
    private readonly object gate = new();
    private CancellationTokenSource? cancellation;
    private NamedPipeServerStream? pipe;
    private Process? helper;
    private IReadOnlyList<FlowSample> samples = Array.Empty<FlowSample>();
    private DateTime received;
    private bool available;
    private string status = "Standard-user log detection. Enhanced network collection is off.";

    public bool Available { get { lock (gate) return available; } }
    public string Status { get { lock (gate) return status; } }
    public IReadOnlyList<FlowSample> Sample()
    {
        lock (gate) return DateTime.UtcNow - received < TimeSpan.FromSeconds(3) ? samples : Array.Empty<FlowSample>();
    }

    public async Task EnableAsync()
    {
        Dispose();
        var executable = Path.Combine(AppContext.BaseDirectory, "collector", "SpeedTracker.Collector.exe");
        if (!File.Exists(executable)) throw new FileNotFoundException("The packaged network collector is missing. Re-extract the complete Speed Tracker package.", executable);
        var name = "SpeedTracker.Collector." + Guid.NewGuid().ToString("N");
        var tokenSource = new CancellationTokenSource();
        var security = new PipeSecurity();
        security.SetAccessRuleProtection(true, false);
        var user = WindowsIdentity.GetCurrent().User ?? throw new InvalidOperationException("Windows user identity is unavailable.");
        security.AddAccessRule(new PipeAccessRule(user, PipeAccessRights.FullControl, AccessControlType.Allow));
        // Credential-based UAC can run the helper as a different administrator account.
        security.AddAccessRule(new PipeAccessRule(new SecurityIdentifier(WellKnownSidType.BuiltinAdministratorsSid, null), PipeAccessRights.ReadWrite, AccessControlType.Allow));
        var server = NamedPipeServerStreamAcl.Create(name, PipeDirection.In, 1, PipeTransmissionMode.Byte,
            PipeOptions.Asynchronous, 64 * 1024, 0, security);
        cancellation = tokenSource;
        pipe = server;
        lock (gate) status = "Waiting for permission to enable passive network counters…";
        try
        {
            var start = new ProcessStartInfo(executable) { UseShellExecute = true, Verb = "runas", WorkingDirectory = AppContext.BaseDirectory };
            start.ArgumentList.Add(name);
            start.ArgumentList.Add(Environment.ProcessId.ToString());
            helper = Process.Start(start) ?? throw new InvalidOperationException("Network collector did not start.");
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(tokenSource.Token);
            timeout.CancelAfter(TimeSpan.FromSeconds(30));
            await server.WaitForConnectionAsync(timeout.Token);
            if (!GetNamedPipeClientProcessId(server.SafePipeHandle.DangerousGetHandle(), out var clientPid) || clientPid != helper.Id)
                throw new IOException("Network collector identity did not match the process that was launched.");
            lock (gate) { available = true; status = "Enhanced passive TCP collection enabled; traffic metrics are estimates."; }
            _ = ReadAsync(server, tokenSource.Token);
        }
        catch
        {
            Dispose();
            throw;
        }
    }

    private async Task ReadAsync(NamedPipeServerStream server, CancellationToken token)
    {
        try
        {
            using var reader = new StreamReader(server);
            while (!token.IsCancellationRequested)
            {
                var line = await reader.ReadLineAsync(token);
                if (line == null) break;
                var message = JsonSerializer.Deserialize<Batch>(line);
                if (message == null) continue;
                lock (gate)
                {
                    samples = message.Samples ?? Array.Empty<FlowSample>();
                    received = DateTime.UtcNow;
                    status = message.Status ?? "Enhanced passive TCP collection";
                }
            }
        }
        catch (Exception error) when (error is IOException or OperationCanceledException or ObjectDisposedException or JsonException) { }
        finally
        {
            lock (gate)
            {
                if (ReferenceEquals(pipe, server))
                {
                    available = false;
                    samples = Array.Empty<FlowSample>();
                    status = "Enhanced collector disconnected; log detection continues.";
                }
            }
        }
    }

    public void Dispose()
    {
        cancellation?.Cancel();
        cancellation?.Dispose();
        cancellation = null;
        pipe?.Dispose();
        pipe = null;
        helper?.Dispose();
        helper = null;
        lock (gate)
        {
            samples = Array.Empty<FlowSample>();
            available = false;
            status = "Standard-user log detection. Enhanced network collection is off.";
        }
    }

    private sealed record Batch(string? Status, FlowSample[]? Samples);
    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetNamedPipeClientProcessId(IntPtr pipe, out uint clientProcessId);
}
