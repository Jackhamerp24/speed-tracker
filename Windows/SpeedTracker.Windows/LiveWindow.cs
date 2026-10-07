using System;
using System.ComponentModel;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.InteropServices;
using System.Threading;
using System.Windows;
using System.Windows.Automation;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Threading;
using SpeedTracker.Core;

namespace SpeedTracker.Windows;

public sealed class LiveWindow : Window
{
    private readonly TrackerService tracker;
    private readonly Action<bool> enhancedChanged;
    private readonly TabControl tabs = new();
    private readonly ComboBox target = new() { MinWidth = 230, Margin = new Thickness(0, 6, 0, 8) };
    private readonly TextBlock activity = Theme.Text("Idle", 13);
    private readonly TextBlock model = Theme.Text("No call observed yet", 18);
    private readonly TextBlock phase = Theme.Text("No model call in progress", 13, true);
    private readonly TextBlock speed = Theme.Text("—", 32);
    private readonly TextBlock timing = Theme.Text("TTFT unavailable", 13, true);
    private readonly TextBlock held = Theme.Text("No previous speed measurement", 13, true);
    private readonly TextBlock recent = Theme.Text("", 12, true);
    private readonly TextBlock networkStatus = Theme.Text("", 12, true);
    private readonly StackPanel harnessStatuses = new();
    private readonly CheckBox enhanced = new() { Content = "Enable optional elevated network collector", Margin = new Thickness(0, 12, 0, 8) };
    private bool synchronizing;
    private bool allowClose;
    private bool settingDialog;
    private int refreshPending;
    private System.Drawing.Rectangle? workArea;
    private IReadOnlyList<HarnessStatus> displayedStatuses = Array.Empty<HarnessStatus>();
    private string? displayedTarget;

    public LiveWindow(TrackerService tracker, Action openDashboard, Action<bool>? enhancedChanged = null)
    {
        this.tracker = tracker;
        this.enhancedChanged = enhancedChanged ?? tracker.SetEnhancedNetwork;
        Title = "Speed Tracker — Live";
        Width = 390;
        SizeToContent = SizeToContent.Height;
        MaxHeight = 780;
        WindowStyle = WindowStyle.None;
        ResizeMode = ResizeMode.NoResize;
        ShowInTaskbar = false;
        Theme.Window(this);
        var body = new StackPanel();
        var header = new DockPanel { Margin = new Thickness(0, 0, 0, 10) };
        var close = new Button { Content = "×", Width = 30, Padding = new Thickness(3), ToolTip = "Dismiss Live (Esc)" };
        AutomationProperties.SetName(close, "Dismiss Live flyout");
        close.Click += (_, _) => Hide();
        DockPanel.SetDock(close, Dock.Right);
        header.Children.Add(close);
        header.Children.Add(Theme.Text("Speed Tracker", 17));
        body.Children.Add(header);
        tabs.Items.Add(new TabItem { Header = "_Live", Content = BuildLive() });
        tabs.Items.Add(new TabItem { Header = "_Settings", Content = BuildSettings() });
        body.Children.Add(tabs);
        var dashboard = new Button { Content = "Open _Dashboard", Margin = new Thickness(0, 10, 0, 0), HorizontalContentAlignment = HorizontalAlignment.Left };
        dashboard.Click += (_, _) => { Hide(); openDashboard(); };
        body.Children.Add(dashboard);
        var frame = Theme.Card(new ScrollViewer { Content = body, VerticalScrollBarVisibility = ScrollBarVisibility.Auto }, 16);
        frame.Margin = new Thickness(0);
        frame.CornerRadius = new CornerRadius(0);
        Content = frame;
        target.SelectionChanged += (_, _) =>
        {
            if (synchronizing) return;
            try { tracker.Target = (target.SelectedItem as TargetChoice)?.Name; }
            catch (Exception error) { ShowError(error.Message, "Live target could not be saved"); }
            Refresh();
        };
        enhanced.Click += (_, _) => ChangeNetworkSetting();
        tracker.Changed += TrackerChanged;
        IsVisibleChanged += (_, _) => { if (IsVisible) Refresh(); };
        Deactivated += (_, _) => { if (!settingDialog) Hide(); };
        SizeChanged += (_, _) =>
        {
            if (IsVisible && workArea is System.Drawing.Rectangle work)
                AlignToWorkArea(new WindowInteropHelper(this).Handle, work);
        };
        PreviewKeyDown += (_, args) => { if (args.Key == Key.Escape) { Hide(); args.Handled = true; } };
    }

    private UIElement BuildLive()
    {
        var panel = new StackPanel { Margin = new Thickness(0, 10, 0, 0) };
        panel.Children.Add(Theme.Text("Watching · Auto follows observed activity; pin one harness", 12, true));
        AutomationProperties.SetName(target, "Live target, independent of dashboard filters");
        panel.Children.Add(target);
        activity.SetResourceReference(TextBlock.ForegroundProperty, Theme.Accent);
        panel.Children.Add(activity);
        var hero = new StackPanel();
        hero.Children.Add(model);
        hero.Children.Add(phase);
        hero.Children.Add(speed);
        hero.Children.Add(timing);
        panel.Children.Add(Theme.Card(hero));
        panel.Children.Add(held);
        panel.Children.Add(recent);
        panel.Children.Add(Theme.Text("Only call evidence marks activity. An open harness or a tool pause is not model generation. “~” marks an estimate; “—” means unavailable.", 11, true));
        return panel;
    }

    private UIElement BuildSettings()
    {
        var panel = new StackPanel { Margin = new Thickness(0, 10, 0, 0) };
        panel.Children.Add(Theme.Text("Standard-user collection", 15));
        panel.Children.Add(Theme.Text("Readable local session telemetry is detected automatically. No credentials are read, no harness settings are changed, and no proxy is required. A source may expose completed usage but no live speed or TTFT.", 12, true));
        panel.Children.Add(enhanced);
        panel.Children.Add(Theme.Text("An optional elevated network collector requires Windows administrator approval. This dashboard stays unprivileged; no app restart is required. Passive network byte-flow estimates are not exact token counts or verified endpoint attribution.", 12, true));
        panel.Children.Add(networkStatus);
        panel.Children.Add(Theme.Text("Detected harnesses", 15));
        panel.Children.Add(harnessStatuses);
        panel.Children.Add(Theme.Text("Windows controls whether this notification-area icon is shown or placed in the overflow menu. Closing these windows leaves collection running; use Quit in the tray menu to exit.", 11, true));
        return panel;
    }

    private void ChangeNetworkSetting()
    {
        if (synchronizing) return;
        var requested = enhanced.IsChecked == true;
        if (requested && !tracker.EnhancedNetworkEnabled)
        {
            MessageBoxResult answer;
            settingDialog = true;
            try
            {
                answer = MessageBox.Show(this,
                    "Enable the optional elevated network collector? Windows may ask for administrator approval for the separate collector. Speed Tracker itself stays unprivileged, and session logs already work without elevation. Byte-flow speeds and provider attribution remain estimates; no proxy or harness configuration changes will be made.",
                    "Optional enhanced collection", MessageBoxButton.YesNo, MessageBoxImage.Information, MessageBoxResult.No);
            }
            finally { settingDialog = false; }
            if (answer != MessageBoxResult.Yes) { Refresh(); return; }
        }
        try { enhancedChanged(requested); }
        catch (Exception error)
        {
            ShowError(error.Message, "Collection setting could not be changed");
        }
        Refresh();
    }

    private void ShowError(string message, string title)
    {
        settingDialog = true;
        try { MessageBox.Show(this, message, title, MessageBoxButton.OK, MessageBoxImage.Warning); }
        finally { settingDialog = false; }
    }

    public void ShowSettings() { tabs.SelectedIndex = 1; }
    public void ShowLive() { tabs.SelectedIndex = 0; }

    public void ShowNearTray()
    {
        var work = System.Windows.Forms.Screen.FromPoint(System.Windows.Forms.Cursor.Position).WorkingArea;
        workArea = work;
        Show();
        UpdateLayout();
        var handle = new WindowInteropHelper(this).Handle;
        AlignToWorkArea(handle, work);
        MaxHeight = Math.Max(100, work.Height / VisualTreeHelper.GetDpi(this).DpiScaleY - 24);
        UpdateLayout();
        AlignToWorkArea(handle, work);
        Activate();
        if (tabs.SelectedIndex == 0) target.Focus();
        else enhanced.Focus();
    }

    private void TrackerChanged()
    {
        if (Interlocked.Exchange(ref refreshPending, 1) != 0 || Dispatcher.HasShutdownStarted) return;
        Dispatcher.BeginInvoke(DispatcherPriority.Background, new Action(() =>
        {
            Interlocked.Exchange(ref refreshPending, 0);
            if (IsVisible) Refresh();
        }));
    }

    private void Refresh()
    {
        synchronizing = true;
        try
        {
            var selectedTarget = tracker.Target;
            var statuses = tracker.Harnesses;
            var statusesChanged = !displayedStatuses.SequenceEqual(statuses);
            if (target.Items.Count == 0 || statusesChanged || displayedTarget != selectedTarget || (target.SelectedItem as TargetChoice)?.Name != selectedTarget)
            {
                var names = statuses.Select(status => status.Name).Concat(selectedTarget is null ? Array.Empty<string>() : new[] { selectedTarget }).Distinct(StringComparer.OrdinalIgnoreCase).OrderBy(name => name, StringComparer.OrdinalIgnoreCase).ToArray();
                if (target.Items.Count == 0 || !target.Items.Cast<TargetChoice>().Skip(1).Select(choice => choice.Name).SequenceEqual(names))
                {
                    target.Items.Clear();
                    target.Items.Add(new TargetChoice(null));
                    foreach (var name in names) target.Items.Add(new TargetChoice(name));
                }
                target.SelectedItem = target.Items.Cast<TargetChoice>().FirstOrDefault(choice => string.Equals(choice.Name, selectedTarget, StringComparison.OrdinalIgnoreCase));
                displayedTarget = selectedTarget;
            }
            var calls = tracker.Active.Where(call => selectedTarget is null || string.Equals(call.Harness, selectedTarget, StringComparison.OrdinalIgnoreCase)).OrderByDescending(call => call.LastActivity).ToArray();
            var call = calls.FirstOrDefault();
            activity.Text = calls.Length == 0 ? "Idle · no model call in progress" : "Active · " + string.Join(", ", calls.Select(item => item.Harness).Distinct());
            var latest = tracker.HeldRecord;
            model.Text = call is not null ? call.Model : latest is not null ? latest.Model : "No call observed yet";
            phase.Text = call is not null ? call.Harness + " · " + call.Phase : selectedTarget is null ? "No model call in progress" : "No observed model call in " + selectedTarget;
            speed.Text = call is not null && call.Rate is double rate && double.IsFinite(rate) && rate >= 0 ? UiFormat.Number(rate, " tok/s", call.Estimated) : call is not null ? "Live speed unavailable" : "Idle";
            speed.FontSize = call is not null && call.Rate is null ? 20 : 32;
            timing.Text = call is not null ? "TTFT " + UiFormat.Number(call.TTFT, " s", call.Estimated) + " · output " + (call.OutputTokens is int tokens ? (call.Estimated ? "~" : "") + UiFormat.Count(tokens) : "unavailable") : "Completed calls are available in the dashboard";
            held.Text = "Last recorded speed: " + UiFormat.Number(tracker.HeldRate, " tok/s", tracker.HeldRateEstimated) + " · last TTFT " + UiFormat.Number(tracker.HeldTTFT, " s", tracker.HeldTTFTEstimated) + "\nHeld while idle, not live. May be a whole-request fallback; inspect the call's generation timing.";
            recent.Text = latest is null ? "Session logs may provide completed metrics without live activity." : "Latest completed: " + latest.StartedAt.ToLocalTime().ToString("g") + " · " + latest.Harness + "\n" + UiFormat.Source(latest) + " · " + UiFormat.Outcome(latest);
            enhanced.IsChecked = tracker.EnhancedNetworkEnabled;
            networkStatus.Text = tracker.NetworkStatus + (tracker.EnhancedNetworkAvailable ? "" : "\nNo enhanced collector is connected; standard-user session logs remain enabled.");
            if (statusesChanged)
            {
                harnessStatuses.Children.Clear();
                foreach (var status in statuses)
                {
                    var text = status.Name + " · " + (status.HasLogs ? "readable telemetry" : "no supported readable telemetry") + (status.IsRunning ? " · process running (not proof of activity)" : "");
                    if (!string.IsNullOrWhiteSpace(status.Limitation)) text += "\n" + status.Limitation;
                    harnessStatuses.Children.Add(Theme.Text(text, 11, true));
                }
                displayedStatuses = statuses;
            }
        }
        finally { synchronizing = false; }
    }
    protected override void OnClosing(CancelEventArgs args)
    {
        if (!allowClose) { args.Cancel = true; Hide(); }
        base.OnClosing(args);
    }

    public void DisposeWindow()
    {
        tracker.Changed -= TrackerChanged;
        allowClose = true;
        Close();
    }

    private static void AlignToWorkArea(IntPtr handle, System.Drawing.Rectangle work)
    {
        if (!GetWindowRect(handle, out var rectangle)) return;
        var x = Math.Max(work.Left + 12, work.Right - (rectangle.Right - rectangle.Left) - 12);
        var y = Math.Max(work.Top + 12, work.Bottom - (rectangle.Bottom - rectangle.Top) - 12);
        SetWindowPos(handle, IntPtr.Zero, x, y, 0, 0, 0x0001 | 0x0004 | 0x0010);
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct NativeRectangle { public int Left, Top, Right, Bottom; }

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetWindowRect(IntPtr handle, out NativeRectangle rectangle);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetWindowPos(IntPtr handle, IntPtr after, int x, int y, int width, int height, uint flags);

    private sealed record TargetChoice(string? Name)
    {
        public override string ToString() => Name ?? "Auto · all harnesses";
    }
}
