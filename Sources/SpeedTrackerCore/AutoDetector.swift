import Foundation

/// Finds model calls on its own, with nothing configured in any harness.
///
/// Two passive sources work together. The harness's session log says which
/// model answered and how many tokens it produced. The process's network
/// traffic says when the request left and when the reply started, which is
/// what makes the numbers live. Where a harness has no log this app can read,
/// traffic alone still gives timing, with an estimated token count.
public final class AutoDetector: @unchecked Sendable {
    public struct HarnessStatus: Equatable, Sendable, Identifiable {
        public var name: String
        /// The harness's session logs were found, so its token counts are exact.
        public var hasLogs: Bool
        public var isRunning: Bool
        /// Its command or app was found on this machine.
        public var isInstalled: Bool

        public init(name: String, hasLogs: Bool, isRunning: Bool, isInstalled: Bool = false) {
            self.name = name
            self.hasLogs = hasLogs
            self.isRunning = isRunning
            self.isInstalled = isInstalled
        }

        public var id: String { name }
    }

    /// Both callbacks run on the detector's queue.
    public var onEvent: ((TrackerEvent) -> Void)?
    public var onStatus: (([HarnessStatus]) -> Void)?

    private let queue = DispatchQueue(label: "speedtracker.detector", qos: .utility)
    private var timer: DispatchSourceTimer?
    private let tracker = FlowTracker()
    private let logs: SessionLogWatcher
    private let hosts = HostCatalog()
    private let knownKeys: Set<String>
    private let defaults = UserDefaults.standard

    private struct Identity {
        var harness: String?
        var ignored: Bool
    }

    private struct Shown {
        var harness: String
        var provider: String
        var visible: Bool
        /// The harness has a session log this app reads, so its records come from there.
        var covered: Bool
        /// The process is a known harness, not just something talking to a provider.
        var isHarness: Bool
        var sentBytes: UInt64 = 0
        var sentAt: TimeInterval = 0
    }

    private var identities: [Int32: Identity] = [:]
    private var shown: [UUID: Shown] = [:]
    /// Calls that ended recently, kept to line up with log entries that arrive later.
    private var ended: [(call: FlowCall, harness: String)] = []
    private var didBackfill = false
    private var target: String?
    private var watched: Set<Int32> = []
    private var lastDiscovery: TimeInterval = -100
    /// Addresses a harness has streamed from before, beyond the built-in provider list.
    private var learned: [String: Set<String>] = [:]
    private var bytesPerToken: [String: Double]
    private var interval: TimeInterval = 0
    private var lastLogPoll: TimeInterval = 0
    private var lastStatus: [HarnessStatus] = []
    private var lastStatusAt: TimeInterval = 0
    private var runningHarnesses: Set<String> = []
    /// Harnesses whose command or app exists on this machine.
    private var installedHarnesses: Set<String> = []
    private var lastPresenceScan: TimeInterval = -1000
    private let findInstalled: () -> Set<String>

    /// While a request waits for its first byte, so the wait is timed closely.
    private static let fastInterval: TimeInterval = 0.1
    /// While text streams: enough for a live rate.
    private static let streamInterval: TimeInterval = 0.25
    /// A harness is open but quiet.
    private static let slowInterval: TimeInterval = 1.0
    /// Nothing to watch; only the periodic search for harnesses runs.
    private static let idleInterval: TimeInterval = 5.0
    private static let calibrationKey = "bytesPerToken"
    private static let maxRecordAge: TimeInterval = 7 * 24 * 3600

    /// `knownKeys` are the log entries already in history, so a relaunch does not record them again.
    /// `findInstalled` reports which harnesses are installed; tests replace it.
    public init(
        logs: SessionLogWatcher = SessionLogWatcher(), knownKeys: Set<String> = [],
        findInstalled: @escaping () -> Set<String> = { HarnessPresence.installed() }
    ) {
        self.logs = logs
        self.knownKeys = knownKeys
        self.findInstalled = findInstalled
        bytesPerToken = (defaults.dictionary(forKey: Self.calibrationKey) as? [String: Double]) ?? [:]
    }

    public func start() {
        queue.async { [self] in
            guard timer == nil else { return }
            // Before anything else, see which harnesses this machine has, so the first
            // list the user is offered already holds only those.
            scanPresence(now: Clock.now)
            publishStatus(now: Clock.now, force: true)
            hosts.refreshIfNeeded()
            schedule(Self.slowInterval)
        }
    }

    /// The one harness the user chose to watch, or nil to watch them all. Other
    /// harnesses stop being sampled when their session logs already record their
    /// calls, so nothing is lost and less work is done.
    public func setTarget(_ harness: String?) {
        queue.async { [self] in
            target = harness
            lastDiscovery = -100
        }
    }

    public func stop() {
        queue.async { [self] in
            timer?.cancel()
            timer = nil
        }
    }

    private func schedule(_ newInterval: TimeInterval) {
        guard newInterval != interval || timer == nil else { return }
        interval = newInterval
        timer?.cancel()
        let source = DispatchSource.makeTimerSource(queue: queue)
        source.schedule(deadline: .now() + newInterval, repeating: newInterval, leeway: .milliseconds(Int(newInterval * 200)))
        source.setEventHandler { [weak self] in self?.tick() }
        source.resume()
        timer = source
    }

    // MARK: Tick

    private func tick() {
        let now = Clock.now
        let wall = Date()
        hosts.refreshIfNeeded(now: wall)

        // Every few seconds look at all processes to find harnesses; in between, only at those.
        let discoveryInterval: TimeInterval = watched.isEmpty ? 5 : 3
        var raw: [FlowSample]?
        if now - lastDiscovery >= discoveryInterval {
            lastDiscovery = now
            let all = Nettop.sample()
            discover(in: all)
            raw = all
        } else if !watched.isEmpty {
            raw = Nettop.sample(pids: watched.sorted())
        }
        if let raw {
            let samples = raw.filter { sample in
                guard watched.contains(sample.pid), !Nettop.isLoopback(sample.remoteAddress) else { return false }
                // A known harness is followed everywhere. Anything else only towards a model provider.
                return identities[sample.pid]?.harness != nil
                    || hosts.host(for: sample.remoteAddress, includingShared: false) != nil
            }
            for event in tracker.ingest(samples, uptime: now, wall: wall) {
                handle(event)
            }
        }

        if now - lastLogPoll >= 0.25 {
            lastLogPoll = now
            for record in logs.poll(now: wall) { handle(record, wall: wall) }
            if !didBackfill {
                // Reading a week of sessions leaves a lot of freed memory behind; hand it back.
                didBackfill = true
                malloc_zone_pressure_relief(nil, 0)
            }
            ended.removeAll { now - $0.call.lastRxUptime > 180 }
        }
        syncLiveView(now: now, wall: wall)
        publishStatus(now: now)

        // Sample finely only around the moments that need it: each sample costs a process launch.
        let recentLog = logs.lastActivity.values.contains { wall.timeIntervalSince($0) < 4 }
        if tracker.isAwaitingFirstByte || (recentLog && !watched.isEmpty) {
            schedule(Self.fastInterval)
        } else if tracker.hasActiveCalls {
            schedule(Self.streamInterval)
        } else if !watched.isEmpty {
            schedule(Self.slowInterval)
        } else {
            schedule(Self.idleInterval)
        }
    }

    /// Finds the processes worth watching: known harnesses, and anything connected to a model provider.
    private func discover(in samples: [FlowSample]) {
        var found: Set<Int32> = []
        var running: Set<String> = []
        var live: Set<Int32> = []
        for sample in samples {
            live.insert(sample.pid)
            guard sample.pid != getpid(), !Nettop.isLoopback(sample.remoteAddress) else { continue }
            let identity = identify(sample)
            guard !identity.ignored else { continue }
            if let harness = identity.harness {
                running.insert(harness)
                let recordedByLog = logs.hasLogs(harness)
                if target == nil || target == harness || !recordedByLog { found.insert(sample.pid) }
            } else if hosts.host(for: sample.remoteAddress, includingShared: false) != nil {
                found.insert(sample.pid)
            }
        }
        watched = found
        runningHarnesses = running
        identities = identities.filter { live.contains($0.key) }
    }

    private func identify(_ sample: FlowSample) -> Identity {
        if let known = identities[sample.pid] { return known }
        var identity = Identity(harness: nil, ignored: HarnessCatalog.isIgnored(processName: sample.processName))
        if !identity.ignored, let path = ProcessInspector.path(of: sample.pid) {
            identity.harness = HarnessCatalog.classify(path: path, arguments: ProcessInspector.arguments(of: sample.pid))
            if identity.harness == nil { identity.ignored = HarnessCatalog.isHostAppProcess(path: path) }
        }
        identities[sample.pid] = identity
        return identity
    }

    // MARK: Flow events

    private func handle(_ event: FlowTracker.Event) {
        switch event {
        case .began(let call):
            shown[call.id] = describe(call)
        case .progressed(let call):
            guard let entry = shown[call.id] else { return }
            if call.isStream, entry.isHarness {
                // This harness streams from here, so trust the address from now on.
                learned[entry.harness, default: []].insert(call.remoteAddress)
            }
        case .ended(let call):
            guard let entry = shown.removeValue(forKey: call.id) else { return }
            if call.isStream { ended.append((call, entry.harness)) }
            if entry.covered || !call.isStream {
                // Either not a generation, or the session log records it; the traffic was only for the live view.
                if entry.visible { onEvent?(.finished(call.id, nil)) }
            } else {
                onEvent?(.finished(call.id, record(for: call, entry: entry)))
            }
        }
    }

    private func describe(_ call: FlowCall) -> Shown {
        let harness = identities[call.pid]?.harness
        let provider = hosts.host(for: call.remoteAddress, includingShared: harness != nil) ?? call.remoteAddress
        let name = harness ?? Self.displayName(process: call.processName)
        let covered = harness != nil && logs.hasLogs(name)
        return Shown(harness: name, provider: provider, visible: false, covered: covered, isHarness: harness != nil)
    }

    private static func displayName(process: String) -> String {
        if process.hasPrefix("Claude") { return "Claude Desktop" }
        if process.hasPrefix("ChatGPT") { return "ChatGPT" }
        return process.isEmpty ? "Unknown" : process
    }

    /// Decides which open calls the UI should show, and tells it what changed.
    private func syncLiveView(now: TimeInterval, wall: Date) {
        for call in tracker.openCalls {
            guard var entry = shown[call.id] else { continue }
            let wanted = shouldShow(call, entry: entry, now: now, wall: wall)
            if wanted {
                // Tell the UI when a call appears, then only when it has new data, a few times a second.
                let changed = call.rxBytes != entry.sentBytes && now - entry.sentAt >= 0.1
                if !entry.visible || changed {
                    let snapshot = self.snapshot(call, entry: entry)
                    onEvent?(entry.visible ? .updated(snapshot) : .started(snapshot))
                    entry.sentBytes = call.rxBytes
                    entry.sentAt = now
                }
            } else if entry.visible {
                onEvent?(.finished(call.id, nil))
            }
            entry.visible = wanted
            shown[call.id] = entry
        }
    }

    private func shouldShow(_ call: FlowCall, entry: Shown, now: TimeInterval, wall: Date) -> Bool {
        if call.dormant {
            // Silence is only worth showing when the harness's own log says a response is still due,
            // and this call is the one that request started.
            guard entry.covered, call.firstByteUptime != nil, logs.isAwaiting(entry.harness, now: wall),
                  let request = logs.lastRequest(entry.harness), let began = call.requestStartWall
            else { return false }
            return began >= request.addingTimeInterval(-2)
        }
        if entry.covered {
            // A harness whose log has been silent for a long time is not generating, whatever its connections do.
            guard let active = logs.lastActivity[entry.harness], wall.timeIntervalSince(active) < 900 else { return false }
        }
        if call.isStream { return true }
        if entry.covered, let request = logs.lastRequest(entry.harness), let began = call.requestStartWall,
           began < request.addingTimeInterval(-2) {
            // Left over from before the request now in flight; not the call the user is waiting on.
            return false
        }
        // Before it has proved to be a stream, only a known harness talking to a known
        // provider is shown, and only after a moment, so housekeeping calls do not flash by.
        guard entry.isHarness else { return false }
        let trusted = learned[entry.harness]?.contains(call.remoteAddress) == true
            || hosts.host(for: call.remoteAddress, includingShared: true) != nil
        guard trusted else { return false }
        // Harnesses also make housekeeping calls to the same host. The log tells a real request apart.
        if entry.covered, !logs.isAwaiting(entry.harness, now: wall) { return false }
        return call.firstByteUptime != nil || now - (call.requestStartUptime ?? now) >= 0.4
    }

    private func snapshot(_ call: FlowCall, entry: Shown) -> LiveSnapshot {
        let model = entry.covered ? (logs.latestModel[entry.harness] ?? entry.provider) : entry.provider
        var start = call.requestStartUptime ?? call.firstByteUptime ?? call.lastRxUptime
        if entry.covered, let logged = logs.lastRequest(entry.harness) {
            // Sampling may have caught the upload late; the harness's log knows when it really went out.
            let loggedUptime = Clock.now - Date().timeIntervalSince(logged)
            if loggedUptime < start, start - loggedUptime < 1.5 { start = loggedUptime }
        }
        let ratio = bytesPerToken[entry.harness] ?? Self.defaultBytesPerToken(provider: entry.provider)
        return LiveSnapshot(
            id: call.id, startedAt: call.requestStartWall ?? call.firstByteWall ?? Date(), startUptime: start,
            harness: entry.harness, route: entry.provider, model: model,
            firstTokenUptime: call.firstByteUptime, firstCharUptime: call.firstByteUptime,
            firstVisibleUptime: call.sustained ? call.firstByteUptime : nil,
            lastTokenUptime: call.firstByteUptime == nil ? nil : call.lastRxUptime,
            estimatedTokens: call.firstByteUptime == nil ? 0 : Double(call.rxBytes) / ratio
        )
    }

    /// A record built from traffic alone, for a harness with no readable log.
    private func record(for call: FlowCall, entry: Shown) -> RequestRecord? {
        // Without having seen the request go out there is no TTFT, and no proof it was a model call.
        guard let first = call.firstByteUptime, call.ttft != nil else { return nil }
        let ratio = bytesPerToken[entry.harness] ?? Self.defaultBytesPerToken(provider: entry.provider)
        let tokens = Int((Double(call.rxBytes) / ratio).rounded())
        let generation = call.lastRxUptime - first
        guard tokens >= 5, generation >= 0.5, Double(call.rxBytes) / generation >= 400 else { return nil }
        let start = call.requestStartUptime ?? first
        return RequestRecord(
            id: call.id, startedAt: call.requestStartWall ?? call.firstByteWall ?? Date(), harness: entry.harness,
            route: entry.provider, upstreamHost: entry.provider, format: .unknown, model: entry.provider,
            streamed: true, status: 200, ttft: call.ttft, firstVisible: nil, ttfb: call.ttft,
            generation: generation, total: call.lastRxUptime - start, inputTokens: nil, cachedInputTokens: nil,
            outputTokens: tokens, reasoningTokens: nil, tokensEstimated: true,
            tps: generation >= 0.05 ? Double(tokens) / generation : nil, aborted: false, source: "network"
        )
    }

    /// Stream bytes per output token before anything has been measured. Anthropic
    /// batches several tokens into each event; OpenAI-style streams send about one.
    static func defaultBytesPerToken(provider: String) -> Double {
        provider.contains("anthropic") ? 22 : 180
    }

    // MARK: Log records

    private func handle(_ log: LogRecord, wall: Date) {
        guard !knownKeys.contains(log.key), wall.timeIntervalSince(log.end) < Self.maxRecordAge else { return }
        let match = flow(matching: log)
        if let match, log.outputTokens >= 20, match.rxBytes > 0 {
            // Learn this harness's bytes per token, so its next live estimate is closer.
            let observed = min(max(Double(match.rxBytes) / Double(log.outputTokens), 3), 800)
            let previous = bytesPerToken[log.harness] ?? observed
            bytesPerToken[log.harness] = previous * 0.7 + observed * 0.3
            defaults.set(bytesPerToken, forKey: Self.calibrationKey)
        }
        var observed: (requestStart: Date, firstByte: Date)?
        if let match, match.ttft != nil, let start = match.requestStartWall, let first = match.firstByteWall {
            observed = (start, first)
        }
        onEvent?(.finished(UUID(), log.makeRecord(observed: observed)))
    }

    /// The traffic that carried a logged response: same harness, ending at the same moment.
    private func flow(matching log: LogRecord) -> FlowCall? {
        // The log entry can arrive before the traffic has gone quiet, so open calls count too.
        let open = tracker.openCalls.compactMap { call in shown[call.id].map { (call: call, harness: $0.harness) } }
        func distance(_ call: FlowCall) -> TimeInterval { abs(call.lastRxWall.timeIntervalSince(log.end)) }
        return (ended + open)
            .filter { $0.harness == log.harness && $0.call.isStream && distance($0.call) < 3 }
            .min { distance($0.call) < distance($1.call) }?
            .call
    }

    // MARK: Status

    /// A harness can be installed while the app is open, so look again once a minute.
    private func scanPresence(now: TimeInterval) {
        guard now - lastPresenceScan >= 60 else { return }
        lastPresenceScan = now
        installedHarnesses = findInstalled()
    }

    /// Reports the harnesses present on this machine: installed, keeping session logs, or
    /// running right now. One that is none of these is not reported, so it cannot be chosen.
    private func publishStatus(now: TimeInterval, force: Bool = false) {
        guard force || now - lastStatusAt >= 2 else { return }
        lastStatusAt = now
        scanPresence(now: now)
        let logged = Set(logs.detectedHarnesses)
        let statuses = logged.union(installedHarnesses).union(runningHarnesses).sorted().map {
            HarnessStatus(
                name: $0, hasLogs: logged.contains($0), isRunning: runningHarnesses.contains($0),
                isInstalled: installedHarnesses.contains($0)
            )
        }
        if force || statuses != lastStatus {
            lastStatus = statuses
            onStatus?(statuses)
        }
    }
}
