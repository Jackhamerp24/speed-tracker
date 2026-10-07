using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.ComponentModel;
using System.Globalization;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Automation;
using System.Windows.Controls;
using System.Windows.Controls.Primitives;
using System.Windows.Data;
using System.Windows.Input;
using System.Windows.Threading;
using SpeedTracker.Core;

namespace SpeedTracker.Windows;

public sealed class DashboardWindow : Window
{
    private const int PageSize = 75;
    private readonly TrackerService tracker;
    private readonly HistoryStore history;
    private readonly List<RequestRecord> records = new();
    private readonly HashSet<string> recordKeys = new(StringComparer.Ordinal);
    private readonly ConcurrentQueue<RequestRecord> incoming = new();
    private readonly TextBlock live = Theme.Text("", 12);
    private readonly TextBlock status = Theme.Text("Loading completed history…", 12, true);
    private readonly ComboBox range = new() { MinWidth = 125 };
    private readonly DatePicker from = new() { Width = 130 };
    private readonly DatePicker through = new() { Width = 130 };
    private readonly ComboBox harness = new() { MinWidth = 155, MaxWidth = 220 };
    private readonly ComboBox provider = new() { MinWidth = 155, MaxWidth = 240 };
    private readonly ComboBox model = new() { MinWidth = 180, MaxWidth = 300 };
    private readonly TabControl tabs = new();
    private readonly StackPanel overview = new();
    private readonly TextBlock metricExplanation = Theme.Text("", 12, true);
    private readonly TrendPlot latencyTrend = new();
    private readonly TrendPlot speedTrend = new();
    private readonly TrendPlot latencyDistribution = new();
    private readonly TrendPlot speedDistribution = new();
    private readonly DataGrid calls = new();
    private readonly TextBlock pageLabel = Theme.Text("", 12, true);
    private readonly Button previous = new() { Content = "_Previous" };
    private readonly Button next = new() { Content = "_Next" };
    private readonly StackPanel inspector = new();
    private readonly Button reload = new() { Content = "_Refresh history" };
    private IReadOnlyList<CallRow> sorted = Array.Empty<CallRow>();
    private DashboardReport? report;
    private bool synchronizing;
    private bool allowClose;
    private bool loading;
    private bool loaded;
    private bool disposed;
    private int page;
    private int changePending;
    private int livePending;
    private string sortColumn = "StartedAt";
    private ListSortDirection sortDirection = ListSortDirection.Descending;

    public DashboardWindow(TrackerService tracker, HistoryStore history)
    {
        this.tracker = tracker;
        this.history = history;
        Title = "Speed Tracker — Dashboard";
        Width = 1250;
        Height = 850;
        MinWidth = 1000;
        MinHeight = 680;
        WindowStartupLocation = WindowStartupLocation.CenterScreen;
        Theme.Window(this);
        var root = new DockPanel { Margin = new Thickness(20) };
        var header = new DockPanel();
        reload.Click += async (_, _) => await ReloadHistory();
        DockPanel.SetDock(reload, Dock.Right);
        header.Children.Add(reload);
        var heading = new StackPanel();
        heading.Children.Add(Theme.Text("Completed-call analytics", 24));
        heading.Children.Add(Theme.Text("Provider means endpoint or reported label, not model vendor. Dashboard filters never change the Live target.", 12, true));
        header.Children.Add(heading);
        DockPanel.SetDock(header, Dock.Top);
        root.Children.Add(header);
        var liveCard = Theme.Card(live, 9);
        liveCard.Margin = new Thickness(0, 12, 0, 12);
        DockPanel.SetDock(liveCard, Dock.Top);
        root.Children.Add(liveCard);
        var filters = BuildFilters();
        DockPanel.SetDock(filters, Dock.Top);
        root.Children.Add(filters);
        DockPanel.SetDock(status, Dock.Bottom);
        root.Children.Add(status);
        tabs.Items.Add(new TabItem { Header = "_Overview", Content = new ScrollViewer { Content = overview, VerticalScrollBarVisibility = ScrollBarVisibility.Auto, HorizontalScrollBarVisibility = ScrollBarVisibility.Auto } });
        tabs.Items.Add(new TabItem { Header = "_Trends & distributions", Content = BuildTrends() });
        tabs.Items.Add(new TabItem { Header = "_Calls & inspector", Content = BuildCalls() });
        root.Children.Add(tabs);
        Content = root;
        tracker.Changed += TrackerChanged;
        history.Recorded += HistoryRecorded;
        IsVisibleChanged += async (_, _) =>
        {
            if (!IsVisible) return;
            UpdateLive();
            if (!loaded) await ReloadHistory();
            else RebuildReport();
        };
        PreviewKeyDown += (_, args) =>
        {
            if (args.Key == Key.Escape) { Hide(); args.Handled = true; }
        };
    }

    private UIElement BuildFilters()
    {
        var panel = new WrapPanel { Margin = new Thickness(0, 0, 0, 10) };
        foreach (var choice in new[] { "Last 24 hours", "Last 7 days", "Last 30 days", "All history", "Custom dates" }) range.Items.Add(choice);
        range.SelectedIndex = 1;
        from.SelectedDate = DateTime.Today.AddDays(-7);
        through.SelectedDate = DateTime.Today;
        AddFilter(panel, "_Period", range, "Date range for completed calls");
        AddFilter(panel, "_From", from, "Inclusive local start date");
        AddFilter(panel, "_Through", through, "Inclusive local end date");
        AddFilter(panel, "_Harness", harness, "Dashboard harness filter, independent of Live target");
        AddFilter(panel, "_Provider", provider, "Endpoint or reported provider filter");
        AddFilter(panel, "_Model", model, "Model filter");
        var reset = new Button { Content = "_Reset filters", Margin = new Thickness(0, 21, 0, 0) };
        reset.Click += (_, _) =>
        {
            synchronizing = true;
            range.SelectedIndex = 1;
            harness.SelectedIndex = provider.SelectedIndex = model.SelectedIndex = 0;
            synchronizing = false;
            FiltersChanged();
        };
        panel.Children.Add(reset);
        foreach (var combo in new[] { range, harness, provider, model }) combo.SelectionChanged += (_, _) => FiltersChanged();
        from.SelectedDateChanged += (_, _) => FiltersChanged();
        through.SelectedDateChanged += (_, _) => FiltersChanged();
        from.IsEnabled = through.IsEnabled = false;
        return panel;
    }

    private static void AddFilter(Panel panel, string label, Control control, string accessibleName)
    {
        var column = new StackPanel { Margin = new Thickness(0, 0, 12, 8) };
        column.Children.Add(new Label { Content = label, Target = control, Padding = new Thickness(0, 0, 0, 4) });
        AutomationProperties.SetName(control, accessibleName);
        column.Children.Add(control);
        panel.Children.Add(column);
    }

    private UIElement BuildTrends()
    {
        var content = new StackPanel { Margin = new Thickness(0, 12, 0, 0) };
        content.Children.Add(metricExplanation);
        content.Children.Add(ChartRow("TTFT · p50 / p95", latencyTrend, "TTFT distribution · measured calls", latencyDistribution));
        content.Children.Add(ChartRow("Generation speed · p50 / p95", speedTrend, "Speed distribution · measured calls", speedDistribution));
        content.Children.Add(Theme.Text("Solid: p50 · dashed: p95. R7 interpolation. Hover or focus a chart and use ← / → to inspect. Missing metrics are gaps, not zeros. Dates are shown in local time.", 12, true));
        return new ScrollViewer { Content = content, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
    }

    private static UIElement ChartRow(string leftTitle, TrendPlot left, string rightTitle, TrendPlot right)
    {
        var row = new Grid();
        row.ColumnDefinitions.Add(new ColumnDefinition());
        row.ColumnDefinitions.Add(new ColumnDefinition());
        var first = new StackPanel();
        first.Children.Add(Theme.Text(leftTitle, 15));
        first.Children.Add(left);
        var second = new StackPanel();
        second.Children.Add(Theme.Text(rightTitle, 15));
        second.Children.Add(right);
        var leftCard = Theme.Card(first);
        leftCard.Margin = new Thickness(0, 0, 8, 12);
        var rightCard = Theme.Card(second);
        rightCard.Margin = new Thickness(8, 0, 0, 12);
        Grid.SetColumn(rightCard, 1);
        row.Children.Add(leftCard);
        row.Children.Add(rightCard);
        return row;
    }

    private UIElement BuildCalls()
    {
        var root = new Grid { Margin = new Thickness(0, 12, 0, 0) };
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(6) });
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(315) });
        var table = new DockPanel();
        var pagination = new StackPanel { Orientation = Orientation.Horizontal, Margin = new Thickness(0, 8, 0, 0) };
        previous.Click += (_, _) => { if (page > 0) { page--; ShowPage(); } };
        next.Click += (_, _) => { if ((page + 1) * PageSize < sorted.Count) { page++; ShowPage(); } };
        pageLabel.Margin = new Thickness(12, 6, 12, 0);
        pagination.Children.Add(previous);
        pagination.Children.Add(pageLabel);
        pagination.Children.Add(next);
        DockPanel.SetDock(pagination, Dock.Bottom);
        table.Children.Add(pagination);
        calls.AutoGenerateColumns = false;
        calls.IsReadOnly = true;
        calls.CanUserAddRows = false;
        calls.CanUserDeleteRows = false;
        calls.SelectionMode = DataGridSelectionMode.Single;
        calls.SelectionUnit = DataGridSelectionUnit.FullRow;
        calls.EnableRowVirtualization = true;
        calls.EnableColumnVirtualization = true;
        calls.HeadersVisibility = DataGridHeadersVisibility.Column;
        calls.GridLinesVisibility = DataGridGridLinesVisibility.Horizontal;
        calls.RowHeight = 32;
        calls.FrozenColumnCount = 1;
        AutomationProperties.SetName(calls, "Completed calls, globally sortable and paginated");
        AddColumn("Started (local)", "StartedDisplay", "StartedAt", 140);
        AddColumn("Harness", "Harness", "Harness", 105);
        AddColumn("Provider", "ProviderDisplay", "Provider", 120);
        AddColumn("Model", "Model", "Model", 155);
        AddColumn("TTFT (s)", "LatencyDisplay", "Latency", 80);
        AddColumn("tok/s", "SpeedDisplay", "Speed", 75);
        AddColumn("Output", "OutputDisplay", "Output", 75);
        AddColumn("Outcome", "Outcome", "Outcome", 120);
        AddColumn("Source", "Source", "Source", 150);
        calls.Sorting += SortCalls;
        calls.SelectionChanged += (_, _) => Inspect((calls.SelectedItem as CallRow)?.Record);
        table.Children.Add(calls);
        root.Children.Add(table);
        var splitter = new GridSplitter { Width = 6, HorizontalAlignment = HorizontalAlignment.Stretch, VerticalAlignment = VerticalAlignment.Stretch, ResizeBehavior = GridResizeBehavior.PreviousAndNext, ResizeDirection = GridResizeDirection.Columns };
        Grid.SetColumn(splitter, 1);
        root.Children.Add(splitter);
        var detail = Theme.Card(new ScrollViewer { Content = inspector, VerticalScrollBarVisibility = ScrollBarVisibility.Auto });
        Grid.SetColumn(detail, 2);
        root.Children.Add(detail);
        Inspect(null);
        return root;
    }

    private void AddColumn(string title, string binding, string sort, double width)
    {
        calls.Columns.Add(new DataGridTextColumn { Header = title, Binding = new Binding(binding), SortMemberPath = sort, Width = width, SortDirection = sort == sortColumn ? sortDirection : null });
    }

    private void FiltersChanged()
    {
        if (synchronizing) return;
        from.IsEnabled = through.IsEnabled = range.SelectedIndex == 4;
        page = 0;
        if (loaded) RebuildReport();
    }

    private DashboardFilter? ReadFilter()
    {
        DateTimeOffset? start = null;
        DateTimeOffset? end = null;
        var now = DateTimeOffset.Now;
        switch (range.SelectedIndex)
        {
            case 0: start = now.AddHours(-24); end = now; break;
            case 1: start = now.AddDays(-7); end = now; break;
            case 2: start = now.AddDays(-30); end = now; break;
            case 4:
                if (from.SelectedDate is not DateTime first || through.SelectedDate is not DateTime last || first.Date > last.Date || last.Date == DateTime.MaxValue.Date)
                {
                    status.Text = "Choose a valid start and end date; the end date must not precede the start.";
                    return null;
                }
                start = new DateTimeOffset(DateTime.SpecifyKind(first.Date, DateTimeKind.Local));
                end = new DateTimeOffset(DateTime.SpecifyKind(last.Date.AddDays(1), DateTimeKind.Local));
                break;
        }
        return new DashboardFilter { From = start, Through = end, Harness = (harness.SelectedItem as FilterChoice)?.Key, Provider = (provider.SelectedItem as FilterChoice)?.Key, Model = (model.SelectedItem as FilterChoice)?.Key };
    }

    private async Task ReloadHistory()
    {
        if (loading || disposed) return;
        loading = true;
        reload.IsEnabled = false;
        status.Text = "Reading completed-call history…";
        try
        {
            var snapshot = await Task.Run(() => history.Read());
            if (disposed) return;
            records.Clear();
            recordKeys.Clear();
            foreach (var record in snapshot) AddRecord(record);
            while (incoming.TryDequeue(out var record)) AddRecord(record);
            loaded = true;
            RebuildReport();
        }
        catch (Exception error)
        {
            status.Text = "History could not be read: " + error.Message + ". Use Refresh history to retry.";
        }
        finally
        {
            loading = false;
            if (!disposed) reload.IsEnabled = true;
        }
    }

    private void AddRecord(RequestRecord record)
    {
        var key = string.IsNullOrWhiteSpace(record.SourceKey) ? "id:" + record.Id : "source:" + record.SourceKey;
        if (recordKeys.Add(key)) records.Add(record);
    }

    private void HistoryRecorded(RequestRecord record)
    {
        incoming.Enqueue(record);
        if (Interlocked.Exchange(ref changePending, 1) != 0 || Dispatcher.HasShutdownStarted) return;
        Dispatcher.BeginInvoke(DispatcherPriority.Background, new Action(() =>
        {
            Interlocked.Exchange(ref changePending, 0);
            if (disposed || loading) return;
            while (incoming.TryDequeue(out var value)) AddRecord(value);
            if (loaded && IsVisible) RebuildReport();
        }));
    }

    private void TrackerChanged()
    {
        if (Interlocked.Exchange(ref livePending, 1) != 0 || Dispatcher.HasShutdownStarted) return;
        Dispatcher.BeginInvoke(DispatcherPriority.Background, new Action(() =>
        {
            Interlocked.Exchange(ref livePending, 0);
            if (IsVisible && !disposed) UpdateLive();
        }));
    }

    private void UpdateLive()
    {
        var active = tracker.Active;
        live.Text = "LIVE (not included in completed analytics) · " + (active.Count == 0 ? "Idle — no model call in progress" : string.Join(" · ", active.Select(call => call.Harness + ": " + call.Phase + " / " + call.Model + " / " + UiFormat.Number(call.Rate, " tok/s", call.Estimated)))) + "\nLive target: " + (tracker.Target ?? "Auto") + " · " + tracker.NetworkStatus;
    }

    private void RebuildReport()
    {
        var filter = ReadFilter();
        if (filter is null) return;
        report = DashboardReport.Create(records, filter);
        synchronizing = true;
        try
        {
            // Each list offers only what has calls with the other two selections.
            Populate(harness, report.HarnessOptions.Select(name => new FilterChoice(name, name)), "All harnesses");
            Populate(provider, report.ProviderOptions.Select(identity => new FilterChoice(identity.Key, identity.Name + (identity.Unverified ? " · unverified" : ""))), "All providers");
            Populate(model, report.ModelOptions.Select(name => new FilterChoice(name, name)), "All models");
        }
        finally { synchronizing = false; }
        RenderOverview(report);
        latencyTrend.SetTrends(report.Trends, false);
        speedTrend.SetTrends(report.Trends, true);
        latencyDistribution.SetDistribution(report.Records.Select(UiFormat.Latency).Where(value => value.HasValue).Select(value => value!.Value), "s");
        speedDistribution.SetDistribution(report.Records.Select(UiFormat.Speed).Where(value => value.HasValue).Select(value => value!.Value), "tok/s");
        metricExplanation.Text = "TTFT: " + UiFormat.Count(report.Summary.LatencyCount) + " measured calls. Generation-window speed: " + UiFormat.Count(report.Summary.SpeedCount) + " measured calls. " + UiFormat.Count(report.Summary.RoundTripCount) + " whole-request speed fallbacks excluded from speed percentiles. Errors / interruptions and invalid values are excluded from both distributions; estimates remain included and marked in Calls.";
        SortRows();
        ShowPage();
        status.Text = UiFormat.Count(report.Summary.Count) + " calls in filter · " + UiFormat.Count(report.Summary.EstimatedCount) + " estimated · " + UiFormat.Count(report.Summary.InterruptedCount) + " interrupted / errors · " + UiFormat.Count(history.SkippedLines) + " malformed history lines skipped · " + history.Home;
    }

    private static void Populate(ComboBox combo, IEnumerable<FilterChoice> choices, string allLabel)
    {
        var selected = combo.SelectedItem as FilterChoice;
        var options = new List<FilterChoice> { new(null, allLabel) };
        options.AddRange(choices);
        if (selected?.Key is string key && !options.Any(choice => choice.Key == key)) options.Add(selected);
        var existing = combo.Items.Cast<FilterChoice>().ToArray();
        if (!existing.SequenceEqual(options))
        {
            combo.ItemsSource = null;
            combo.Items.Clear();
            foreach (var option in options) combo.Items.Add(option);
        }
        combo.SelectedItem = combo.Items.Cast<FilterChoice>().FirstOrDefault(choice => choice.Key == selected?.Key) ?? combo.Items[0];
    }

    private void RenderOverview(DashboardReport data)
    {
        overview.Children.Clear();
        var metrics = new UniformGrid { Columns = 4, Margin = new Thickness(0, 12, 0, 4) };
        Metric(metrics, "Calls / output tokens", UiFormat.Count(data.Summary.Count), UiFormat.Count(data.Summary.OutputTokens) + " output tokens");
        Metric(metrics, "Generation speed", UiFormat.Number(data.Summary.MedianTPS, " tok/s"), "p95 " + UiFormat.Number(data.Summary.P95TPS, " tok/s") + " · n=" + data.Summary.SpeedCount);
        Metric(metrics, "Time to first token", UiFormat.Number(data.Summary.MedianTTFT, " s"), "p95 " + UiFormat.Number(data.Summary.P95TTFT, " s") + " · n=" + data.Summary.LatencyCount);
        Metric(metrics, "Provenance / outcomes", UiFormat.Count(data.Summary.EstimatedCount) + " estimated", UiFormat.Count(data.Summary.InterruptedCount) + " interrupted or errors");
        overview.Children.Add(metrics);
        overview.Children.Add(Theme.Text("Provider × harness · select a cell to inspect its calls", 17));
        overview.Children.Add(Theme.Text("Endpoint attribution is separate from model identity. “Unverified” includes reported labels and passive estimates. A dash is unavailable, not zero.", 12, true));
        if (data.Groups.Count == 0)
        {
            overview.Children.Add(Theme.Card(Theme.Text("No completed calls match these filters. Live collection stays independent. Try All history or reset the filters.")));
            return;
        }
        var providerKeys = data.Groups.Select(group => group.Provider.Key).ToHashSet(StringComparer.Ordinal);
        var providers = data.Providers.Where(identity => providerKeys.Contains(identity.Key)).OrderBy(identity => identity.Name, StringComparer.OrdinalIgnoreCase).ToArray();
        var groupMap = data.Groups.ToDictionary(group => (group.Provider.Key, group.Harness));
        var harnesses = data.Groups.Select(group => group.Harness).Distinct().OrderBy(name => name, StringComparer.OrdinalIgnoreCase).ToArray();
        var matrix = new Grid { Margin = new Thickness(0, 10, 0, 10) };
        matrix.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(190) });
        foreach (var name in harnesses) matrix.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(190) });
        matrix.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        for (var column = 0; column < harnesses.Length; column++) Place(matrix, Theme.Text(harnesses[column], 13), 0, column + 1);
        for (var row = 0; row < providers.Length; row++)
        {
            var identity = providers[row];
            matrix.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
            var label = Theme.Text(identity.Name + (identity.Unverified ? "\nUnverified endpoint" : "\nObserved endpoint"), 13);
            label.ToolTip = identity.Evidence;
            Place(matrix, label, row + 1, 0);
            for (var column = 0; column < harnesses.Length; column++)
            {
                var name = harnesses[column];
                groupMap.TryGetValue((identity.Key, name), out var group);
                var content = group is null ? "No calls" : UiFormat.Count(group.Summary.Count) + " calls\np50 " + UiFormat.Number(group.Summary.MedianTPS, " tok/s") + "\nTTFT " + UiFormat.Number(group.Summary.MedianTTFT, " s");
                var button = new Button { Content = Theme.Text(content, 13), Margin = new Thickness(4), HorizontalContentAlignment = HorizontalAlignment.Left, IsEnabled = group is not null, ToolTip = group?.Provider.Evidence };
                AutomationProperties.SetName(button, identity.Name + " / " + name + ": " + content.Replace('\n', ' '));
                button.Click += (_, _) => DrillDown(identity.Key, name);
                Place(matrix, button, row + 1, column + 1);
            }
        }
        overview.Children.Add(matrix);
        var sources = data.Records.GroupBy(UiFormat.Source).OrderBy(group => group.Key).Select(group => group.Key + ": " + UiFormat.Count(group.Count()));
        overview.Children.Add(Theme.Text("Sources · " + string.Join(" · ", sources), 12, true));
        overview.Children.Add(Theme.Text("Percentiles use valid nonnegative measurements from non-error, non-interrupted calls. Speed requires a positive generation window; whole-request fallback is never presented as generation speed. Estimated observations are not exact measurements.", 12, true));
    }

    private static void Metric(Panel panel, string title, string value, string detail)
    {
        var content = new StackPanel();
        content.Children.Add(Theme.Text(title, 12, true));
        content.Children.Add(Theme.Text(value, 23));
        content.Children.Add(Theme.Text(detail, 12, true));
        var card = Theme.Card(content);
        card.Margin = new Thickness(0, 0, 10, 10);
        panel.Children.Add(card);
    }

    private static void Place(Grid grid, UIElement element, int row, int column)
    {
        Grid.SetRow(element, row);
        Grid.SetColumn(element, column);
        grid.Children.Add(element);
    }

    private void DrillDown(string providerKey, string harnessName)
    {
        synchronizing = true;
        provider.SelectedItem = provider.Items.Cast<FilterChoice>().FirstOrDefault(choice => choice.Key == providerKey);
        harness.SelectedItem = harness.Items.Cast<FilterChoice>().FirstOrDefault(choice => choice.Key == harnessName);
        synchronizing = false;
        page = 0;
        RebuildReport();
        tabs.SelectedIndex = 2;
        calls.Focus();
    }

    private void SortCalls(object sender, DataGridSortingEventArgs args)
    {
        args.Handled = true;
        sortDirection = sortColumn == args.Column.SortMemberPath && sortDirection == ListSortDirection.Ascending ? ListSortDirection.Descending : ListSortDirection.Ascending;
        sortColumn = args.Column.SortMemberPath;
        foreach (var column in calls.Columns) column.SortDirection = column == args.Column ? sortDirection : null;
        page = 0;
        SortRows();
        ShowPage();
    }

    private void SortRows()
    {
        if (report is null) return;
        var rows = report.Records.Select(record => new CallRow(record)).ToList();
        rows.Sort((first, second) =>
        {
            var comparison = Compare(first, second);
            if (comparison == 0) comparison = first.Record.Id.CompareTo(second.Record.Id);
            return comparison;
        });
        sorted = rows;
    }

    private int Compare(CallRow first, CallRow second)
    {
        if (sortColumn is "Latency" or "Speed")
        {
            var a = sortColumn == "Latency" ? UiFormat.Latency(first.Record) : UiFormat.Speed(first.Record);
            var b = sortColumn == "Latency" ? UiFormat.Latency(second.Record) : UiFormat.Speed(second.Record);
            if (!a.HasValue) return b.HasValue ? 1 : 0;
            if (!b.HasValue) return -1;
            return Direction(a.Value.CompareTo(b.Value));
        }
        var result = sortColumn switch
        {
            "StartedAt" => first.Record.StartedAt.CompareTo(second.Record.StartedAt),
            "Output" => first.Record.OutputTokens.CompareTo(second.Record.OutputTokens),
            "Harness" => StringComparer.OrdinalIgnoreCase.Compare(first.Harness, second.Harness),
            "Provider" => StringComparer.OrdinalIgnoreCase.Compare(first.ProviderDisplay, second.ProviderDisplay),
            "Model" => StringComparer.OrdinalIgnoreCase.Compare(first.Model, second.Model),
            "Outcome" => StringComparer.OrdinalIgnoreCase.Compare(first.Outcome, second.Outcome),
            "Source" => StringComparer.OrdinalIgnoreCase.Compare(first.Source, second.Source),
            _ => first.Record.StartedAt.CompareTo(second.Record.StartedAt)
        };
        return Direction(result);
    }
    private int Direction(int comparison) => sortDirection == ListSortDirection.Ascending ? comparison : -comparison;

    private void ShowPage()
    {
        page = Math.Clamp(page, 0, Math.Max(0, (sorted.Count - 1) / PageSize));
        var selectedId = (calls.SelectedItem as CallRow)?.Record.Id;
        var rows = sorted.Skip(page * PageSize).Take(PageSize).ToArray();
        calls.ItemsSource = rows;
        calls.SelectedItem = rows.FirstOrDefault(row => row.Record.Id == selectedId);
        previous.IsEnabled = page > 0;
        next.IsEnabled = (page + 1) * PageSize < sorted.Count;
        pageLabel.Text = sorted.Count == 0 ? "No calls" : "Page " + (page + 1) + " of " + ((sorted.Count + PageSize - 1) / PageSize) + " · " + (page * PageSize + 1) + "–" + Math.Min(sorted.Count, (page + 1) * PageSize) + " / " + UiFormat.Count(sorted.Count);
        if (calls.SelectedItem is null) Inspect(null);
    }

    private void Inspect(RequestRecord? record)
    {
        inspector.Children.Clear();
        inspector.Children.Add(Theme.Text("Call inspector", 17));
        if (record is null)
        {
            inspector.Children.Add(Theme.Text("Select a completed call with the mouse or keyboard. Column headers sort the entire filtered result, not just this page.", 13, true));
            return;
        }
        var identity = ProviderIdentity.From(record);
        Detail("Started", record.StartedAt.ToLocalTime().ToString("F", CultureInfo.CurrentCulture));
        Detail("Harness / model", record.Harness + "\n" + record.Model);
        Detail("Provider / endpoint", identity.Name + "\n" + (identity.Host ?? "Endpoint host unavailable") + "\n" + identity.Evidence);
        Detail("Observation", UiFormat.Source(record) + (record.TokensEstimated ? " · estimated token counts" : " · no token-estimate flag") + "\n" + (record.Source is "log" or "network" or "proxy" ? "" : "Legacy / unknown source: exactness is not established."));
        Detail("Outcome", UiFormat.Outcome(record) + " · " + (record.Streamed ? "streamed" : "non-streamed"));
        Detail("Generation speed", UiFormat.Number(UiFormat.Speed(record), " tok/s", record.TokensEstimated) + "\n" + (UiFormat.Speed(record).HasValue ? "Positive generation window; eligible for speed percentiles." : "Not eligible for generation-speed percentiles. Missing timing, interruption or an error is not zero speed."));
        Detail("Recorded speed", UiFormat.Number(record.Tps, " tok/s", record.TokensEstimated) + (record.Generation is null or 0 ? " · whole-request fallback / generation timing unavailable" : ""));
        Detail("Timing (seconds)", "TTFT " + UiFormat.Number(record.Ttft) + "\nFirst visible " + UiFormat.Number(record.FirstVisible) + "\nFirst byte " + UiFormat.Number(record.Ttfb) + "\nGeneration " + UiFormat.Number(record.Generation) + "\nTotal request " + UiFormat.Number(record.Total));
        Detail("Tokens", "Input " + OptionalCount(record.InputTokens) + "\nCached input " + OptionalCount(record.CachedInputTokens) + "\nOutput " + (record.TokensEstimated ? "~" : "") + UiFormat.Count(record.OutputTokens) + "\nReasoning " + OptionalCount(record.ReasoningTokens));
        Detail("API format", record.Format);
        Detail("Record ID", record.Id.ToString());
        if (!string.IsNullOrWhiteSpace(record.SourceKey)) Detail("Stable source key", record.SourceKey);
    }

    private static string OptionalCount(int? count) => count is int number ? UiFormat.Count(number) : "unavailable";
    private void Detail(string label, string value)
    {
        inspector.Children.Add(Theme.Text(label, 12, true));
        var field = new TextBox { Text = value, IsReadOnly = true, TextWrapping = TextWrapping.Wrap, BorderThickness = new Thickness(0), Padding = new Thickness(0), Margin = new Thickness(0, 0, 0, 10) };
        AutomationProperties.SetName(field, label);
        inspector.Children.Add(field);
    }

    protected override void OnClosing(CancelEventArgs args)
    {
        if (!allowClose) { args.Cancel = true; Hide(); }
        base.OnClosing(args);
    }

    public void DisposeWindow()
    {
        disposed = true;
        tracker.Changed -= TrackerChanged;
        history.Recorded -= HistoryRecorded;
        allowClose = true;
        Close();
    }

    private sealed record FilterChoice(string? Key, string Label)
    {
        public override string ToString() => Label;
    }

    private sealed class CallRow
    {
        public RequestRecord Record { get; }
        public string StartedDisplay => Record.StartedAt.ToLocalTime().ToString("g", CultureInfo.CurrentCulture);
        public string Harness => Record.Harness;
        public string Model => Record.Model;
        public string ProviderDisplay { get; }
        public string LatencyDisplay => UiFormat.Number(UiFormat.Latency(Record));
        public string SpeedDisplay => UiFormat.Number(UiFormat.Speed(Record), estimated: Record.TokensEstimated);
        public string OutputDisplay => (Record.TokensEstimated ? "~" : "") + UiFormat.Count(Record.OutputTokens);
        public string Outcome => UiFormat.Outcome(Record);
        public string Source => UiFormat.Source(Record);
        public CallRow(RequestRecord record)
        {
            Record = record;
            var identity = ProviderIdentity.From(record);
            ProviderDisplay = identity.Name + (identity.Unverified ? " *" : "");
        }
    }
}
