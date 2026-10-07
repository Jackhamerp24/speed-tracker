using System;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.Runtime.InteropServices;
using System.Threading;
using System.Windows;
using System.Windows.Threading;
using SpeedTracker.Core;
using Forms = System.Windows.Forms;

namespace SpeedTracker.Windows;

public sealed class TrayApplication : Application
{
    private readonly TrackerService tracker;
    private readonly HistoryStore history;
    private readonly Action<bool> enhancedChanged;
    private readonly DispatcherTimer clickTimer;
    private Forms.NotifyIcon? tray;
    private Forms.ContextMenuStrip? menu;
    private Icon? idleIcon;
    private Icon? activeIcon;
    private DashboardWindow? dashboard;
    private LiveWindow? flyout;
    private bool started;
    private bool dashboardRequested;
    private bool exiting;
    private bool wasActive;
    private int updatePending;

    public TrayApplication(TrackerService tracker, HistoryStore history, Action<bool>? enhancedChanged = null)
    {
        this.tracker = tracker;
        this.history = history;
        this.enhancedChanged = enhancedChanged ?? tracker.SetEnhancedNetwork;
        ShutdownMode = ShutdownMode.OnExplicitShutdown;
        clickTimer = new DispatcherTimer(DispatcherPriority.Input, Dispatcher)
        {
            Interval = TimeSpan.FromMilliseconds(Forms.SystemInformation.DoubleClickTime)
        };
        clickTimer.Tick += (_, _) => { clickTimer.Stop(); ToggleLive(); };
    }

    protected override void OnStartup(StartupEventArgs args)
    {
        base.OnStartup(args);
        Theme.Initialize(this);
        try
        {
            idleIcon = CreateIcon(false);
            activeIcon = CreateIcon(true);
            menu = new Forms.ContextMenuStrip();
            menu.Items.Add("Open Dashboard", null, (_, _) => OpenDashboard());
            menu.Items.Add("Open Live", null, (_, _) => ShowLive());
            menu.Items.Add("Settings — optional network collector", null, (_, _) => OpenSettings());
            menu.Items.Add(new Forms.ToolStripSeparator());
            menu.Items.Add("Quit Speed Tracker", null, (_, _) => Shutdown());
            tray = new Forms.NotifyIcon
            {
                Icon = idleIcon,
                Text = "Speed Tracker · idle · session-log collection",
                ContextMenuStrip = menu,
                Visible = true
            };
            tray.MouseClick += (_, mouse) =>
            {
                if (mouse.Button != Forms.MouseButtons.Left) return;
                clickTimer.Stop();
                clickTimer.Start();
            };
            tray.MouseDoubleClick += (_, mouse) =>
            {
                if (mouse.Button != Forms.MouseButtons.Left) return;
                clickTimer.Stop();
                OpenDashboard();
            };
            tracker.Changed += TrackerChanged;
            tracker.Start();
            started = true;
            UpdateTray();
            if (dashboardRequested) OpenDashboard();
        }
        catch (Exception error)
        {
            MessageBox.Show("Speed Tracker could not start: " + error.Message, "Speed Tracker", MessageBoxButton.OK, MessageBoxImage.Error);
            Shutdown(1);
        }
    }

    public void OpenDashboard()
    {
        if (!Dispatcher.CheckAccess()) { Dispatcher.BeginInvoke(new Action(OpenDashboard)); return; }
        if (exiting) return;
        if (!started) { dashboardRequested = true; return; }
        clickTimer.Stop();
        flyout?.Hide();
        dashboard ??= new DashboardWindow(tracker, history);
        dashboard.Show();
        if (dashboard.WindowState == WindowState.Minimized) dashboard.WindowState = WindowState.Normal;
        dashboard.Activate();
    }

    public void OpenSettings()
    {
        if (!Dispatcher.CheckAccess()) { Dispatcher.BeginInvoke(new Action(OpenSettings)); return; }
        if (!started || exiting) return;
        clickTimer.Stop();
        GetFlyout().ShowSettings();
        GetFlyout().ShowNearTray();
    }

    private LiveWindow GetFlyout() => flyout ??= new LiveWindow(tracker, OpenDashboard, enhancedChanged);
    private void ToggleLive()
    {
        if (exiting) return;
        if (flyout?.IsVisible == true) flyout.Hide();
        else ShowLive();
    }
    private void ShowLive()
    {
        if (!started || exiting) return;
        clickTimer.Stop();
        GetFlyout().ShowLive();
        GetFlyout().ShowNearTray();
    }

    private void TrackerChanged()
    {
        if (Interlocked.Exchange(ref updatePending, 1) != 0 || Dispatcher.HasShutdownStarted) return;
        Dispatcher.BeginInvoke(DispatcherPriority.Background, new Action(() =>
        {
            Interlocked.Exchange(ref updatePending, 0);
            if (!exiting) UpdateTray();
        }));
    }

    private void UpdateTray()
    {
        if (tray is null) return;
        var active = tracker.Active.Count > 0;
        if (wasActive != active) { tray.Icon = active ? activeIcon : idleIcon; wasActive = active; }
        var text = "Speed Tracker · " + (active ? "active" : "idle") + " · held " + UiFormat.Number(tracker.HeldRate, " tok/s", tracker.HeldRateEstimated);
        var target = tracker.Target;
        if (target is not null) text += " · " + target;
        if (text.Length > 63)
        {
            text = text[..62];
            if (char.IsHighSurrogate(text[^1])) text = text[..^1];
            text += "…";
        }
        if (tray.Text != text) tray.Text = text;
    }

    protected override void OnExit(ExitEventArgs args)
    {
        exiting = true;
        clickTimer.Stop();
        tracker.Changed -= TrackerChanged;
        if (tray is not null) { tray.Visible = false; tray.Dispose(); }
        menu?.Dispose();
        flyout?.DisposeWindow();
        dashboard?.DisposeWindow();
        try { enhancedChanged(false); }
        catch (Exception error) { System.Diagnostics.Trace.WriteLine("Network collector shutdown: " + error.Message); }
        tracker.Dispose();
        idleIcon?.Dispose();
        activeIcon?.Dispose();
        Theme.Dispose();
        base.OnExit(args);
    }

    private static Icon CreateIcon(bool active)
    {
        using var image = new Bitmap(32, 32);
        using (var graphics = Graphics.FromImage(image))
        {
            graphics.SmoothingMode = SmoothingMode.AntiAlias;
            graphics.Clear(Color.Transparent);
            using var background = new SolidBrush(active ? Color.FromArgb(30, 111, 203) : Color.FromArgb(80, 88, 102));
            using var foreground = new SolidBrush(Color.White);
            graphics.FillEllipse(background, 1, 1, 30, 30);
            graphics.FillRectangle(foreground, 7, 18, 4, 7);
            graphics.FillRectangle(foreground, 14, 12, 4, 13);
            graphics.FillRectangle(foreground, 21, 7, 4, 18);
        }
        var handle = image.GetHicon();
        try
        {
            using var borrowed = Icon.FromHandle(handle);
            return (Icon)borrowed.Clone();
        }
        finally { DestroyIcon(handle); }
    }

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool DestroyIcon(IntPtr handle);
}
