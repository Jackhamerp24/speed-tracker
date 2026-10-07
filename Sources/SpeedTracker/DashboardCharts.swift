import Charts
import Foundation
import SpeedTrackerCore
import SwiftUI

struct DashboardTrendChart: View {
    let trends: [DashboardTrend]
    let summary: DashboardSummary
    let metric: DashboardMetric
    // This package builds with the Command Line Tools: do not use the @State macro.
    private let selectedDate = State<Date?>(initialValue: nil)

    var body: some View {
        Card(padding: 14) {
            VStack(alignment: .leading, spacing: 10) {
                header
                if hasMeasurements {
                    chart.frame(height: 210)
                    selectionCaption
                        .font(.system(size: 10))
                        .foregroundStyle(Theme.muted)
                        .frame(height: 30, alignment: .topLeading)
                } else {
                    DashboardEmptyState(symbol: "waveform.path", title: missingTitle, message: missingMessage)
                        .frame(height: 250)
                }
            }
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(metric == .latency ? "Time to first token" : "Generation speed")
                    .font(.system(size: 14, weight: .semibold))
                Spacer()
                Text("\(sampleCount) measured calls")
                    .font(.system(size: 10)).foregroundStyle(Theme.muted)
            }
            HStack(spacing: 12) {
                HStack(spacing: 5) {
                    Circle().fill(color).frame(width: 6, height: 6)
                    Text("p50 · median")
                }
                HStack(spacing: 5) {
                    Rectangle().fill(Color.orange).frame(width: 12, height: 2)
                    Text("p95 · 95th percentile")
                }
                Spacer()
                Text(metric == .latency ? "Lower is faster" : "tok/s · generation only")
            }
            .font(.system(size: 10))
            .foregroundStyle(Theme.muted)
        }
    }

    private var chart: some View {
        Chart {
            ForEach(trends) { trend in
                if let value = median(trend.summary) {
                    LineMark(x: .value("Date", trend.date), y: .value(axisTitle, value))
                        .foregroundStyle(by: .value("Percentile", "p50"))
                        .lineStyle(StrokeStyle(lineWidth: 2))
                        .interpolationMethod(.linear)
                    PointMark(x: .value("Date", trend.date), y: .value(axisTitle, value))
                        .foregroundStyle(by: .value("Percentile", "p50"))
                        .symbol(.circle)
                        .symbolSize(22)
                }
                if let value = percentile(trend.summary) {
                    LineMark(x: .value("Date", trend.date), y: .value(axisTitle, value))
                        .foregroundStyle(by: .value("Percentile", "p95"))
                        .lineStyle(StrokeStyle(lineWidth: 1.7, dash: [5, 4]))
                        .interpolationMethod(.linear)
                    PointMark(x: .value("Date", trend.date), y: .value(axisTitle, value))
                        .foregroundStyle(by: .value("Percentile", "p95"))
                        .symbol(.diamond)
                        .symbolSize(18)
                }
            }
            if let selected = selectedTrend {
                RuleMark(x: .value("Selected date", selected.date))
                    .foregroundStyle(Color.secondary.opacity(0.5))
                    .lineStyle(StrokeStyle(lineWidth: 1, dash: [2, 3]))
            }
        }
        .chartForegroundStyleScale(domain: ["p50", "p95"], range: [color, Color.orange])
        .chartLegend(.hidden)
        .chartYScale(domain: .automatic(includesZero: true))
        .chartXAxis {
            AxisMarks(values: .automatic(desiredCount: 5)) { value in
                AxisGridLine().foregroundStyle(Theme.divider)
                AxisTick()
                AxisValueLabel(anchor: .top) {
                    if let date = value.as(Date.self) {
                        Text(date, format: isHourly ? .dateTime.hour().minute() : .dateTime.month(.abbreviated).day())
                            .font(.system(size: 9))
                            .foregroundStyle(Color.secondary)
                    }
                }
            }
        }
        .chartYAxis {
            AxisMarks(position: .leading, values: .automatic(desiredCount: 4)) { value in
                AxisGridLine().foregroundStyle(Theme.divider)
                AxisValueLabel(anchor: .trailing) {
                    if let value = value.as(Double.self) {
                        Text(axisValue(value)).font(.system(size: 9)).monospacedDigit()
                            .foregroundStyle(Color.secondary)
                    }
                }
            }
        }
        .chartXSelection(value: selectedDate.projectedValue)
        .accessibilityLabel(metric == .latency ? "First-token latency by date, median and 95th percentile" : "Generation speed by date, median and 95th percentile")
        .accessibilityValue("\(sampleCount) measured calls across \(trends.count) chronological buckets. \(metric == .latency ? "Lower latency is faster." : "Speeds exclude whole-request timing.")")
    }

    @ViewBuilder private var selectionCaption: some View {
        if let selected = selectedTrend {
            let estimated = metric == .speed && selected.summary.estimatedCount > 0 ? "~" : ""
            Text("\(selected.date.formatted(date: .abbreviated, time: isHourly ? .shortened : .omitted)) · p50 \(estimated + formatted(median(selected.summary))) · p95 \(estimated + formatted(percentile(selected.summary))) · n = \(metric == .latency ? selected.summary.latencyCount : selected.summary.speedCount)")
                .monospacedDigit()
        } else {
            Text(metric == .speed && summary.estimatedCount > 0 ? "Select a date to inspect p50/p95. ~ Token estimates are present in this view; whole-request TPS is excluded." : "Select a date to inspect p50/p95. Missing measurements are omitted, not filled with zero.")
        }
    }

    private var selectedTrend: DashboardTrend? {
        guard let date = selectedDate.wrappedValue else { return nil }
        return trends.filter { median($0.summary) != nil || percentile($0.summary) != nil }
            .min { abs($0.date.timeIntervalSince(date)) < abs($1.date.timeIntervalSince(date)) }
    }

    private var hasMeasurements: Bool { trends.contains { median($0.summary) != nil || percentile($0.summary) != nil } }
    private var color: Color { metric == .latency ? .blue : Theme.accent }
    private var axisTitle: String { metric == .latency ? "TTFT (seconds)" : "Generation speed (tokens/second)" }
    private var sampleCount: Int { metric == .latency ? summary.latencyCount : summary.speedCount }
    private var isHourly: Bool {
        guard let first = trends.first, let last = trends.last else { return true }
        return last.date.timeIntervalSince(first.date) < 172_800
    }
    private var missingTitle: String { metric == .latency ? "No measured first-token latency" : "No generation-window speed" }
    private var missingMessage: String {
        metric == .latency
            ? "These calls do not contain eligible TTFT measurements. Session logs can report usage without first-token timing; missing latency is not zero."
            : "A positive generation window and a recorded speed are required. Whole-request timings, interrupted calls and known errors are not pooled into generation speed."
    }

    private func median(_ summary: DashboardSummary) -> Double? {
        metric == .latency ? summary.medianTTFT : summary.medianTPS
    }

    private func percentile(_ summary: DashboardSummary) -> Double? {
        metric == .latency ? summary.p95TTFT : summary.p95TPS
    }

    private func axisValue(_ value: Double) -> String {
        metric == .latency ? Format.durationText(value) : Format.rate(value)
    }

    private func formatted(_ value: Double?) -> String {
        metric == .latency ? DashboardDisplay.duration(value) : value.map { Format.rate($0) + " tok/s" } ?? "–"
    }
}

struct DashboardDistributionChart: View {
    let bins: [DashboardBin]
    let summary: DashboardSummary
    let metric: DashboardMetric

    var body: some View {
        Card(padding: 14) {
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    Text(metric == .latency ? "TTFT distribution" : "Generation-speed distribution")
                        .font(.system(size: 14, weight: .semibold))
                    Spacer()
                    Text("\(count) samples")
                        .font(.system(size: 10)).foregroundStyle(Theme.muted)
                }
                if bins.isEmpty {
                    DashboardEmptyState(symbol: "chart.bar", title: "No distribution to display", message: metric == .latency ? "First-token latency was not measured for eligible calls in this view." : "No eligible generation-window TPS measurements exist in this view.")
                        .frame(height: 190)
                } else {
                    chart.frame(height: 155)
                    Text(caption)
                        .font(.system(size: 10)).foregroundStyle(Theme.muted)
                        .frame(height: 25, alignment: .topLeading)
                }
            }
        }
    }

    private var chart: some View {
        Chart(bins) { bin in
            RectangleMark(
                xStart: .value(axisTitle, bin.lower),
                xEnd: .value(axisTitle, bin.upper),
                yStart: .value("Measured calls", 0),
                yEnd: .value("Measured calls", bin.count)
            )
            .foregroundStyle((metric == .latency ? Color.blue : Theme.accent).opacity(0.72))
            .cornerRadius(2)
            .accessibilityLabel("\(formatted(bin.lower)) to \(formatted(bin.upper))")
            .accessibilityValue("\(bin.count) measured calls")
        }
        .chartYScale(domain: .automatic(includesZero: true))
        .chartXAxis {
            AxisMarks(values: .automatic(desiredCount: 5)) { value in
                AxisTick()
                AxisValueLabel(anchor: .top) {
                    if let number = value.as(Double.self) {
                        Text(formatted(number)).font(.system(size: 9)).monospacedDigit()
                            .foregroundStyle(Color.secondary)
                    }
                }
            }
        }
        .chartYAxis {
            AxisMarks(position: .leading, values: .automatic(desiredCount: 4)) { value in
                AxisGridLine().foregroundStyle(Theme.divider)
                AxisValueLabel(anchor: .trailing) {
                    if let number = value.as(Double.self) {
                        Text(number.formatted(.number.precision(.fractionLength(0))))
                            .font(.system(size: 9)).monospacedDigit()
                            .foregroundStyle(Color.secondary)
                    }
                }
            }
        }
        .chartYAxisLabel(position: .leading) { Text("Calls").foregroundStyle(Color.secondary) }
        .accessibilityLabel(metric == .latency ? "Distribution of first-token latency" : "Distribution of generation-window speed")
    }

    private var count: Int { bins.reduce(0) { $0 + $1.count } }
    private var axisTitle: String { metric == .latency ? "TTFT (seconds)" : "Generation speed (tokens/second)" }
    private var caption: String {
        if count < 5 { return "Low sample: fewer than five eligible calls; this distribution is not a reliable comparison." }
        if metric == .speed && summary.estimatedCount > 0 { return "tok/s over the generation window; this view includes estimated token counts (~)." }
        return metric == .latency ? "Latency bands in request-to-first-token time; missing TTFT is excluded." : "tok/s over the generation window; whole-request speeds are excluded."
    }

    private func formatted(_ value: Double) -> String {
        metric == .latency ? Format.durationText(value) : Format.rate(value)
    }
}
