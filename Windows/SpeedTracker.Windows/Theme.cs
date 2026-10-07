using System;
using System.Globalization;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using System.Windows.Controls.Primitives;
using Microsoft.Win32;
using SpeedTracker.Core;

namespace SpeedTracker.Windows;

internal static class Theme
{
    public const string Background = "Tracker.Background";
    public const string Panel = "Tracker.Panel";
    public const string Foreground = "Tracker.Foreground";
    public const string Muted = "Tracker.Muted";
    public const string Border = "Tracker.Border";
    public const string Accent = "Tracker.Accent";
    public const string Secondary = "Tracker.Secondary";
    private static Application? application;

    public static void Initialize(Application app)
    {
        application = app;
        Apply();
        SystemEvents.UserPreferenceChanged += PreferencesChanged;
        SystemParameters.StaticPropertyChanged += ParametersChanged;
        InstallControlStyles(app.Resources);
    }

    public static void Dispose()
    {
        SystemEvents.UserPreferenceChanged -= PreferencesChanged;
        SystemParameters.StaticPropertyChanged -= ParametersChanged;
        application = null;
    }

    private static void PreferencesChanged(object sender, UserPreferenceChangedEventArgs args) => ScheduleApply();
    private static void ParametersChanged(object? sender, System.ComponentModel.PropertyChangedEventArgs args) => ScheduleApply();
    private static void ScheduleApply()
    {
        var app = application;
        if (app is not null && !app.Dispatcher.HasShutdownStarted)
            app.Dispatcher.BeginInvoke(new Action(Apply));
    }

    private static void Apply()
    {
        var app = application;
        if (app is null) return;
        var dark = false;
        try
        {
            using var key = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", false);
            dark = key?.GetValue("AppsUseLightTheme") is int value && value == 0;
        }
        catch (Exception error) when (error is System.Security.SecurityException or UnauthorizedAccessException or System.IO.IOException)
        {
            // System colors remain a usable fallback when personalization is unavailable.
        }
        var highContrast = SystemParameters.HighContrast;
        var resources = app.Resources;
        resources[Background] = highContrast ? SystemColors.WindowBrush : Brush(dark ? "#17191D" : "#F4F5F7");
        resources[Panel] = highContrast ? SystemColors.WindowBrush : Brush(dark ? "#23262C" : "#FFFFFF");
        resources[Foreground] = highContrast ? SystemColors.WindowTextBrush : Brush(dark ? "#F3F4F6" : "#191C22");
        resources[Muted] = highContrast ? SystemColors.WindowTextBrush : Brush(dark ? "#B8BFCC" : "#596171");
        resources[Border] = highContrast ? SystemColors.WindowTextBrush : Brush(dark ? "#444A55" : "#D4D9E2");
        resources[Accent] = highContrast ? SystemColors.HighlightBrush : Brush(dark ? "#76B8FF" : "#146AC8");
        resources[Secondary] = highContrast ? SystemColors.WindowTextBrush : Brush(dark ? "#F6BA73" : "#A75805");
        resources[SystemColors.WindowBrushKey] = resources[Panel];
        resources[SystemColors.WindowTextBrushKey] = resources[Foreground];
        resources[SystemColors.ControlBrushKey] = resources[Panel];
        resources[SystemColors.ControlTextBrushKey] = resources[Foreground];
        resources[SystemColors.ControlDarkBrushKey] = resources[Border];
        resources[SystemColors.GrayTextBrushKey] = resources[Muted];
    }

    private static SolidColorBrush Brush(string color)
    {
        var brush = new SolidColorBrush((Color)ColorConverter.ConvertFromString(color));
        brush.Freeze();
        return brush;
    }

    private static void InstallControlStyles(ResourceDictionary resources)
    {
        foreach (var type in new[] { typeof(Button), typeof(ComboBox), typeof(ComboBoxItem), typeof(TextBox), typeof(CheckBox), typeof(DatePicker), typeof(TabControl), typeof(TabItem), typeof(DataGrid), typeof(DataGridColumnHeader), typeof(DataGridCell), typeof(DataGridRow), typeof(ScrollViewer), typeof(ToolTip) })
        {
            var style = new Style(type);
            style.Setters.Add(new Setter(Control.ForegroundProperty, new DynamicResourceExtension(Foreground)));
            style.Setters.Add(new Setter(Control.FontFamilyProperty, new FontFamily("Segoe UI")));
            style.Setters.Add(new Setter(Control.FontSizeProperty, 13.0));
            if (type != typeof(CheckBox) && type != typeof(ScrollViewer))
            {
                style.Setters.Add(new Setter(Control.BackgroundProperty, new DynamicResourceExtension(Panel)));
                style.Setters.Add(new Setter(Control.BorderBrushProperty, new DynamicResourceExtension(Border)));
            }
            if (type == typeof(Button))
            {
                style.Setters.Add(new Setter(Control.PaddingProperty, new Thickness(10, 6, 10, 6)));
                style.Setters.Add(new Setter(Control.MinHeightProperty, 30.0));
            }
            DependencyProperty? selection = type == typeof(DataGridCell) ? DataGridCell.IsSelectedProperty
                : type == typeof(DataGridRow) ? DataGridRow.IsSelectedProperty
                : type == typeof(ComboBoxItem) ? ComboBoxItem.IsHighlightedProperty : null;
            if (selection is not null)
            {
                var selected = new Trigger { Property = selection, Value = true };
                selected.Setters.Add(new Setter(Control.BackgroundProperty, new DynamicResourceExtension(SystemColors.HighlightBrushKey)));
                selected.Setters.Add(new Setter(Control.ForegroundProperty, new DynamicResourceExtension(SystemColors.HighlightTextBrushKey)));
                style.Triggers.Add(selected);
            }
            var disabled = new Trigger { Property = UIElement.IsEnabledProperty, Value = false };
            disabled.Setters.Add(new Setter(Control.ForegroundProperty, new DynamicResourceExtension(Muted)));
            style.Triggers.Add(disabled);
            resources[type] = style;
        }
    }

    public static Brush Get(string key) => application?.TryFindResource(key) as Brush ?? SystemColors.WindowTextBrush;

    public static TextBlock Text(string text = "", double size = 13, bool muted = false)
    {
        var block = new TextBlock { Text = text, FontSize = size, TextWrapping = TextWrapping.Wrap, Margin = new Thickness(0, 3, 0, 3) };
        block.SetResourceReference(TextBlock.ForegroundProperty, muted ? Muted : Foreground);
        return block;
    }

    public static Border Card(UIElement child, double padding = 14)
    {
        var border = new Border { Child = child, Padding = new Thickness(padding), BorderThickness = new Thickness(1), CornerRadius = new CornerRadius(8), Margin = new Thickness(0, 0, 0, 10) };
        border.SetResourceReference(System.Windows.Controls.Border.BackgroundProperty, Panel);
        border.SetResourceReference(System.Windows.Controls.Border.BorderBrushProperty, Border);
        return border;
    }

    public static void Window(Window window)
    {
        window.FontFamily = new FontFamily("Segoe UI");
        window.FontSize = 13;
        window.SetResourceReference(Control.BackgroundProperty, Background);
        window.SetResourceReference(Control.ForegroundProperty, Foreground);
    }
}

internal static class UiFormat
{
    public static string Number(double? value, string suffix = "", bool estimated = false)
        => value is double number && double.IsFinite(number) && number >= 0
            ? (estimated ? "~" : "") + number.ToString(number < 10 ? "0.00" : "0.0", CultureInfo.CurrentCulture) + suffix
            : "—";
    public static string Count(long value) => value.ToString("N0", CultureInfo.CurrentCulture);
    public static double? Latency(RequestRecord record) => DashboardSummary.ValidLatency(record) ? record.Ttft : null;
    public static double? Speed(RequestRecord record) => DashboardSummary.ValidSpeed(record) ? record.Tps : null;
    public static string Source(RequestRecord record) => record.Source switch { "log" => "Session log", "network" => "Passive network estimate", "proxy" => "Observed proxy", _ => "Unknown / legacy source" };
    public static string Outcome(RequestRecord record) => record.Aborted ? "Interrupted" : record.Status >= 400 ? "Error " + record.Status : record.Status > 0 ? record.Status.ToString(CultureInfo.InvariantCulture) : "Status unavailable";
}
