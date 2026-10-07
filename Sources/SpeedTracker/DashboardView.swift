import Foundation
import SpeedTrackerCore
import SwiftUI

struct DashboardView: View {
    @ObservedObject var dashboard: DashboardStore

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            header
            filters
            DashboardLiveStrip()
            navigation
            if let error = dashboard.loadError {
                notice(symbol: "exclamationmark.triangle", color: .orange) {
                    Text("History could not be refreshed. \(error)")
                    if !dashboard.report.records.isEmpty {
                        Text("Previously loaded calls remain visible.").foregroundStyle(.secondary)
                    }
                }
            }
            if dashboard.skippedLines > 0 {
                notice(symbol: "doc.badge.ellipsis", color: .secondary) {
                    Text("\(dashboard.skippedLines.formatted()) malformed or incomplete history lines were skipped; the remaining calls are shown.")
                }
            }
            content
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        }
        .padding(20)
        .frame(minWidth: 1000, minHeight: 680)
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(systemName: "bolt.fill")
                .font(.system(size: 18, weight: .bold))
                .foregroundStyle(Theme.accent)
                .frame(width: 40, height: 40)
                .background(Theme.accentSoft, in: RoundedRectangle(cornerRadius: 11))
            VStack(alignment: .leading, spacing: 2) {
                Text("Speed Tracker")
                    .font(.system(size: 22, weight: .semibold, design: .rounded))
                Text("Local model-call dashboard · provider evidence, not model-name guesses")
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.muted)
            }
            Spacer()
            VStack(alignment: .trailing, spacing: 3) {
                if dashboard.isLoading {
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.small)
                        Text("Reading local history…")
                    }
                } else if let date = dashboard.lastRefreshed {
                    Text("Updated \(date.formatted(date: .omitted, time: .shortened))")
                } else {
                    Text("Local history")
                }
                Text("Dashboard filters do not change the menu-bar target")
                    .font(.system(size: 10))
            }
            .font(.system(size: 11))
            .foregroundStyle(Theme.muted)
            Button(action: dashboard.refresh) {
                Label("Refresh", systemImage: "arrow.clockwise")
            }
            .keyboardShortcut("r", modifiers: .command)
            .disabled(dashboard.isLoading)
            .help("Reload local call history (⌘R)")
        }
    }

    private var filters: some View {
        HStack(alignment: .bottom, spacing: 12) {
            filterField("Date range") {
                Picker("Date range", selection: $dashboard.range) {
                    ForEach(DashboardRange.allCases) { Text($0.title).tag($0) }
                }
            }
            .frame(width: 124)
            filterField("Harness") {
                Picker("Harness", selection: $dashboard.harnessFilter) {
                    Text("All harnesses").tag("")
                    ForEach(dashboard.report.harnessOptions, id: \.self) { Text($0).tag($0) }
                    if !dashboard.harnessFilter.isEmpty && !dashboard.report.harnessOptions.contains(dashboard.harnessFilter) {
                        Text(dashboard.harnessFilter + " · no calls").tag(dashboard.harnessFilter)
                    }
                }
            }
            filterField("Provider / endpoint") {
                Picker("Provider or endpoint", selection: $dashboard.providerFilter) {
                    Text("All providers").tag("")
                    ForEach(dashboard.report.providerOptions) { provider in
                        Text(provider.displayName + (provider.isUnverified ? " · unverified" : "")).tag(provider.id)
                    }
                    if !dashboard.providerFilter.isEmpty && !dashboard.report.providerOptions.contains(where: { $0.id == dashboard.providerFilter }) {
                        Text(dashboard.providerFilter + " · no calls").tag(dashboard.providerFilter)
                    }
                }
            }
            filterField("Model") {
                Picker("Model", selection: $dashboard.modelFilter) {
                    Text("All models").tag("")
                    ForEach(dashboard.report.modelOptions.filter { !$0.isEmpty }, id: \.self) { Text($0).tag($0) }
                    if !dashboard.modelFilter.isEmpty && !dashboard.report.modelOptions.contains(dashboard.modelFilter) {
                        Text(dashboard.modelFilter + " · no calls").tag(dashboard.modelFilter)
                    }
                }
            }
            Button(action: dashboard.clearFilters) {
                Image(systemName: "line.3.horizontal.decrease.circle")
                    .font(.system(size: 16))
                    .frame(width: 24, height: 20)
            }
            .disabled(!hasFilters)
            .accessibilityLabel("Clear harness, provider, and model filters")
            .help("Clear harness, provider and model filters; keep the date range. Each filter only offers choices that have calls with the other two.")
        }
        .controlSize(.regular)
    }

    private func filterField<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            SectionLabel(title)
            content().labelsHidden().frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var navigation: some View {
        HStack {
            HStack(spacing: 3) {
                ForEach(DashboardSection.allCases) { section in
                    Button { dashboard.section = section } label: {
                        Text(section.title)
                            .font(.system(size: 12, weight: dashboard.section == section ? .semibold : .medium))
                            .padding(.horizontal, 18)
                            .padding(.vertical, 7)
                            .foregroundStyle(dashboard.section == section ? Color.primary : Color.secondary)
                            .background(dashboard.section == section ? Theme.panelRaised : .clear, in: RoundedRectangle(cornerRadius: 7))
                    }
                    .buttonStyle(.plain)
                    .keyboardShortcut(shortcut(for: section), modifiers: .command)
                    .accessibilityAddTraits(dashboard.section == section ? .isSelected : [])
                }
            }
            .padding(3)
            .background(Theme.panel, in: RoundedRectangle(cornerRadius: 10))
            Spacer()
            Text(scopeDescription)
                .font(.system(size: 11))
                .foregroundStyle(Theme.muted)
                .lineLimit(1)
                .truncationMode(.middle)
                .help(scopeDescription)
        }
    }

    @ViewBuilder private var content: some View {
        if dashboard.isLoading && dashboard.report.records.isEmpty {
            DashboardEmptyState(symbol: "clock.arrow.circlepath", title: "Reading local call history", message: "Existing records are being grouped by provider, harness and model. Live activity above remains independent of this date range.", loading: true)
        } else if dashboard.report.records.isEmpty {
            VStack(spacing: 14) {
                DashboardEmptyState(symbol: dashboard.loadError == nil ? "chart.bar.xaxis" : "exclamationmark.triangle", title: dashboard.loadError == nil ? "No calls in this view" : "History is unavailable", message: dashboard.loadError == nil ? "Use a detected coding harness as usual, choose a wider date range, or clear the filters. Calls appear when a recorded request completes; missing measurements are never treated as zero." : "Live activity is still shown above. Retry reading local history to display completed calls.")
                HStack {
                    if hasFilters { Button("Clear filters", action: dashboard.clearFilters) }
                    Button("Reload history", action: dashboard.refresh).disabled(dashboard.isLoading)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            switch dashboard.section {
            case .overview:
                ScrollView {
                    VStack(alignment: .leading, spacing: 16) {
                        DashboardSummaryStrip(summary: dashboard.report.summary)
                        DashboardMatrix(report: dashboard.report, metric: $dashboard.metric, select: dashboard.select)
                        HStack(alignment: .top, spacing: 14) {
                            DashboardCoverage(report: dashboard.report)
                            DashboardInterpretation(summary: dashboard.report.summary)
                        }
                    }
                    .padding(.bottom, 8)
                }
            case .trends:
                ScrollView {
                    VStack(alignment: .leading, spacing: 16) {
                        DashboardSummaryStrip(summary: dashboard.report.summary)
                        HStack(alignment: .top, spacing: 14) {
                            DashboardTrendChart(trends: dashboard.report.trends, summary: dashboard.report.summary, metric: .latency)
                            DashboardTrendChart(trends: dashboard.report.trends, summary: dashboard.report.summary, metric: .speed)
                        }
                        HStack(alignment: .top, spacing: 14) {
                            DashboardDistributionChart(bins: dashboard.report.histogram(for: .latency), summary: dashboard.report.summary, metric: .latency)
                            DashboardDistributionChart(bins: dashboard.report.histogram(for: .speed), summary: dashboard.report.summary, metric: .speed)
                        }
                        Text("Each date bucket summarizes recorded calls, not token-by-token playback. Missing measurements are omitted. p95 TTFT describes the slow latency tail; p95 speed describes the fast speed tail. Interrupted calls and known errors are excluded from performance statistics.")
                            .font(.system(size: 11))
                            .foregroundStyle(Theme.muted)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .padding(.bottom, 8)
                }
            case .calls:
                DashboardCallExplorer(dashboard: dashboard)
            }
        }
    }

    private func notice<Content: View>(symbol: String, color: Color, @ViewBuilder content: () -> Content) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: symbol).foregroundStyle(color)
            VStack(alignment: .leading, spacing: 2, content: content)
        }
        .font(.system(size: 11))
        .padding(9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(color.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
        .accessibilityElement(children: .combine)
    }

    private var hasFilters: Bool {
        !dashboard.harnessFilter.isEmpty || !dashboard.providerFilter.isEmpty || !dashboard.modelFilter.isEmpty
    }

    private var scopeDescription: String {
        let provider = dashboard.report.providers.first { $0.id == dashboard.providerFilter }?.displayName ?? "All providers"
        return [dashboard.range.title, provider, dashboard.harnessFilter.isEmpty ? "All harnesses" : dashboard.harnessFilter, dashboard.modelFilter.isEmpty ? "All models" : dashboard.modelFilter].joined(separator: " · ")
    }

    private func shortcut(for section: DashboardSection) -> KeyEquivalent {
        switch section {
        case .overview: return "1"
        case .trends: return "2"
        case .calls: return "3"
        }
    }
}

private struct DashboardLiveStrip: View {
    @EnvironmentObject private var metrics: MetricsStore
    private var requests: [LiveRequest] { metrics.dashboardActive }
    private var now: TimeInterval { metrics.now }

    var body: some View {
        HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                SectionLabel("Live · all harnesses")
                HStack(spacing: 5) {
                    Circle().fill(requests.isEmpty ? Color.secondary : Theme.accent).frame(width: 6, height: 6)
                    Text(requests.isEmpty ? "Idle" : "\(requests.count) active")
                        .font(.system(size: 12, weight: .semibold))
                }
            }
            .frame(width: 130, alignment: .leading)
            if requests.isEmpty {
                Text("No model call in progress. Completed-call statistics below use your history filters.")
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.muted)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                ScrollView(.horizontal) {
                    HStack(spacing: 10) {
                        ForEach(requests) { request in
                            liveCall(request)
                        }
                    }
                    .padding(.vertical, 2)
                }
                .scrollIndicators(.hidden)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
        .frame(minHeight: 64)
        .background(Theme.panel, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Theme.border, lineWidth: 1))
    }

    private func liveCall(_ request: LiveRequest) -> some View {
        let snapshot = request.snapshot
        let fresh = snapshot.lastTokenUptime.map { now - $0 <= 1.2 } ?? false
        return VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                HarnessDot(harness: snapshot.harness, size: 6)
                Text(snapshot.harness).fontWeight(.semibold)
                Text(snapshot.model.isEmpty ? "Unknown model" : snapshot.model)
                    .foregroundStyle(Theme.muted)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            HStack(spacing: 8) {
                Text(phase(snapshot.phase, fresh: fresh))
                    .foregroundStyle(snapshot.phase == .waiting ? Color.orange : Theme.accent)
                if let rate = request.smoothedRate, rate.isFinite, rate >= 0 {
                    Text("\(fresh ? "~" : "Held ~")\(Format.wholeRate(rate)) tok/s")
                        .foregroundStyle(fresh ? Color.primary : Color.secondary)
                        .help(fresh ? "Smoothed estimate while tokens arrive; not a completed-call measurement" : "Last smoothed speed is held through a pause, not a fresh measurement")
                }
                Text("\(Format.durationText(max(0, now - snapshot.startUptime))) elapsed")
                    .foregroundStyle(Theme.muted)
            }
            .font(.system(size: 10))
            .monospacedDigit()
        }
        .font(.system(size: 11))
        .frame(width: 320, alignment: .leading)
        .accessibilityElement(children: .combine)
    }

    private func phase(_ phase: LiveSnapshot.Phase, fresh: Bool) -> String {
        switch phase {
        case .waiting: return "Waiting for first token"
        case .thinking: return "Thinking"
        case .streaming: return fresh ? "Streaming" : "Paused / waiting"
        }
    }
}

private struct DashboardSummaryStrip: View {
    let summary: DashboardSummary

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            DashboardFigure(title: "Calls", value: summary.count.formatted(), caption: "Recorded in this view")
            DashboardFigure(title: "Median TTFT", value: Format.durationText(summary.medianTTFT), caption: "n = \(summary.latencyCount)")
            DashboardFigure(title: "p95 TTFT", value: Format.durationText(summary.p95TTFT), caption: "95th percentile latency")
            DashboardFigure(title: "Median speed", value: DashboardDisplay.summaryRate(summary.medianTPS, summary: summary), caption: "tok/s · n = \(summary.speedCount)")
            DashboardFigure(title: "p95 speed", value: DashboardDisplay.summaryRate(summary.p95TPS, summary: summary), caption: "tok/s · generation only")
            DashboardFigure(title: "Estimated", value: String(format: "%.0f%%", Double(summary.estimatedCount) / Double(max(1, summary.count)) * 100), caption: "\(summary.estimatedCount) calls · token estimates")
            DashboardFigure(title: "Interrupted / errors", value: summary.interruptedCount.formatted(), caption: "Not a success-rate metric")
        }
    }
}

private struct DashboardFigure: View {
    let title: String
    let value: String
    let caption: String

    var body: some View {
        Card(padding: 10) {
            VStack(alignment: .leading, spacing: 5) {
                Text(title).font(.system(size: 10, weight: .medium)).foregroundStyle(Theme.muted)
                Text(value)
                    .font(.system(size: 19, weight: .semibold, design: .rounded))
                    .monospacedDigit()
                    .lineLimit(1)
                    .minimumScaleFactor(0.7)
                Text(caption).font(.system(size: 9)).foregroundStyle(Theme.muted).lineLimit(2)
            }
            .frame(height: 70, alignment: .topLeading)
        }
        .accessibilityElement(children: .combine)
    }
}

private struct DashboardMatrix: View {
    let report: DashboardReport
    let metric: Binding<DashboardMetric>
    let select: (DashboardGroup) -> Void

    private struct CellKey: Hashable {
        let provider: String
        let harness: String
    }

    var body: some View {
        let providerIDs = Set(report.groups.map { $0.provider.id })
        let providers = report.providers.filter { providerIDs.contains($0.id) }
        let harnesses = Array(Set(report.groups.map(\.harness))).sorted()
        let cells = Dictionary(report.groups.map { (CellKey(provider: $0.provider.id, harness: $0.harness), $0) }, uniquingKeysWith: { first, _ in first })
        let values = report.groups.compactMap { metric.wrappedValue.value(in: $0.summary) }
        let lower = values.min() ?? 0
        let upper = values.max() ?? 0
        return Card(padding: 14) {
            VStack(alignment: .leading, spacing: 12) {
                HStack {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Provider × harness").font(.system(size: 15, weight: .semibold))
                        Text("Choose a cell to inspect its trends; keep the current date range and model filter.")
                            .font(.system(size: 11)).foregroundStyle(Theme.muted)
                    }
                    Spacer()
                    Picker("Matrix color metric", selection: metric) {
                        ForEach(DashboardMetric.allCases) { Text($0.title).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .frame(width: 350)
                }
                ScrollView(.horizontal) {
                    VStack(alignment: .leading, spacing: 6) {
                        HStack(spacing: 6) {
                            SectionLabel("Endpoint identity").frame(width: 190, alignment: .leading)
                            ForEach(harnesses, id: \.self) { harness in
                                HStack(spacing: 5) {
                                    HarnessDot(harness: harness, size: 6)
                                    Text(harness).font(.system(size: 11, weight: .semibold)).lineLimit(1)
                                }
                                .frame(width: 130)
                            }
                        }
                        .padding(.bottom, 4)
                        ForEach(providers) { provider in
                            HStack(spacing: 6) {
                                providerLabel(provider).frame(width: 190, alignment: .leading)
                                ForEach(harnesses, id: \.self) { harness in
                                    if let group = cells[CellKey(provider: provider.id, harness: harness)] {
                                        matrixCell(group, lower: lower, upper: upper)
                                    } else {
                                        VStack(spacing: 4) {
                                            Text("–").font(.system(size: 18))
                                            Text("No recorded calls").font(.system(size: 9)).foregroundStyle(Theme.muted)
                                        }
                                        .frame(width: 130, height: 72)
                                        .background(Theme.panel, in: RoundedRectangle(cornerRadius: 9))
                                        .accessibilityLabel("\(provider.displayName), \(harness): no recorded calls")
                                    }
                                }
                            }
                        }
                    }
                    .padding(.bottom, 4)
                }
                HStack(alignment: .top, spacing: 12) {
                    HStack(spacing: 5) {
                        RoundedRectangle(cornerRadius: 2).fill(Theme.accent.opacity(0.12)).frame(width: 12, height: 9)
                        RoundedRectangle(cornerRadius: 2).fill(Theme.accent.opacity(0.36)).frame(width: 12, height: 9)
                        Text(intensityCaption)
                    }
                    Spacer()
                    Text("~ includes token estimates · n < 5 is a low sample · – means unmeasured")
                }
                .font(.system(size: 10))
                .foregroundStyle(Theme.muted)
                Text("Providers follow recorded endpoint/route evidence. Legacy log defaults are unverified; a gateway does not prove its downstream provider. No provider is inferred from a model name.")
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.muted)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    private func providerLabel(_ provider: ProviderIdentity) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(provider.displayName).font(.system(size: 12, weight: .semibold)).lineLimit(2)
            if provider.isUnverified {
                Label("Endpoint unverified", systemImage: "exclamationmark.shield")
                    .font(.system(size: 9)).foregroundStyle(Color.orange)
            } else {
                Text(provider.host ?? "Recorded route")
                    .font(.system(size: 9)).foregroundStyle(Theme.muted).lineLimit(1).truncationMode(.middle)
            }
        }
        .help(provider.evidence)
        .accessibilityElement(children: .combine)
        .accessibilityHint(provider.evidence)
    }

    private func matrixCell(_ group: DashboardGroup, lower: Double, upper: Double) -> some View {
        let value = metric.wrappedValue.value(in: group.summary)
        let fraction = value.map { upper > lower ? ($0 - lower) / (upper - lower) : 0.5 } ?? 0
        let intensity = metric.wrappedValue == .latency ? 1 - fraction : fraction
        let sample = sampleCount(group.summary)
        return Button { select(group) } label: {
            VStack(spacing: 4) {
                Text(DashboardDisplay.metric(metric.wrappedValue, summary: group.summary))
                    .font(.system(size: 16, weight: .semibold, design: .rounded))
                    .monospacedDigit()
                    .foregroundStyle(Color.primary)
                Text("\(group.summary.count) calls · n = \(sample)")
                    .font(.system(size: 9)).foregroundStyle(Color.secondary)
                Text(sample < 5 ? "Low sample" : metric.wrappedValue == .speed ? "Generation window" : metric.wrappedValue == .latency ? "First token" : "Recorded volume")
                    .font(.system(size: 9, weight: sample < 5 ? .semibold : .regular))
                    .foregroundStyle(sample < 5 ? Color.orange : Color.secondary)
            }
            .frame(width: 130, height: 72)
            .background(value == nil ? Theme.panel : Theme.accent.opacity(0.10 + 0.26 * min(1, max(0, intensity))), in: RoundedRectangle(cornerRadius: 9))
            .overlay(RoundedRectangle(cornerRadius: 9).strokeBorder(Theme.border, lineWidth: 1))
            .contentShape(RoundedRectangle(cornerRadius: 9))
        }
        .buttonStyle(.plain)
        .help("\(group.provider.evidence)\n\(sample) measured samples. Open this provider and harness in Trends.")
        .accessibilityLabel("\(group.provider.displayName), \(group.harness), \(metric.wrappedValue.title): \(DashboardDisplay.metric(metric.wrappedValue, summary: group.summary)), \(group.summary.count) calls, \(sample) samples\(sample < 5 ? ", low sample" : "")")
        .accessibilityHint("Filters to this provider and harness and opens Trends")
    }

    private func sampleCount(_ summary: DashboardSummary) -> Int {
        switch metric.wrappedValue {
        case .speed: return summary.speedCount
        case .latency: return summary.latencyCount
        case .calls, .tokens: return summary.count
        }
    }

    private var intensityCaption: String {
        switch metric.wrappedValue {
        case .speed: return "Deeper color = faster median generation"
        case .latency: return "Deeper color = lower median TTFT"
        case .calls: return "Deeper color = more calls"
        case .tokens: return "Deeper color = more output tokens"
        }
    }
}

private struct DashboardCoverage: View {
    let report: DashboardReport

    var body: some View {
        Card(padding: 14) {
            VStack(alignment: .leading, spacing: 12) {
                Text("Source coverage").font(.system(size: 15, weight: .semibold))
                ForEach(report.sources) { source in
                    VStack(alignment: .leading, spacing: 5) {
                        HStack {
                            Label(DashboardDisplay.source(source.id), systemImage: sourceSymbol(source.id))
                            Spacer()
                            Text("\(source.count.formatted()) calls · \(Int((Double(source.count) / Double(max(1, report.summary.count)) * 100).rounded()))%")
                                .monospacedDigit()
                                .foregroundStyle(Theme.muted)
                        }
                        .font(.system(size: 11))
                        GeometryReader { proxy in
                            Capsule().fill(Theme.panelRaised)
                            Capsule().fill(sourceColor(source.id)).frame(width: proxy.size.width * CGFloat(source.count) / CGFloat(max(1, report.summary.count)))
                        }
                        .frame(height: 5)
                        Text(sourceCaption(source.id)).font(.system(size: 10)).foregroundStyle(Theme.muted)
                    }
                    .accessibilityElement(children: .combine)
                }
                Text("Source counts describe observed calls, not every request a harness may have made.")
                    .font(.system(size: 10)).foregroundStyle(Theme.muted)
            }
        }
    }

    private func sourceSymbol(_ source: String) -> String {
        switch source {
        case "log": return "doc.text"
        case "network": return "network"
        case "proxy": return "arrow.triangle.branch"
        default: return "questionmark.circle"
        }
    }

    private func sourceColor(_ source: String) -> Color {
        switch source {
        case "log": return .purple
        case "network": return .blue
        case "proxy": return Theme.accent
        default: return .secondary
        }
    }

    private func sourceCaption(_ source: String) -> String {
        switch source {
        case "log": return "Harness-reported usage; latency may be absent and reported status is not proof of HTTP success."
        case "network": return "Passive traffic timing; token counts are estimated and timing coverage may be limited."
        case "proxy": return "Observed request/response timings; provider usage is used when it is reported."
        default: return "Legacy or unspecified observation source; use call details to inspect available evidence."
        }
    }
}

private struct DashboardInterpretation: View {
    let summary: DashboardSummary

    var body: some View {
        Card(padding: 14) {
            VStack(alignment: .leading, spacing: 12) {
                Text("Volume & measurement coverage").font(.system(size: 15, weight: .semibold))
                HStack(alignment: .firstTextBaseline, spacing: 5) {
                    Text((summary.estimatedCount > 0 ? "~" : "") + Format.tokens(summary.outputTokens))
                        .font(.system(size: 28, weight: .semibold, design: .rounded)).monospacedDigit()
                    Text("output tokens").font(.system(size: 11)).foregroundStyle(Theme.muted)
                }
                coverageRow("First-token latency", count: summary.latencyCount)
                coverageRow("Generation-window speed", count: summary.speedCount)
                coverageRow("Whole-request speed only", count: summary.roundTripCount)
                Divider()
                Text("Whole-request speed divides output tokens by request-to-end time. It is shown separately in call details and is not pooled with generation-window TPS.")
                Text("~ marks figures containing estimated tokens. Counts and token volume include interrupted/error calls; latency and generation-speed statistics do not. Logs do not establish a success rate.")
                Text("TTFT is harness-specific: Claude/Codex can include the start of reasoning; OMP reports first content. Compare within a harness/model before interpreting cross-harness differences. Trend buckets use UTC boundaries; labels use local time.")
            }
            .font(.system(size: 11))
            .foregroundStyle(Theme.muted)
            .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func coverageRow(_ title: String, count: Int) -> some View {
        HStack {
            Text(title)
            Spacer()
            Text("\(count.formatted()) / \(summary.count.formatted()) calls").monospacedDigit().foregroundStyle(Color.primary)
        }
    }
}

private struct DashboardCallExplorer: View {
    @ObservedObject var dashboard: DashboardStore

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                VStack(alignment: .leading, spacing: 3) {
                    Text("Recorded calls").font(.system(size: 15, weight: .semibold))
                    Text("Select a row for evidence and timing details. Scroll horizontally for every column.")
                        .font(.system(size: 11)).foregroundStyle(Theme.muted)
                }
                Spacer()
                Picker("Sort calls", selection: $dashboard.callSort) {
                    ForEach(DashboardCallSort.allCases) { Text($0.title).tag($0) }
                }
                .frame(width: 190)
            }
            HStack(alignment: .top, spacing: 12) {
                callTable
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                DashboardCallInspector(record: dashboard.selectedCall) { dashboard.selectedCallID = nil }
                    .frame(width: 286)
                    .frame(maxHeight: .infinity)
            }
            HStack(spacing: 10) {
                Text(pageDescription).font(.system(size: 11)).foregroundStyle(Theme.muted)
                Spacer()
                Button { dashboard.page = max(0, dashboard.page - 1) } label: {
                    Label("Previous", systemImage: "chevron.left")
                }
                .keyboardShortcut("[", modifiers: .command)
                .disabled(dashboard.page <= 0)
                Text("Page \(dashboard.page + 1) of \(dashboard.pageCount)")
                    .font(.system(size: 11)).monospacedDigit()
                Button { dashboard.page = min(dashboard.pageCount - 1, dashboard.page + 1) } label: {
                    Label("Next", systemImage: "chevron.right")
                }
                .keyboardShortcut("]", modifiers: .command)
                .disabled(dashboard.page + 1 >= dashboard.pageCount)
            }
            Text("~ estimated tokens/speed · whole-request speed is explicitly labelled · reported log status is not verified HTTP success · missing fields are –")
                .font(.system(size: 10)).foregroundStyle(Theme.muted)
        }
    }

    private var callTable: some View {
        Table(dashboard.pageRecords, selection: $dashboard.selectedCallID) {
            TableColumn("Started") { record in
                VStack(alignment: .leading, spacing: 2) {
                    Text(record.startedAt, format: .dateTime.month(.abbreviated).day())
                    Text(record.startedAt, format: .dateTime.hour().minute().second()).foregroundStyle(Theme.muted)
                }
                .font(.system(size: 11)).monospacedDigit()
                .help(record.startedAt.formatted(date: .complete, time: .complete))
            }
            .width(min: 104, ideal: 112)
            TableColumn("Harness") { record in
                HStack(spacing: 5) {
                    HarnessDot(harness: record.harness, size: 6)
                    Text(record.harness).lineLimit(1)
                }
            }
            .width(min: 100, ideal: 120)
            TableColumn("Provider / endpoint") { record in
                let provider = ProviderIdentity(record: record)
                VStack(alignment: .leading, spacing: 2) {
                    Text(provider.displayName).lineLimit(1)
                    if provider.isUnverified { Text("Unverified").font(.system(size: 9)).foregroundStyle(Color.orange) }
                }
                .help(provider.evidence)
            }
            .width(min: 140, ideal: 160)
            TableColumn("Model") { record in
                Text(record.model.isEmpty ? "Unknown model" : record.model)
                    .lineLimit(1).truncationMode(.middle).help(record.model)
            }
            .width(min: 160, ideal: 200)
            TableColumn("TTFT") { record in
                Text(DashboardDisplay.duration(record.ttft)).monospacedDigit()
            }
            .width(min: 72, ideal: 80)
            TableColumn("Speed · tok/s") { record in
                VStack(alignment: .leading, spacing: 2) {
                    Text(DashboardDisplay.callRate(record)).monospacedDigit()
                    if DashboardDisplay.generationRate(record) == nil && DashboardDisplay.wholeRequestRate(record) != nil {
                        Text("whole request").font(.system(size: 9)).foregroundStyle(Theme.muted)
                    }
                }
            }
            .width(min: 96, ideal: 112)
            TableColumn("Output tokens") { record in
                Text((record.tokensEstimated ? "~" : "") + DashboardDisplay.tokens(record.outputTokens))
                    .monospacedDigit()
                    .help(record.tokensEstimated ? "Output token count is estimated" : "Recorded output token count")
            }
            .width(min: 88, ideal: 98)
            TableColumn("Total") { record in
                Text(DashboardDisplay.duration(record.total)).monospacedDigit()
            }
            .width(min: 72, ideal: 80)
            TableColumn("Source") { record in
                Text(DashboardDisplay.source(record.source)).foregroundStyle(Theme.muted)
            }
            .width(min: 78, ideal: 86)
            TableColumn("Status") { record in
                Text(DashboardDisplay.status(record))
                    .foregroundStyle(record.aborted || record.status >= 400 ? Color.orange : Color.secondary)
                    .help(DashboardDisplay.statusEvidence(record))
            }
            .width(min: 104, ideal: 120)
        }
        .font(.system(size: 11))
        .tableStyle(.inset)
        .background(Theme.panel, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Theme.border, lineWidth: 1).allowsHitTesting(false))
        .accessibilityLabel("Recorded calls, select a row to inspect details")
    }

    private var pageDescription: String {
        let start = dashboard.page * 25 + 1
        let end = min(dashboard.report.records.count, start + dashboard.pageRecords.count - 1)
        return "\(start.formatted())–\(end.formatted()) of \(dashboard.report.records.count.formatted()) calls · 25 per page"
    }
}

private struct DashboardCallInspector: View {
    let record: RequestRecord?
    let close: () -> Void

    var body: some View {
        Card(padding: 12) {
            if let record {
                ScrollView {
                    VStack(alignment: .leading, spacing: 13) {
                        inspectorHeader(record)
                        providerEvidence(record)
                        detailSection("Timing") {
                            detail("First token · TTFT", value: DashboardDisplay.duration(record.ttft))
                            detail("First visible token", value: DashboardDisplay.duration(record.firstVisible))
                            detail("Response headers · TTFB", value: DashboardDisplay.duration(record.ttfb))
                            detail("Generation window", value: DashboardDisplay.duration(record.generation))
                            detail("Request to end", value: DashboardDisplay.duration(record.total))
                        }
                        detailSection("Speed · tok/s") {
                            detail("Generation-window speed", value: estimatedRate(DashboardDisplay.generationRate(record), record: record))
                            detail("Whole-request speed", value: estimatedRate(DashboardDisplay.wholeRequestRate(record), record: record))
                            Text("Whole-request speed = output tokens ÷ total duration, including waiting. Only measured generation-window speed is included in dashboard TPS percentiles.")
                                .font(.system(size: 10)).foregroundStyle(Theme.muted)
                        }
                        detailSection("Recorded tokens") {
                            detail("Output", value: (record.tokensEstimated ? "~" : "") + DashboardDisplay.tokens(record.outputTokens))
                            detail("Input", value: DashboardDisplay.tokens(record.inputTokens))
                            detail("Cached input", value: DashboardDisplay.tokens(record.cachedInputTokens))
                            detail("Reasoning", value: DashboardDisplay.tokens(record.reasoningTokens))
                            Text(record.tokensEstimated ? "~ Token counts are estimated, not provider-reported usage." : "Token counts are recorded usage where available; absent fields are not zero.")
                                .font(.system(size: 10)).foregroundStyle(Theme.muted)
                        }
                        detailSection("Observation") {
                            detail("Source", value: DashboardDisplay.source(record.source))
                            detail("Status", value: DashboardDisplay.status(record))
                            detail("Response format", value: record.format.rawValue)
                            detail("Streamed", value: record.streamed ? "Yes" : "No")
                            Text(DashboardDisplay.statusEvidence(record)).font(.system(size: 10)).foregroundStyle(Theme.muted)
                        }
                        Text("This inspector shows telemetry only. Prompts, replies, endpoint credentials and configuration are not displayed.")
                            .font(.system(size: 10)).foregroundStyle(Theme.muted)
                    }
                    .textSelection(.enabled)
                    .padding(.bottom, 4)
                }
            } else {
                VStack(alignment: .leading, spacing: 10) {
                    Image(systemName: "sidebar.right").font(.system(size: 24)).foregroundStyle(Theme.muted)
                    Text("Call inspector").font(.system(size: 15, weight: .semibold))
                    Text("Select a recorded call to inspect endpoint evidence, timing, token usage and observation status. Use the table’s arrow keys to change selection.")
                        .font(.system(size: 11)).foregroundStyle(Theme.muted)
                    Spacer(minLength: 0)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            }
        }
    }

    private func inspectorHeader(_ record: RequestRecord) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                SectionLabel("Call inspector")
                Spacer()
                Button(action: close) { Image(systemName: "xmark") }
                    .buttonStyle(.borderless)
                    .keyboardShortcut(.escape, modifiers: [])
                    .accessibilityLabel("Close call details")
            }
            Text(record.model.isEmpty ? "Unknown model" : record.model)
                .font(.system(size: 14, weight: .semibold))
                .fixedSize(horizontal: false, vertical: true)
            HarnessChip(name: record.harness)
            Text(record.startedAt.formatted(date: .abbreviated, time: .standard))
                .font(.system(size: 10)).foregroundStyle(Theme.muted)
        }
    }

    private func providerEvidence(_ record: RequestRecord) -> some View {
        let provider = ProviderIdentity(record: record)
        return VStack(alignment: .leading, spacing: 5) {
            SectionLabel("Provider / endpoint evidence")
            Text(provider.displayName).font(.system(size: 12, weight: .semibold))
            if let host = provider.host {
                Text(host).font(.system(size: 10, design: .monospaced)).foregroundStyle(Theme.muted)
            }
            if provider.isUnverified {
                Label("Endpoint unverified", systemImage: "exclamationmark.shield")
                    .font(.system(size: 10, weight: .medium)).foregroundStyle(Color.orange)
            }
            Text(provider.evidence).font(.system(size: 10)).foregroundStyle(Theme.muted)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func detailSection<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            Divider()
            SectionLabel(title)
            content()
        }
    }

    private func detail(_ title: String, value: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(title).foregroundStyle(Theme.muted)
            Spacer(minLength: 0)
            Text(value).monospacedDigit().multilineTextAlignment(.trailing)
        }
        .font(.system(size: 10))
        .accessibilityElement(children: .combine)
    }

    private func estimatedRate(_ value: Double?, record: RequestRecord) -> String {
        guard let value else { return "–" }
        return (record.tokensEstimated ? "~" : "") + Format.rate(value)
    }
}

struct DashboardEmptyState: View {
    let symbol: String
    let title: String
    let message: String
    var loading = false

    var body: some View {
        VStack(spacing: 12) {
            if loading { ProgressView().controlSize(.large) }
            else { Image(systemName: symbol).font(.system(size: 30, weight: .light)).foregroundStyle(Theme.muted) }
            Text(title).font(.system(size: 16, weight: .semibold))
            Text(message)
                .font(.system(size: 12)).foregroundStyle(Theme.muted)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 470)
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .accessibilityElement(children: .combine)
    }
}

enum DashboardDisplay {
    static func duration(_ value: Double?) -> String {
        guard let value, value.isFinite, value >= 0 else { return "–" }
        return Format.durationText(value)
    }

    static func tokens(_ value: Int?) -> String {
        guard let value, value >= 0 else { return "–" }
        return value.formatted()
    }

    static func generationRate(_ record: RequestRecord) -> Double? {
        guard let generation = record.generation, generation.isFinite, generation > 0,
              let rate = record.tps, rate.isFinite, rate >= 0 else { return nil }
        return rate
    }

    static func wholeRequestRate(_ record: RequestRecord) -> Double? {
        guard record.total.isFinite, record.total > 0, record.outputTokens >= 0 else { return nil }
        let rate = Double(record.outputTokens) / record.total
        return rate.isFinite ? rate : nil
    }

    static func callRate(_ record: RequestRecord) -> String {
        guard let value = generationRate(record) ?? wholeRequestRate(record) else { return "–" }
        return (record.tokensEstimated ? "~" : "") + Format.rate(value)
    }

    static func summaryRate(_ value: Double?, summary: DashboardSummary) -> String {
        guard let value else { return "–" }
        return (summary.estimatedCount > 0 ? "~" : "") + Format.rate(value)
    }

    static func metric(_ metric: DashboardMetric, summary: DashboardSummary) -> String {
        switch metric {
        case .speed:
            let value = summaryRate(summary.medianTPS, summary: summary)
            return value == "–" ? value : value + " tok/s"
        case .latency: return duration(summary.medianTTFT)
        case .calls: return summary.count.formatted()
        case .tokens: return (summary.estimatedCount > 0 ? "~" : "") + Format.tokens(summary.outputTokens)
        }
    }

    static func source(_ source: String?) -> String {
        switch source {
        case "log": return "Session log"
        case "network": return "Network"
        case "proxy": return "Proxy"
        default: return "Unknown"
        }
    }

    static func status(_ record: RequestRecord) -> String {
        if record.aborted { return "Interrupted" }
        if record.status <= 0 { return "Not recorded" }
        return "\(record.source == "proxy" ? "HTTP" : "Reported") \(record.status)"
    }

    static func statusEvidence(_ record: RequestRecord) -> String {
        var text = record.source == "proxy"
            ? "The proxy recorded HTTP status \(record.status > 0 ? String(record.status) : "unavailable"). An HTTP response alone does not prove a useful model answer."
            : "Raw reported status: \(record.status > 0 ? String(record.status) : "unavailable"). A log/network/default status is not independent evidence of HTTP success."
        if record.aborted { text += " The call was interrupted before completion." }
        return text
    }
}
