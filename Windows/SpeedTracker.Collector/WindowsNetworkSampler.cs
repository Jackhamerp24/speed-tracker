using System.Diagnostics;
using System.Management;
using System.Net;
using System.Runtime.InteropServices;
using SpeedTracker.Core;

namespace SpeedTracker.Collector;

// Runs only in the explicitly elevated helper. No payload capture or process environment reads.
internal sealed class WindowsNetworkSampler : IDisposable
{
    private readonly Dictionary<int, (string Name, string? Harness, long Started)> processes = new();
    private readonly Dictionary<string, string> providers = new(StringComparer.OrdinalIgnoreCase);
    private readonly HashSet<string> enabled = new();
    private DateTime nextProcesses, nextDns;
    private readonly uint[] families = [2, 23];
    public string Status { get; private set; } = "Passive TCP byte counters";

    public IReadOnlyList<FlowSample> Sample()
    {
        RefreshProcesses();
        RefreshProviders();
        var samples = new List<FlowSample>();
        var present = new HashSet<string>();
        foreach (var family in families)
        {
            uint bytes = 0;
            var result = GetExtendedTcpTable(IntPtr.Zero, ref bytes, false, family, 5, 0);
            if (result != 122 || bytes > 32 * 1024 * 1024) continue;
            var buffer = Marshal.AllocHGlobal((int)bytes);
            try
            {
                result = GetExtendedTcpTable(buffer, ref bytes, false, family, 5, 0);
                if (result != 0) { Status = $"TCP endpoint discovery unavailable ({result})"; continue; }
                var count = Marshal.ReadInt32(buffer);
                var rowSize = family == 2 ? 24 : 56;
                if (count < 0 || 4L + (long)count * rowSize > bytes) continue;
                for (var index = 0; index < count; index++)
                {
                    var row = IntPtr.Add(buffer, 4 + index * rowSize);
                    var state = (uint)Marshal.ReadInt32(row, family == 2 ? 0 : 48);
                    var pid = Marshal.ReadInt32(row, family == 2 ? 20 : 52);
                    if (state != 5 || !processes.TryGetValue(pid, out var process)) continue;
                    var remoteBytes = new byte[family == 2 ? 4 : 16];
                    Marshal.Copy(IntPtr.Add(row, family == 2 ? 12 : 24), remoteBytes, 0, remoteBytes.Length);
                    var remote = new IPAddress(remoteBytes);
                    if (IPAddress.IsLoopback(remote) || remote.IsIPv6LinkLocal) continue;
                    var remotePort = Port(Marshal.ReadInt32(row, family == 2 ? 16 : 44));
                    var localPort = Port(Marshal.ReadInt32(row, family == 2 ? 8 : 20));
                    var address = remote.ToString();
                    var knownProvider = providers.TryGetValue(address, out var provider);
                    if (process.Harness == null && !knownProvider) continue;
                    if (Ignored(process.Name)) continue;
                    var key = $"{pid}:{process.Started}:{family}:{localPort}:{address}:{remotePort}";
                    present.Add(key);
                    var tcpRow = Marshal.AllocHGlobal(family == 2 ? 20 : 52);
                    try
                    {
                        // MIB_TCPROW matches the owner row's prefix; MIB_TCP6ROW puts State first.
                        if (family == 2)
                        {
                            for (var offset = 0; offset < 20; offset += 4) Marshal.WriteInt32(tcpRow, offset, Marshal.ReadInt32(row, offset));
                        }
                        else
                        {
                            Marshal.WriteInt32(tcpRow, 0, (int)state);
                            for (var offset = 0; offset < 48; offset += 4) Marshal.WriteInt32(tcpRow, offset + 4, Marshal.ReadInt32(row, offset));
                        }
                        if (!enabled.Contains(key))
                        {
                            byte enable = 1;
                            var error = family == 2
                                ? SetPerTcpConnectionEStats(tcpRow, 1, ref enable, 0, 1, 0)
                                : SetPerTcp6ConnectionEStats(tcpRow, 1, ref enable, 0, 1, 0);
                            if (error != 0) { Status = $"TCP counters unavailable ({error}); log detection continues"; continue; }
                            enabled.Add(key);
                        }
                        byte active = 0;
                        var counters = new DataCounters();
                        var code = family == 2
                            ? GetPerTcpConnectionEStats(tcpRow, 1, ref active, 0, 1, IntPtr.Zero, 0, 0, ref counters, 0, (uint)Marshal.SizeOf<DataCounters>())
                            : GetPerTcp6ConnectionEStats(tcpRow, 1, ref active, 0, 1, IntPtr.Zero, 0, 0, ref counters, 0, (uint)Marshal.SizeOf<DataCounters>());
                        if (code == 0 && active != 0)
                            samples.Add(new FlowSample(pid, process.Harness ?? process.Name, provider ?? address,
                                counters.DataBytesIn, counters.DataBytesOut, key));
                    }
                    finally { Marshal.FreeHGlobal(tcpRow); }
                }
            }
            finally { Marshal.FreeHGlobal(buffer); }
        }
        enabled.IntersectWith(present);
        return samples;
    }

    private void RefreshProcesses()
    {
        if (DateTime.UtcNow < nextProcesses) return;
        nextProcesses = DateTime.UtcNow.AddSeconds(3);
        var present = new HashSet<int>();
        foreach (var process in Process.GetProcesses())
        using (process)
        {
            try
            {
                var pid = process.Id;
                present.Add(pid);
                var started = process.StartTime.ToUniversalTime().Ticks;
                if (processes.TryGetValue(pid, out var cached) && cached.Started == started) continue;
                var name = process.ProcessName;
                var harness = HarnessNames.Classify(name);
                if (name is "node" or "bun" or "python" or "python3" or "deno")
                {
                    using var search = new ManagementObjectSearcher($"SELECT CommandLine FROM Win32_Process WHERE ProcessId={pid}");
                    using var found = search.Get();
                    foreach (ManagementObject item in found)
                    using (item) { harness = HarnessNames.Classify(name, item["CommandLine"] as string); }
                }
                processes[pid] = (name, harness, started);
            }
            catch (Exception error) when (error is System.ComponentModel.Win32Exception or InvalidOperationException or ManagementException or UnauthorizedAccessException) { }
        }
        foreach (var pid in processes.Keys.Where(id => !present.Contains(id)).ToArray()) processes.Remove(pid);
    }

    private void RefreshProviders()
    {
        if (DateTime.UtcNow < nextDns) return;
        nextDns = DateTime.UtcNow.AddMinutes(5);
        foreach (var host in new[] { "api.anthropic.com", "api.openai.com", "chatgpt.com", "api.deepseek.com", "openrouter.ai", "api.x.ai", "api.groq.com", "api.cerebras.ai", "api.mistral.ai", "api.moonshot.ai", "api.z.ai" })
        {
            try
            {
                // These are public endpoint DNS lookups, not requests to the model APIs.
                foreach (var address in Dns.GetHostAddresses(host)) providers[address.ToString()] = host;
            }
            catch (System.Net.Sockets.SocketException) { }
        }
    }

    private static bool Ignored(string name) => new[] { "speedtracker", "chrome", "msedge", "firefox", "brave", "opera", "claude helper", "chatgpt", "codex desktop", "svchost", "system" }.Any(prefix => name.StartsWith(prefix, StringComparison.OrdinalIgnoreCase));
    private static int Port(int raw) => ((raw & 255) << 8) | ((raw >> 8) & 255);
    public void Dispose() { processes.Clear(); enabled.Clear(); }

    [StructLayout(LayoutKind.Sequential)]
    private struct DataCounters
    {
        public ulong DataBytesOut, DataSegsOut, DataBytesIn, DataSegsIn, SegsOut, SegsIn;
        public uint SoftErrors, SoftErrorReason, SndUna, SndNxt, SndMax;
        public ulong ThruBytesAcked;
        public uint RcvNxt;
        public ulong ThruBytesReceived;
    }

    [DllImport("iphlpapi.dll")] private static extern uint GetExtendedTcpTable(IntPtr table, ref uint size, [MarshalAs(UnmanagedType.Bool)] bool order, uint family, int tableClass, uint reserved);
    [DllImport("iphlpapi.dll")] private static extern uint SetPerTcpConnectionEStats(IntPtr row, int type, ref byte rw, uint version, uint size, uint offset);
    [DllImport("iphlpapi.dll")] private static extern uint SetPerTcp6ConnectionEStats(IntPtr row, int type, ref byte rw, uint version, uint size, uint offset);
    [DllImport("iphlpapi.dll")] private static extern uint GetPerTcpConnectionEStats(IntPtr row, int type, ref byte rw, uint rwVersion, uint rwSize, IntPtr ros, uint rosVersion, uint rosSize, ref DataCounters rod, uint rodVersion, uint rodSize);
    [DllImport("iphlpapi.dll")] private static extern uint GetPerTcp6ConnectionEStats(IntPtr row, int type, ref byte rw, uint rwVersion, uint rwSize, IntPtr ros, uint rosVersion, uint rosSize, ref DataCounters rod, uint rodVersion, uint rodSize);
}
