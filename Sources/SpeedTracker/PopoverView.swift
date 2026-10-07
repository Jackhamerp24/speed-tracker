import SpeedTrackerCore
import SwiftUI

enum PopoverTab: String, CaseIterable, Identifiable {
    case live = "Live"
    case models = "Models"
    case setup = "Settings"

    var id: String { rawValue }
}

struct PopoverView: View {
    @EnvironmentObject private var store: MetricsStore
    // Spelled out because the @State macro plugin ships only with Xcode.
    private let tabState: State<PopoverTab>
    private let openDashboard: () -> Void
    /// Opens the Live tab with its harness list showing. Only snapshots ask for this.
    private let targetListOpen: Bool

    init(tab: PopoverTab = .live, targetListOpen: Bool = false, openDashboard: @escaping () -> Void = {}) {
        tabState = State(initialValue: tab)
        self.targetListOpen = targetListOpen
        self.openDashboard = openDashboard
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            header
            Picker("", selection: tabState.projectedValue) {
                ForEach(PopoverTab.allCases) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented)
            .controlSize(.small)
            .labelsHidden()

            switch tabState.wrappedValue {
            case .live: LiveTab(targetListOpen: targetListOpen)
            case .models: ModelsTab()
            case .setup: SettingsTab()
            }
            Button(action: openDashboard) {
                HStack(spacing: 7) {
                    Image(systemName: "chart.xyaxis.line")
                        .font(.system(size: 11, weight: .medium))
                    Text("Open Dashboard")
                        .font(.system(size: 11, weight: .medium))
                    Spacer()
                    Text("⌘D")
                        .font(.system(size: 9.5, weight: .medium, design: .monospaced))
                        .foregroundStyle(Theme.muted)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1.5)
                        .background(Theme.panelRaised, in: RoundedRectangle(cornerRadius: 4, style: .continuous))
                        .overlay(RoundedRectangle(cornerRadius: 4, style: .continuous).strokeBorder(Theme.border, lineWidth: 1))
                    Image(systemName: "arrow.up.right")
                        .font(.system(size: 9, weight: .semibold))
                        .foregroundStyle(Theme.muted)
                }
                .padding(.horizontal, 10)
                .padding(.vertical, 7)
                .background(Theme.panel, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(Theme.border, lineWidth: 1))
            }
            .buttonStyle(.plain)
            .keyboardShortcut("d", modifiers: [.command])
            .help("Inspect history by provider, harness and model (⌘D)")
        }
        .padding(Layout.padding)
        .frame(width: Layout.width)
        .onReceive(NotificationCenter.default.publisher(for: .probeSwitchTab)) { note in
            if let name = note.object as? String, let tab = PopoverTab(rawValue: name) { tabState.wrappedValue = tab }
        }
    }

    private var header: some View {
        HStack(spacing: 9) {
            Image(systemName: "bolt.fill")
                .font(.system(size: 11, weight: .bold))
                .foregroundStyle(Theme.accent)
                .frame(width: 24, height: 24)
                .background(
                    LinearGradient(
                        colors: [Theme.accent.opacity(0.24), Theme.accent.opacity(0.08)],
                        startPoint: .topLeading, endPoint: .bottomTrailing
                    ),
                    in: RoundedRectangle(cornerRadius: 7, style: .continuous)
                )
                .overlay(RoundedRectangle(cornerRadius: 7, style: .continuous).strokeBorder(Theme.accent.opacity(0.28), lineWidth: 1))
            VStack(alignment: .leading, spacing: 1) {
                Text("Speed Tracker")
                    .font(.system(size: 13, weight: .semibold))
                Text("Model response telemetry")
                    .font(.system(size: 10, weight: .medium))
                    .foregroundStyle(Theme.muted)
            }
            Spacer()
            ActivityPill(generating: store.generatingHarnesses)
        }
    }
}

/// Says who is generating right now. An open but idle harness does not count.
private struct ActivityPill: View {
    let generating: [String]
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(spacing: 5) {
            if generating.isEmpty {
                Circle().fill(Color.secondary.opacity(0.45)).frame(width: 6, height: 6)
            } else {
                if reduceMotion {
                    Circle().fill(Theme.accent).frame(width: 6, height: 6)
                } else {
                    Image(systemName: "circle.fill")
                        .font(.system(size: 6))
                        .foregroundStyle(Theme.accent)
                        .symbolEffect(.pulse, options: .repeating)
                }
            }
            Text(generating.isEmpty ? "Idle" : generating.joined(separator: ", "))
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(generating.isEmpty ? Theme.muted : .primary)
                .lineLimit(1)
                .truncationMode(.tail)
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 4)
        .background(generating.isEmpty ? Theme.panel : Theme.accentSoft, in: Capsule())
        .overlay(Capsule().strokeBorder(generating.isEmpty ? Theme.border : Theme.accent.opacity(0.35), lineWidth: 1))
        .frame(maxWidth: 200, alignment: .trailing)
        .help(generating.isEmpty ? "No model call in progress" : "Model call in progress")
    }
}

// MARK: - Live

private struct LiveTab: View {
    @EnvironmentObject private var store: MetricsStore
    // Spelled out because the @State macro plugin ships only with Xcode.
    private let targetListOpen: State<Bool>
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    /// Room for the harness list even when there is nothing else to show yet.
    private static let minimumHeight: CGFloat = 290

    init(targetListOpen: Bool = false) {
        self.targetListOpen = State(initialValue: targetListOpen)
    }

    var body: some View {
        let recent = store.recent
        let isOpen = targetListOpen.wrappedValue
        VStack(alignment: .leading, spacing: 10) {
            TargetHeader(target: store.target, harnesses: store.harnesses, isOpen: isOpen) { setOpen(!isOpen) }
            if let primary = store.primary {
                LiveHero(request: primary, now: store.now, heldRate: store.displayRate)
                // Only calls that are producing text; a harness's side requests are not worth a row.
                let others = store.active.filter { $0.id != primary.id && $0.smoothedRate != nil }
                if !others.isEmpty {
                    Card(padding: 0) {
                        VStack(spacing: 0) {
                            ForEach(others) { OtherActiveRow(request: $0) }
                        }
                    }
                }
            } else if let last = store.lastSignificant {
                LastHero(record: last)
            } else {
                EmptyHero(target: store.target)
            }

            if !recent.isEmpty {
                StatsRow(stats: store.periodStats)
                SectionLabel("Recent calls")
                    .padding(.top, 2)
                Card(padding: 0) {
                    VStack(spacing: 0) {
                        let rows = Array(recent.prefix(4))
                        ForEach(rows) { record in
                            CallRow(record: record)
                            if record.id != rows.last?.id { RowDivider() }
                        }
                    }
                }
            }
        }
        .frame(minHeight: Self.minimumHeight, alignment: .top)
        // The list floats over the tab instead of pushing it down. Growing the popover made
        // it taller than a laptop screen, and AppKit then threw it to the edge of the display
        // (measured: window x 706 -> 0). An overlay is sized to the tab, so nothing resizes.
        .overlay(alignment: .top) {
            if isOpen {
                ZStack(alignment: .top) {
                    // A click anywhere else closes the list.
                    Color.clear
                        .contentShape(Rectangle())
                        .onTapGesture { setOpen(false) }
                        .accessibilityHidden(true)
                    TargetList(target: store.target, harnesses: store.harnesses) { harness in
                        store.setTarget(harness)
                        setOpen(false)
                    }
                    .padding(.top, TargetHeader.height + 5)
                }
                .transition(.opacity)
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: .probeToggleTargetList)) { _ in setOpen(!targetListOpen.wrappedValue) }
    }

    private func setOpen(_ open: Bool) {
        if reduceMotion {
            targetListOpen.wrappedValue = open
        } else {
            withAnimation(.snappy(duration: 0.16)) { targetListOpen.wrappedValue = open }
        }
    }
}

/// The row that names what the Live tab and menu bar are watching: Auto, which follows
/// whichever harness is active, or one pinned harness. Clicking it opens the list.
private struct TargetHeader: View {
    let target: String?
    let harnesses: [HarnessInfo]
    let isOpen: Bool
    let toggle: () -> Void

    static let height: CGFloat = 36

    var body: some View {
        Button(action: toggle) {
            HStack(spacing: 6) {
                Image(systemName: "scope")
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(Theme.accent)
                Text("WATCHING")
                    .font(.system(size: 10, weight: .bold, design: .rounded))
                    .kerning(0.6)
                    .foregroundStyle(Theme.muted)
                Spacer(minLength: 8)
                TargetMark(harness: target, isGenerating: harnesses.first { $0.name == target }?.isGenerating ?? false)
                Text(target ?? "Auto")
                    .font(.system(size: 12, weight: .semibold))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Image(systemName: "chevron.down")
                    .font(.system(size: 9, weight: .bold))
                    .foregroundStyle(Theme.muted)
                    .rotationEffect(.degrees(isOpen ? 180 : 0))
            }
            .padding(.horizontal, 10)
            .frame(height: Self.height)
            .background(Theme.panel, in: RoundedRectangle(cornerRadius: 11, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 11, style: .continuous)
                    .strokeBorder(isOpen ? Theme.accent.opacity(0.55) : Theme.border, lineWidth: 1)
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Watching \(target ?? "Auto, every harness")")
        .accessibilityHint(isOpen ? "Closes the list of harnesses" : "Opens the list of harnesses to choose from")
        .help("Choose which harness the Live tab and menu bar follow")
    }
}

/// The harnesses to choose from, as a card floating under the header. It takes the
/// height its rows need, up to the space the tab has, and scrolls beyond that.
private struct TargetList: View {
    let target: String?
    let harnesses: [HarnessInfo]
    let choose: (String?) -> Void

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: 11, style: .continuous)
        ScrollView {
            VStack(spacing: 0) {
                TargetRow(
                    harness: nil, title: "Auto", detail: "Follow whichever harness is active",
                    isSelected: target == nil, isGenerating: false
                ) { choose(nil) }
                ForEach(harnesses) { harness in
                    TargetRow(
                        harness: harness.name, title: harness.name, detail: Self.detail(harness),
                        isSelected: target == harness.name, isGenerating: harness.isGenerating
                    ) { choose(harness.name) }
                }
            }
            .padding(.vertical, 3)
        }
        .frame(maxHeight: CGFloat(harnesses.count + 1) * TargetRow.height + 6)
        // Opaque, so the tab underneath does not show through the rows.
        .background(Color(nsColor: .windowBackgroundColor), in: shape)
        .background(Theme.panel, in: shape)
        .overlay(shape.strokeBorder(Theme.border, lineWidth: 1))
        .clipShape(shape)
        .shadow(color: .black.opacity(0.28), radius: 14, y: 6)
    }

    private static func detail(_ harness: HarnessInfo) -> String {
        if harness.isGenerating { return "Generating now" }
        if !harness.isPresent { return "Not found on this Mac" }
        var parts: [String] = []
        if harness.isOpen { parts.append("Open, idle") }
        parts.append(harness.lastCall.map { "last call \(Format.ago($0)) ago" } ?? "no calls yet")
        return parts.joined(separator: " · ")
    }
}

/// The dot for a harness, pulsing while it generates, or the Auto symbol.
private struct TargetMark: View {
    let harness: String?
    let isGenerating: Bool
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        Group {
            if let harness {
                let color = HarnessStyle.color(for: harness)
                if isGenerating, !reduceMotion {
                    Image(systemName: "circle.fill")
                        .font(.system(size: 7))
                        .foregroundStyle(color)
                        .symbolEffect(.pulse, options: .repeating)
                } else {
                    Circle().fill(color).frame(width: 7, height: 7)
                }
            } else {
                Image(systemName: "sparkles")
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(Theme.accent)
            }
        }
        .frame(width: 14)
        .accessibilityHidden(true)
    }
}

private struct TargetRow: View {
    let harness: String?
    let title: String
    let detail: String
    let isSelected: Bool
    let isGenerating: Bool
    let action: () -> Void
    private let isHovered = State(initialValue: false)

    static let height: CGFloat = 40

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                TargetMark(harness: harness, isGenerating: isGenerating)
                VStack(alignment: .leading, spacing: 1) {
                    Text(title)
                        .font(.system(size: 12, weight: isSelected ? .semibold : .medium))
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Text(detail)
                        .font(.system(size: 10.5))
                        .foregroundStyle(isGenerating ? Theme.accent : Theme.muted)
                        .lineLimit(1)
                }
                Spacer(minLength: 8)
                if isSelected {
                    Image(systemName: "checkmark")
                        .font(.system(size: 10, weight: .bold))
                        .foregroundStyle(Theme.accent)
                }
            }
            .padding(.horizontal, 10)
            .frame(height: Self.height)
            .background(
                RoundedRectangle(cornerRadius: 7, style: .continuous)
                    .fill(isHovered.wrappedValue ? Theme.panelRaised : .clear)
                    .padding(.horizontal, 3)
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { isHovered.wrappedValue = $0 }
        .accessibilityLabel(harness == nil ? "Watch every harness" : "Watch \(title)")
        .accessibilityValue(detail)
        .accessibilityAddTraits(isSelected ? .isSelected : [])
    }
}

private struct RowDivider: View {
    var body: some View {
        Rectangle()
            .fill(Theme.divider)
            .frame(height: 1)
            .padding(.leading, 30)
    }
}

private struct PhasePill: View {
    let phase: LiveSnapshot.Phase
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(spacing: 5) {
            if reduceMotion {
                Circle().fill(color).frame(width: 6, height: 6)
            } else {
                Image(systemName: "circle.fill")
                    .font(.system(size: 6))
                    .symbolEffect(.pulse, options: .repeating)
            }
            Text(label)
                .font(.system(size: 11, weight: .semibold))
        }
        .foregroundStyle(color)
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(color.opacity(0.14), in: Capsule())
        .overlay(Capsule().strokeBorder(color.opacity(0.28), lineWidth: 1))
    }

    private var label: String {
        switch phase {
        case .waiting: return "Waiting for first token"
        case .thinking: return "Thinking"
        case .streaming: return "Streaming"
        }
    }

    private var color: Color {
        switch phase {
        case .waiting: return .orange
        case .thinking: return .purple
        case .streaming: return Theme.accent
        }
    }
}

private struct LiveHero: View {
    let request: LiveRequest
    let now: TimeInterval
    /// The speed last shown, carried over until this call has one of its own.
    let heldRate: Double?

    var body: some View {
        let snapshot = request.snapshot
        let elapsed = max(0, now - snapshot.startUptime)
        Card(padding: 14) {
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    PhasePill(phase: snapshot.phase)
                    Spacer()
                    HarnessChip(name: snapshot.harness)
                }

                VStack(alignment: .leading, spacing: 2) {
                    SectionLabel("Current response")
                    Text(snapshot.model)
                        .font(.system(size: 15, weight: .semibold))
                        .lineLimit(1)
                        .truncationMode(.middle)
                }

                HStack(alignment: .top, spacing: 14) {
                    if let rate = request.smoothedRate {
                        Metric(label: "SPEED", value: "~" + Format.wholeRate(rate), unit: "tok/s")
                    } else {
                        // Nothing measured for this call yet: keep the last speed in view, dimmed.
                        Metric(label: "SPEED", value: heldRate.map(Format.wholeRate) ?? "–", unit: "tok/s", dimmed: true)
                    }
                    if let ttft = snapshot.ttft {
                        let parts = Format.duration(ttft)
                        Metric(label: "TTFT", value: parts.value, unit: parts.unit)
                    } else {
                        Metric(label: "TTFT SO FAR", value: String(format: "%.1f", elapsed), unit: "s")
                    }
                }

                Sparkline(values: request.rateHistory, color: HarnessStyle.color(for: snapshot.harness))
                    .frame(height: 46)

                HStack {
                    HStack(spacing: 4) {
                        Text("~\(Format.tokens(Int(snapshot.estimatedTokens.rounded()))) tokens")
                        Text("·")
                            .foregroundStyle(Theme.muted.opacity(0.6))
                        Text(String(format: "%.1fs elapsed", elapsed))
                    }
                    Spacer()
                    if request.peakRate > 0 {
                        HStack(spacing: 3) {
                            Image(systemName: "arrow.up.right")
                                .font(.system(size: 9, weight: .bold))
                                .foregroundStyle(Theme.accent)
                            Text("peak \(Format.wholeRate(request.peakRate)) tok/s")
                        }
                    }
                }
                .font(.system(size: 11))
                .monospacedDigit()
                .foregroundStyle(Theme.muted)
            }
        }
    }
}

private struct LastHero: View {
    let record: RequestRecord

    var body: some View {
        Card(padding: 14) {
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    HStack(spacing: 5) {
                        Image(systemName: "clock.arrow.circlepath")
                            .font(.system(size: 10, weight: .semibold))
                            .foregroundStyle(Theme.muted)
                        SectionLabel("Last call · \(Format.ago(record.startedAt)) ago")
                    }
                    Spacer()
                    HarnessChip(name: record.harness)
                }
                Text(record.model)
                    .font(.system(size: 15, weight: .semibold))
                    .lineLimit(1)
                    .truncationMode(.middle)

                HStack(alignment: .top, spacing: 14) {
                    Metric(
                        label: "SPEED", value: (record.tokensEstimated ? "~" : "") + Format.rate(record.tps), unit: "tok/s"
                    )
                    if let ttft = record.ttft {
                        let parts = Format.duration(ttft)
                        Metric(label: "TTFT", value: parts.value, unit: parts.unit)
                    } else {
                        Metric(label: "TTFT", value: "–", unit: "", dimmed: true)
                    }
                }

                Text(detail)
                    .font(.system(size: 11))
                    .monospacedDigit()
                    .foregroundStyle(Theme.muted)
                    .lineLimit(2)
            }
        }
    }

    private var detail: String {
        var parts = ["\(Format.tokens(record.outputTokens)) tokens out"]
        if let input = record.inputTokens { parts.append("\(Format.tokens(input)) in") }
        parts.append("\(Format.durationText(record.total)) total")
        if record.aborted { parts.append("interrupted") }
        return parts.joined(separator: " · ")
    }
}

private struct EmptyHero: View {
    let target: String?

    var body: some View {
        Card(padding: 16) {
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 8) {
                    Image(systemName: "scope")
                        .font(.system(size: 15, weight: .semibold))
                        .foregroundStyle(Theme.accent)
                    Text(target.map { "No calls from \($0) yet" } ?? "No calls yet")
                        .font(.system(size: 14, weight: .semibold))
                }
                Text(target == nil
                    ? "Use your coding harness as usual. Its time to first token and tokens per second appear here on their own."
                    : "Its time to first token and tokens per second appear here when it next calls a model. Choose Auto to watch every harness.")
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.muted)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

private struct StatsRow: View {
    let stats: PeriodStats

    var body: some View {
        HStack(spacing: 8) {
            StatTile(
                label: stats.label == "Today" ? "Calls today" : "Recent calls",
                icon: "number",
                value: "\(stats.calls)",
                unit: ""
            )
            if let ttft = stats.medianTTFT {
                let parts = Format.duration(ttft)
                StatTile(label: "Median TTFT", icon: "timer", value: parts.value, unit: parts.unit)
            } else {
                StatTile(label: "Median TTFT", icon: "timer", value: "–", unit: "")
            }
            StatTile(label: "Median speed", icon: "bolt", value: Format.rate(stats.medianTPS), unit: "tok/s")
        }
    }
}

private struct StatTile: View {
    let label: String
    var icon: String? = nil
    let value: String
    let unit: String

    var body: some View {
        Card(padding: 9) {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 4) {
                    if let icon {
                        Image(systemName: icon)
                            .font(.system(size: 9, weight: .medium))
                            .foregroundStyle(Theme.muted)
                    }
                    Text(label)
                        .font(.system(size: 10, weight: .medium))
                        .foregroundStyle(Theme.muted)
                        .lineLimit(1)
                }
                HStack(alignment: .firstTextBaseline, spacing: 2) {
                    Text(value)
                        .font(.system(size: 16, weight: .semibold, design: .rounded))
                        .monospacedDigit()
                    if !unit.isEmpty {
                        Text(unit)
                            .font(.system(size: 10, weight: .medium))
                            .foregroundStyle(Theme.muted)
                    }
                }
                .lineLimit(1)
            }
        }
    }
}

/// Speed and TTFT, right-aligned, shared by the call and model rows.
private struct RowFigures: View {
    let rate: String
    let ttft: Double?

    var body: some View {
        VStack(alignment: .trailing, spacing: 1) {
            HStack(alignment: .firstTextBaseline, spacing: 3) {
                Text(rate)
                    .font(.system(size: 13, weight: .semibold, design: .rounded))
                Text("tok/s")
                    .font(.system(size: 10))
                    .foregroundStyle(Theme.muted)
            }
            Text(ttft.map { "TTFT \(Format.durationText($0))" } ?? "TTFT –")
                .font(.system(size: 10.5))
                .foregroundStyle(Theme.muted)
        }
        .monospacedDigit()
        .fixedSize()
    }
}

private struct CallRow: View {
    let record: RequestRecord

    var body: some View {
        HStack(spacing: 10) {
            HarnessDot(harness: record.harness, size: 7)
            VStack(alignment: .leading, spacing: 1.5) {
                Text(record.model)
                    .font(.system(size: 12, weight: .medium))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text("\(record.harness) · \(Format.ago(record.startedAt))")
                    .font(.system(size: 10.5))
                    .foregroundStyle(Theme.muted)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            RowFigures(rate: (record.tokensEstimated ? "~" : "") + Format.rate(record.tps), ttft: record.ttft)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .help(help)
    }

    private var help: String {
        let origin: String
        switch record.source {
        case "log": origin = "from the harness's session log"
        case "network": origin = "from network timing, tokens estimated"
        case "proxy": origin = "through the proxy"
        default: origin = "via \(record.upstreamHost)"
        }
        return "\(Format.tokens(record.outputTokens)) tokens out, \(origin)"
    }
}

private struct OtherActiveRow: View {
    let request: LiveRequest

    var body: some View {
        HStack(spacing: 10) {
            HarnessDot(harness: request.snapshot.harness)
            VStack(alignment: .leading, spacing: 1) {
                Text(request.snapshot.model)
                    .font(.system(size: 12, weight: .medium))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text("\(request.snapshot.harness) · also running")
                    .font(.system(size: 10.5))
                    .foregroundStyle(Theme.muted)
            }
            Spacer(minLength: 8)
            RowFigures(rate: request.smoothedRate.map { "~" + Format.wholeRate($0) } ?? "–", ttft: request.snapshot.ttft)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}

// MARK: - Models

private enum ModelSort: String, CaseIterable, Identifiable {
    case speed = "Speed"
    case ttft = "TTFT"
    case calls = "Calls"
    case recent = "Recent"

    var id: String { rawValue }
}

private struct ModelsTab: View {
    @EnvironmentObject private var store: MetricsStore
    private let sortState: State<ModelSort>

    init() {
        sortState = State(initialValue: .speed)
    }

    var body: some View {
        let rawStats = store.modelStats
        let sortedStats: [ModelStat] = {
            switch sortState.wrappedValue {
            case .speed:
                return rawStats.sorted { ($0.medianTPS ?? 0) > ($1.medianTPS ?? 0) }
            case .ttft:
                return rawStats.sorted { ($0.medianTTFT ?? Double.infinity) < ($1.medianTTFT ?? Double.infinity) }
            case .calls:
                return rawStats.sorted { $0.runs > $1.runs }
            case .recent:
                return rawStats.sorted { $0.lastUsed > $1.lastUsed }
            }
        }()
        let fastest = rawStats.compactMap(\.medianTPS).max() ?? 1

        if rawStats.isEmpty {
            Card(padding: 16) {
                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 6) {
                        Image(systemName: "cpu")
                            .font(.system(size: 14))
                            .foregroundStyle(Theme.accent)
                        Text("No models recorded yet")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    Text("Models show up here after their first call. Median speed and TTFT will be calculated automatically.")
                        .font(.system(size: 12))
                        .foregroundStyle(Theme.muted)
                }
            }
        } else {
            VStack(alignment: .leading, spacing: 8) {
                HStack {
                    SectionLabel("Median performance")
                    Spacer()
                    Picker("", selection: sortState.projectedValue) {
                        ForEach(ModelSort.allCases) { Text($0.rawValue).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .controlSize(.mini)
                    .frame(width: 190)
                }
                Card(padding: 0) {
                    if sortedStats.count > 6 {
                        ScrollView {
                            rows(sortedStats, fastest: fastest)
                        }
                        .frame(height: 384)
                    } else {
                        rows(sortedStats, fastest: fastest)
                    }
                }
            }
        }
    }

    private func rows(_ stats: [ModelStat], fastest: Double) -> some View {
        VStack(spacing: 0) {
            ForEach(stats) { stat in
                ModelRow(stat: stat, fastest: fastest)
                if stat.id != stats.last?.id { RowDivider() }
            }
        }
    }
}

private struct ModelRow: View {
    let stat: ModelStat
    let fastest: Double

    var body: some View {
        VStack(spacing: 6) {
            HStack(spacing: 9) {
                HarnessDot(harness: stat.harness, size: 7)
                VStack(alignment: .leading, spacing: 1) {
                    Text(stat.model)
                        .font(.system(size: 12, weight: .medium))
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Text("\(stat.harness) · \(stat.runs) \(stat.runs == 1 ? "call" : "calls") · \(Format.ago(stat.lastUsed)) ago")
                        .font(.system(size: 10.5))
                        .foregroundStyle(Theme.muted)
                        .lineLimit(1)
                }
                Spacer(minLength: 8)
                RowFigures(rate: Format.rate(stat.medianTPS), ttft: stat.medianTTFT)
            }
            GeometryReader { proxy in
                ZStack(alignment: .leading) {
                    Capsule().fill(Theme.divider)
                    Capsule()
                        .fill(
                            LinearGradient(
                                colors: [
                                    HarnessStyle.color(for: stat.harness),
                                    HarnessStyle.color(for: stat.harness).opacity(0.7)
                                ],
                                startPoint: .leading,
                                endPoint: .trailing
                            )
                        )
                        .frame(width: max(4, proxy.size.width * min(1, (stat.medianTPS ?? 0) / max(fastest, 1))))
                }
            }
            .frame(height: 4)
            .padding(.leading, 16)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}
