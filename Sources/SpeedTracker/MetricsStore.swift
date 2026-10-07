import Combine
import Foundation
import SpeedTrackerCore

/// A call in flight plus the smoothed rate the UI shows for it.
struct LiveRequest: Identifiable {
    var snapshot: LiveSnapshot
    private var samples: [(time: TimeInterval, tokens: Double)] = []
    /// Tokens per second, smoothed. Held, not zeroed, while the model pauses.
    private(set) var smoothedRate: Double?
    private(set) var peakRate: Double = 0
    /// Smoothed rate at each moment the stream was flowing, for the sparkline.
    private(set) var rateHistory: [Double] = []

    /// The speed on display when this stretch of text began. A new stretch starts from
    /// it and moves to its own measurement as that becomes trustworthy.
    private var carried: Double?
    private var flowStart: TimeInterval?
    private var lastFlow: TimeInterval?

    private static let window: TimeInterval = 3
    /// Shortest stretch that gives a usable rate.
    private static let warmUp: TimeInterval = 0.75
    /// How long until a stretch's own rate fully replaces the carried one.
    private static let settle: TimeInterval = 6

    var id: UUID { snapshot.id }

    init(snapshot: LiveSnapshot, carried: Double?) {
        self.snapshot = snapshot
        self.carried = carried
    }

    mutating func update(_ new: LiveSnapshot) {
        snapshot = new
        guard let time = new.lastTokenUptime, new.estimatedTokens > 0 else { return }
        if let last = samples.last, last.tokens == new.estimatedTokens { return }
        samples.append((time, new.estimatedTokens))
        if samples.count > 600 { samples.removeFirst(samples.count - 600) }
    }

    /// Tokens per second between two moments, or nil when they are too close to measure.
    private func rate(from start: TimeInterval, to end: TimeInterval) -> Double? {
        guard let first = samples.first, let last = samples.last, end - start >= Self.warmUp else { return nil }
        let baseline = samples.last { $0.time <= start }?.tokens ?? first.tokens
        return max(0, last.tokens - baseline) / (end - start)
    }

    /// Advances the smoothed rate. It only moves while text is arriving. Through a pause
    /// the last value stands: a pause is the model thinking or calling a tool, not its
    /// speed falling to zero. After one, the rate picks up from where it was instead of
    /// climbing back from nothing.
    mutating func advance(at now: TimeInterval) {
        guard let first = samples.first, let last = samples.last, now - last.time <= 1.2 else { return }
        // Occasional keep-alive bytes while a model thinks are not text.
        guard let recent = rate(from: max(now - Self.window, first.time), to: now),
              snapshot.phase == .streaming || recent >= 15
        else { return }

        if flowStart == nil || lastFlow.map({ now - $0 > 1.5 }) ?? false {
            // A new stretch of text, at the start of the call or after a pause.
            flowStart = max(first.time, now - 1)
            carried = smoothedRate ?? carried
        }
        lastFlow = now
        guard let start = flowStart, let measured = rate(from: max(now - Self.window, start), to: now) else { return }

        var target = measured
        if let carried {
            let trust = min(1, (now - start) / Self.settle)
            target = carried + (measured - carried) * trust
        }
        let current = smoothedRate ?? carried ?? target
        let smoothed = current + (target - current) * 0.3
        smoothedRate = smoothed
        peakRate = max(peakRate, smoothed)
        rateHistory.append(smoothed)
        if rateHistory.count > 200 { rateHistory.removeFirst(rateHistory.count - 200) }
    }
}

struct ModelStat: Identifiable {
    var harness: String
    var model: String
    var runs: Int
    var medianTTFT: Double?
    var medianTPS: Double?
    var lastUsed: Date

    var id: String { harness + "\u{1F}" + model }
}

struct HarnessInfo: Identifiable {
    var name: String
    var isGenerating: Bool
    var isOpen: Bool
    /// Token counts come from the harness's own log, not from an estimate.
    var hasLogs: Bool
    var lastCall: Date?
    /// Found on this machine. Only a pinned target that has since gone missing is listed without it.
    var isPresent = true

    var id: String { name }
}

struct PeriodStats {
    var label: String
    var calls: Int
    var medianTTFT: Double?
    var medianTPS: Double?
}

/// Everything the menu bar and popover show. Lives on the main thread.
final class MetricsStore: ObservableObject {
    @Published private var allActive: [LiveRequest] = []
    /// Newest first, every harness.
    @Published private var allRecent: [RequestRecord] = []
    @Published private(set) var proxyState: ProxyServer.State = .stopped
    @Published private var statuses: [AutoDetector.HarnessStatus] = []
    /// The harness the Live tab and menu bar are set to, or nil for Auto: whichever is active.
    @Published private(set) var target: String?
    /// Advances a few times a second while something is streaming, so timers and rates redraw.
    @Published private(set) var now: TimeInterval = Clock.now
    /// The speed on display. It moves with the stream, stays put through pauses and
    /// between calls, and settles on the exact figure when a call is logged.
    @Published private(set) var displayRate: Double?
    @Published private(set) var displayTTFT: Double?

    var port: UInt16 = ProxyConfig.defaultPort
    var routes: [String: String] = ProxyConfig.defaultRoutes
    var onTargetChange: ((String?) -> Void)?
    /// Historical consumers receive only records accepted by the existing deduplication path.
    let recorded = PassthroughSubject<RequestRecord, Never>()

    /// Dashboard activity is independent of the menu-bar target.
    var dashboardActive: [LiveRequest] { allActive }

    private let history: HistoryStore?
    private let defaults: UserDefaults?
    private var ticker: Timer?
    private var seenKeys: Set<String> = []
    private static let recentLimit = 1000
    private static let targetKey = "targetHarness"

    init(history: HistoryStore?, defaults: UserDefaults? = .standard) {
        self.history = history
        self.defaults = defaults
        target = defaults?.string(forKey: Self.targetKey)
        if let history {
            allRecent = history.loadRecent(limit: Self.recentLimit).sorted { $0.startedAt > $1.startedAt }
            seenKeys = history.loadSourceKeys()
        }
        settleDisplay()
    }

    /// Log entries already stored, so the detector can skip them.
    var knownSourceKeys: Set<String> { seenKeys }

    // MARK: Events

    func apply(_ event: TrackerEvent) {
        switch event {
        case .started(let snapshot):
            allActive.append(LiveRequest(snapshot: snapshot, carried: displayRate))
            startTicking()
        case .updated(let snapshot):
            guard let index = allActive.firstIndex(where: { $0.id == snapshot.id }) else { return }
            allActive[index].update(snapshot)
        case .finished(let id, let record):
            allActive.removeAll { $0.id == id }
            if let record, record.sourceKey.map({ seenKeys.insert($0).inserted }) ?? true {
                // Usually the newest, but entries read back from a harness log can be older.
                let index = allRecent.firstIndex { $0.startedAt <= record.startedAt } ?? allRecent.count
                allRecent.insert(record, at: index)
                if allRecent.count > Self.recentLimit { allRecent.removeLast(allRecent.count - Self.recentLimit) }
                history?.append(record)
                recorded.send(record)
                // Nothing in flight: let the display rest on the newest call, now known exactly.
                if primary == nil { settleDisplay() }
            }
            if allActive.isEmpty { stopTicking() }
        }
        now = Clock.now
    }

    func setProxyState(_ state: ProxyServer.State) {
        proxyState = state
    }

    func setHarnesses(_ statuses: [AutoDetector.HarnessStatus]) {
        self.statuses = statuses
    }

    private func startTicking() {
        guard ticker == nil else { return }
        let timer = Timer(timeInterval: 0.25, repeats: true) { [weak self] _ in self?.tick() }
        RunLoop.main.add(timer, forMode: .common)
        ticker = timer
    }

    private func stopTicking() {
        ticker?.invalidate()
        ticker = nil
    }

    private func tick() {
        let now = Clock.now
        for index in allActive.indices { allActive[index].advance(at: now) }
        if let primary {
            if let rate = primary.smoothedRate { displayRate = rate }
            if let ttft = primary.snapshot.ttft { displayTTFT = ttft }
        }
        self.now = now
    }

    private func settleDisplay() {
        displayRate = lastSignificant?.tps
        displayTTFT = lastSignificant?.ttft
    }

    // MARK: Target

    /// Points the Live tab and menu bar at one harness, or back to Auto with nil.
    func setTarget(_ harness: String?) {
        guard harness != target else { return }
        target = harness
        if let harness {
            defaults?.set(harness, forKey: Self.targetKey)
        } else {
            defaults?.removeObject(forKey: Self.targetKey)
        }
        onTargetChange?(harness)
        // The number on display belonged to the old target.
        settleDisplay()
        if let rate = primary?.smoothedRate { displayRate = rate }
    }

    /// Calls in flight from the target harness, or from any harness on Auto.
    var active: [LiveRequest] {
        guard let target else { return allActive }
        return allActive.filter { $0.snapshot.harness == target }
    }

    /// Finished calls from the target harness, or from any harness on Auto. Newest first.
    var recent: [RequestRecord] {
        guard let target else { return allRecent }
        return allRecent.filter { $0.harness == target }
    }

    /// Every harness the app knows about.
    var harnesses: [HarnessInfo] {
        var lastCall: [String: Date] = [:]
        for record in allRecent where lastCall[record.harness] == nil { lastCall[record.harness] = record.startedAt }
        let generating = Set(allActive.map(\.snapshot.harness))
        // Only harnesses the detector found on this machine. A name that survives only in
        // old history cannot be chosen; the pinned target stays listed so it can be changed.
        var names = statuses.map(\.name)
        if let target, !names.contains(target) { names.append(target) }
        return names.map { name in
            let status = statuses.first { $0.name == name }
            return HarnessInfo(
                name: name, isGenerating: generating.contains(name),
                isOpen: status?.isRunning ?? false, hasLogs: status?.hasLogs ?? false, lastCall: lastCall[name],
                isPresent: status != nil
            )
        }
    }

    // MARK: Derived

    /// Harnesses with a model call in flight right now, whatever the target.
    var generatingHarnesses: [String] {
        Array(Set(allActive.map(\.snapshot.harness))).sorted()
    }

    /// The call the menu bar follows: the one that has produced the most so
    /// far, so a short background call does not steal the display.
    var primary: LiveRequest? {
        active.max { lhs, rhs in
            if lhs.snapshot.estimatedTokens != rhs.snapshot.estimatedTokens {
                return lhs.snapshot.estimatedTokens < rhs.snapshot.estimatedTokens
            }
            return lhs.snapshot.startUptime > rhs.snapshot.startUptime
        }
    }

    /// Latest finished call worth showing. Tiny replies give a noisy rate, so
    /// they only count when there is nothing better.
    var lastSignificant: RequestRecord? {
        let recent = self.recent
        return recent.prefix(20).first { $0.outputTokens >= 16 && $0.tps != nil } ?? recent.first
    }

    /// One row per harness and model: the same model can behave differently in different harnesses.
    /// Always every harness; this is the view for comparing them.
    var modelStats: [ModelStat] {
        Dictionary(grouping: allRecent) { $0.harness + "\u{1F}" + $0.model }.values.compactMap { records in
            guard let first = records.first else { return nil }
            return ModelStat(
                harness: first.harness, model: first.model, runs: records.count,
                medianTTFT: Self.median(records.compactMap(\.ttft)),
                medianTPS: Self.median(records.filter { $0.outputTokens >= 16 }.compactMap(\.tps))
                    ?? Self.median(records.compactMap(\.tps)),
                lastUsed: records.map(\.startedAt).max() ?? .distantPast
            )
        }
        .sorted { $0.lastUsed > $1.lastUsed }
    }

    /// Today's calls, or the whole recent window when there are none yet today.
    var periodStats: PeriodStats {
        let recent = self.recent
        let startOfDay = Calendar.current.startOfDay(for: Date())
        let today = recent.prefix { $0.startedAt >= startOfDay }
        let records = today.isEmpty ? recent[...] : today
        return PeriodStats(
            label: today.isEmpty ? "Recent" : "Today", calls: records.count,
            medianTTFT: Self.median(records.compactMap(\.ttft)),
            medianTPS: Self.median(records.filter { $0.outputTokens >= 16 }.compactMap(\.tps))
        )
    }

    static func median(_ values: [Double]) -> Double? {
        guard !values.isEmpty else { return nil }
        let sorted = values.sorted()
        let middle = sorted.count / 2
        return sorted.count.isMultiple(of: 2) ? (sorted[middle - 1] + sorted[middle]) / 2 : sorted[middle]
    }

    // MARK: Demo data

    /// Fills the store with plausible calls for `--snapshot` renders.
    func loadDemo(streaming: Bool) {
        let now = Clock.now
        proxyState = .running(port: port)
        statuses = [
            .init(name: "Claude Code", hasLogs: true, isRunning: true),
            .init(name: "Codex", hasLogs: true, isRunning: true),
            .init(name: "OMP", hasLogs: true, isRunning: false),
            .init(name: "DeepSeek CLI", hasLogs: false, isRunning: false, isInstalled: true),
        ]
        func record(_ model: String, _ harness: String, ago: TimeInterval, ttft: Double, tps: Double, out: Int) -> RequestRecord {
            RequestRecord(
                id: UUID(), startedAt: Date().addingTimeInterval(-ago), harness: harness, route: "demo",
                upstreamHost: "example.com", format: .anthropic, model: model, streamed: true, status: 200,
                ttft: ttft, firstVisible: ttft + 0.4, ttfb: ttft * 0.6, generation: Double(out) / tps,
                total: ttft + Double(out) / tps, inputTokens: 18_400, cachedInputTokens: 17_900,
                outputTokens: out, reasoningTokens: nil, tokensEstimated: harness == "DeepSeek CLI", tps: tps,
                aborted: false, source: harness == "DeepSeek CLI" ? "network" : "log"
            )
        }
        allRecent = [
            record("claude-opus-5-5", "Claude Code", ago: 40, ttft: 1.42, tps: 68.4, out: 1840),
            record("gpt-6.1-sol", "Codex", ago: 190, ttft: 0.88, tps: 142.0, out: 960),
            record("deepseek-reasoner", "DeepSeek CLI", ago: 800, ttft: 2.31, tps: 34.7, out: 2210),
            record("claude-haiku-4-5-20251001", "Claude Code", ago: 1500, ttft: 0.41, tps: 171.3, out: 96),
            record("gpt-6-astra", "OMP", ago: 4200, ttft: 0.93, tps: 131.5, out: 512),
            record("claude-opus-5-5", "Claude Code", ago: 9000, ttft: 1.18, tps: 72.9, out: 3120),
        ]
        settleDisplay()
        guard streaming else { return }
        var live = LiveRequest(snapshot: LiveSnapshot(
            id: UUID(), startedAt: Date().addingTimeInterval(-11), startUptime: now - 11,
            harness: "Claude Code", route: "anthropic", model: "claude-opus-5-5",
            firstTokenUptime: now - 9.6, firstCharUptime: now - 9.6, firstVisibleUptime: now - 9.4,
            lastTokenUptime: now - 9.6, estimatedTokens: 0
        ), carried: displayRate)
        var tokens = 0.0
        for step in 0..<38 {
            let time = now - 9.5 + Double(step) * 0.25
            // A stream that speeds up, pauses for a moment, and resumes.
            let paused = (18..<22).contains(step)
            tokens += paused ? 0 : 17 + 6 * sin(Double(step) / 3) + Double(step % 5)
            var snapshot = live.snapshot
            if !paused { snapshot.lastTokenUptime = time }
            snapshot.estimatedTokens = tokens
            live.update(snapshot)
            live.advance(at: time)
        }
        allActive = [live]
        displayRate = live.smoothedRate
        displayTTFT = live.snapshot.ttft
        self.now = now
    }
}
