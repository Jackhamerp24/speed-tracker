using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Windows;
using System.Windows.Automation;
using System.Windows.Automation.Peers;
using System.Windows.Input;
using System.Windows.Media;
using SpeedTracker.Core;

namespace SpeedTracker.Windows;

public sealed class TrendPlot : FrameworkElement
{
    private IReadOnlyList<DashboardBucket> buckets = Array.Empty<DashboardBucket>();
    private double[] values = Array.Empty<double>();
    private int[] counts = Array.Empty<int>();
    private bool histogram;
    private bool speed;
    private double maximum;
    private string unit = "s";
    private int selected = -1;

    public TrendPlot()
    {
        MinHeight = 190;
        Height = 210;
        Focusable = true;
        SnapsToDevicePixels = true;
        SetResourceReference(TextElementForegroundProperty, Theme.Foreground);
        MouseMove += (_, args) => SelectAt(args.GetPosition(this).X);
        MouseLeave += (_, _) => { if (!IsKeyboardFocused) { selected = -1; InvalidateVisual(); } };
        GotKeyboardFocus += (_, _) => InvalidateVisual();
        LostKeyboardFocus += (_, _) => InvalidateVisual();
        KeyDown += (_, args) =>
        {
            var count = histogram ? counts.Length : buckets.Count;
            if (count == 0 || (args.Key != Key.Left && args.Key != Key.Right)) return;
            selected = Math.Clamp(selected + (args.Key == Key.Left ? -1 : 1), 0, count - 1);
            UpdateToolTip();
            InvalidateVisual();
            args.Handled = true;
        };
    }

    // A resource dependency invalidates the drawing when the OS theme changes.
    private static readonly DependencyProperty TextElementForegroundProperty = DependencyProperty.Register(
        "PlotForeground", typeof(Brush), typeof(TrendPlot), new FrameworkPropertyMetadata(null, FrameworkPropertyMetadataOptions.AffectsRender));

    public void SetTrends(IReadOnlyList<DashboardBucket> trends, bool speedMetric)
    {
        buckets = trends;
        histogram = false;
        speed = speedMetric;
        unit = speed ? "tok/s" : "s";
        maximum = trends.SelectMany(bucket => new[] { Median(bucket), P95(bucket) }).Where(value => value.HasValue && double.IsFinite(value.Value)).Select(value => value!.Value).DefaultIfEmpty(0).Max();
        selected = -1;
        AutomationProperties.SetName(this, (speed ? "Generation speed" : "Time to first token") + " trend, p50 and p95, " + trends.Count + " time buckets");
        AutomationProperties.SetHelpText(this, "Use left and right arrow keys to inspect buckets. Missing measurements are not zero. Solid line is p50; dashed line is p95.");
        ToolTip = "Solid: p50 · Dashed: p95. Use arrow keys or hover to inspect.";
        InvalidateVisual();
    }

    public void SetDistribution(IEnumerable<double> measurements, string measurementUnit)
    {
        values = measurements.Where(value => double.IsFinite(value) && value >= 0).ToArray();
        histogram = true;
        unit = measurementUnit;
        maximum = values.DefaultIfEmpty(0).Max();
        counts = new int[values.Length == 0 ? 0 : Math.Min(12, Math.Max(1, (int)Math.Ceiling(Math.Sqrt(values.Length))))];
        foreach (var value in values)
        {
            var index = maximum > 0 ? Math.Min(counts.Length - 1, (int)(value / maximum * counts.Length)) : 0;
            counts[index]++;
        }
        selected = -1;
        AutomationProperties.SetName(this, "Distribution of " + values.Length + " measurements in " + unit);
        AutomationProperties.SetHelpText(this, "Use left and right arrow keys to inspect histogram ranges. Invalid, interrupted and unavailable measurements are excluded.");
        ToolTip = values.Length == 0 ? "No valid measurements" : values.Length + " measured calls; hover or use arrow keys for range counts.";
        InvalidateVisual();
    }

    private double? Median(DashboardBucket bucket) => speed ? bucket.Summary.MedianTPS : bucket.Summary.MedianTTFT;
    private double? P95(DashboardBucket bucket) => speed ? bucket.Summary.P95TPS : bucket.Summary.P95TTFT;
    private int Samples(DashboardBucket bucket) => speed ? bucket.Summary.SpeedCount : bucket.Summary.LatencyCount;
    private Rect PlotArea => new(58, 14, Math.Max(1, ActualWidth - 76), Math.Max(1, ActualHeight - 54));

    protected override void OnRender(DrawingContext drawing)
    {
        base.OnRender(drawing);
        var area = PlotArea;
        drawing.DrawRectangle(Theme.Get(Theme.Panel), null, new Rect(RenderSize));
        var grid = new Pen(Theme.Get(Theme.Border), 1);
        var yMaximum = histogram ? counts.DefaultIfEmpty(0).Max() : maximum;
        var scale = yMaximum > 0 ? yMaximum : 1;
        for (var tick = 0; tick <= 3; tick++)
        {
            var y = area.Bottom - area.Height * tick / 3;
            drawing.DrawLine(grid, new Point(area.Left, y), new Point(area.Right, y));
            Label(drawing, (scale * tick / 3).ToString(histogram ? "0" : "0.##", CultureInfo.CurrentCulture), 2, y - 8);
        }
        if ((histogram && values.Length == 0) || (!histogram && !buckets.Any(bucket => Median(bucket).HasValue)))
        {
            Label(drawing, "No valid measurements in this selection", area.Left + 12, area.Top + area.Height / 2 - 8);
            return;
        }
        if (histogram)
        {
            var width = area.Width / counts.Length;
            for (var index = 0; index < counts.Length; index++)
            {
                var height = counts[index] / scale * area.Height;
                var rectangle = new Rect(area.Left + index * width + 1, area.Bottom - height, Math.Max(1, width - 2), height);
                drawing.DrawRectangle(Theme.Get(index == selected ? Theme.Secondary : Theme.Accent), null, rectangle);
            }
            Label(drawing, "0 " + unit, area.Left, area.Bottom + 8);
            LabelRight(drawing, UiFormat.Number(maximum, " " + unit), area.Right, area.Bottom + 8);
        }
        else
        {
            DrawSeries(drawing, area, scale, Median, new Pen(Theme.Get(Theme.Accent), 2));
            DrawSeries(drawing, area, scale, P95, new Pen(Theme.Get(Theme.Secondary), 2) { DashStyle = DashStyles.Dash });
            Label(drawing, buckets[0].Date.ToLocalTime().ToString("MMM d HH:mm", CultureInfo.CurrentCulture), area.Left, area.Bottom + 8);
            if (buckets.Count > 1) LabelRight(drawing, buckets[^1].Date.ToLocalTime().ToString("MMM d HH:mm", CultureInfo.CurrentCulture), area.Right, area.Bottom + 8);
            if (selected >= 0 && selected < buckets.Count)
            {
                var x = X(selected, area);
                drawing.DrawLine(new Pen(Theme.Get(Theme.Muted), 1) { DashStyle = DashStyles.Dot }, new Point(x, area.Top), new Point(x, area.Bottom));
            }
        }
        if (IsKeyboardFocused)
            drawing.DrawRectangle(null, new Pen(Theme.Get(Theme.Accent), 1) { DashStyle = DashStyles.Dot }, new Rect(1, 1, Math.Max(0, ActualWidth - 2), Math.Max(0, ActualHeight - 2)));
    }

    private void DrawSeries(DrawingContext drawing, Rect area, double scale, Func<DashboardBucket, double?> metric, Pen pen)
    {
        Point? previous = null;
        for (var index = 0; index < buckets.Count; index++)
        {
            var value = metric(buckets[index]);
            if (value is not double number || !double.IsFinite(number) || number < 0) { previous = null; continue; }
            var point = new Point(X(index, area), area.Bottom - number / scale * area.Height);
            if (previous is Point start) drawing.DrawLine(pen, start, point);
            drawing.DrawEllipse(pen.Brush, null, point, 2.5, 2.5);
            previous = point;
        }
    }

    private double X(int index, Rect area)
    {
        if (buckets.Count <= 1) return area.Left + area.Width / 2;
        var duration = (buckets[^1].Date - buckets[0].Date).TotalSeconds;
        return duration <= 0 ? area.Left + area.Width / 2 : area.Left + (buckets[index].Date - buckets[0].Date).TotalSeconds / duration * area.Width;
    }
    private void Label(DrawingContext drawing, string text, double x, double y) => drawing.DrawText(Formatted(text), new Point(x, y));
    private void LabelRight(DrawingContext drawing, string text, double right, double y)
    {
        var label = Formatted(text);
        drawing.DrawText(label, new Point(Math.Max(0, right - label.Width), y));
    }
    private FormattedText Formatted(string text) => new(text, CultureInfo.CurrentCulture, FlowDirection.LeftToRight, new Typeface("Segoe UI"), 11, Theme.Get(Theme.Muted), VisualTreeHelper.GetDpi(this).PixelsPerDip);

    private void SelectAt(double x)
    {
        var count = histogram ? counts.Length : buckets.Count;
        if (count == 0) return;
        var ratio = Math.Clamp((x - PlotArea.Left) / PlotArea.Width, 0, 1);
        var index = histogram ? Math.Min(count - 1, (int)(ratio * count)) : NearestBucket(ratio);
        if (index == selected) return;
        selected = index;
        UpdateToolTip();
        InvalidateVisual();
    }

    private int NearestBucket(double ratio)
    {
        if (buckets.Count <= 1) return 0;
        var seconds = (buckets[^1].Date - buckets[0].Date).TotalSeconds * ratio;
        var best = 0;
        var distance = double.MaxValue;
        for (var index = 0; index < buckets.Count; index++)
        {
            var candidate = Math.Abs((buckets[index].Date - buckets[0].Date).TotalSeconds - seconds);
            if (candidate < distance) { best = index; distance = candidate; }
        }
        return best;
    }

    private void UpdateToolTip()
    {
        string text;
        if (histogram)
        {
            var lower = maximum * selected / counts.Length;
            var upper = maximum * (selected + 1) / counts.Length;
            text = UiFormat.Number(lower) + "–" + UiFormat.Number(upper) + " " + unit + ": " + UiFormat.Count(counts[selected]) + " calls";
        }
        else
        {
            var bucket = buckets[selected];
            text = bucket.Date.ToLocalTime().ToString("g", CultureInfo.CurrentCulture) + " · p50 " + UiFormat.Number(Median(bucket), " " + unit) + " · p95 " + UiFormat.Number(P95(bucket), " " + unit) + " · " + Samples(bucket) + " measured calls";
        }
        ToolTip = text;
        AutomationProperties.SetHelpText(this, text);
    }

    protected override AutomationPeer OnCreateAutomationPeer() => new FrameworkElementAutomationPeer(this);
}
