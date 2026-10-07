import Foundation

/// Endpoint identity, not a guess based on the model being served.
public struct ProviderIdentity: Identifiable, Equatable, Hashable, Sendable {
    public let id: String
    public let displayName: String
    public let host: String?
    public let evidence: String
    public let isUnverified: Bool

    public init(record: RequestRecord) {
        if let endpoint = Self.endpoint(record.upstreamHost) ?? Self.endpoint(record.route) {
            host = endpoint.host
            if Self.isLocal(endpoint.host) {
                id = "local:\(endpoint.authority)"
                displayName = "Local · \(endpoint.authority)"
            } else if let known = Self.hosts[endpoint.host] {
                id = known.id
                displayName = known.name
            } else {
                id = "host:\(endpoint.host)"
                displayName = endpoint.host
            }
            isUnverified = record.source != "proxy"
            switch record.source {
            case "proxy":
                evidence = "Observed upstream endpoint host: \(endpoint.authority). This identifies the endpoint, not necessarily the model vendor behind it."
            case "network":
                evidence = "Provider host attributed from passive network discovery: \(endpoint.authority). Shared addresses and gateways do not prove the original request host or downstream model vendor."
            case "log":
                evidence = "Endpoint host reported in session log: \(endpoint.authority); not independently verified on the network."
            default:
                evidence = "Recorded endpoint host: \(endpoint.authority); the legacy observation source is unknown, so attribution is unverified."
            }
            return
        }

        host = nil
        isUnverified = true
        let upstream = Self.label(record.upstreamHost)
        let route = Self.label(record.route)
        let label = upstream.isEmpty || upstream == "unknown" ? route : upstream
        if label.isEmpty || label == "unknown" || label == "_" {
            id = "unknown"
            displayName = "Unknown"
            evidence = "No endpoint host or provider label was recorded; provider is unknown."
        } else {
            let known = Self.aliases[label]
            id = known?.id ?? "reported:\(label)"
            displayName = known?.name ?? label
            let harness = record.harness.lowercased()
            if (harness == "claude code" && id == "anthropic") || (harness == "codex" && id == "openai") {
                evidence = "Legacy \(record.harness) provider label; endpoint unverified. Session logs historically used this default, so it is not proof of a direct provider connection."
            } else {
                evidence = "Reported provider label: \(label); endpoint unverified. No endpoint host was recorded."
            }
        }
    }

    private init(id: String, displayName: String, host: String?, evidence: String, isUnverified: Bool) {
        self.id = id
        self.displayName = displayName
        self.host = host
        self.evidence = evidence
        self.isUnverified = isUnverified
    }

    /// Aggregate identities retain uncertainty if any constituent call only reported a label.
    fileprivate func merging(_ other: ProviderIdentity) -> ProviderIdentity {
        if self == other { return self }
        let selectedHost: String?
        switch (host, other.host) {
        case let (first?, second?): selectedHost = min(first, second)
        case let (first?, nil): selectedHost = first
        case let (nil, second?): selectedHost = second
        case (nil, nil): selectedHost = nil
        }
        let unverified = isUnverified || other.isUnverified
        let evidence: String
        if unverified, let host = selectedHost {
            evidence = "Includes endpoint host \(host) and endpoint-unverified provenance; inspect individual calls for reported labels and observations."
        } else if unverified {
            evidence = "Reported provider labels only; endpoint unverified. Inspect individual calls for provenance."
        } else {
            evidence = "Recorded provider endpoints, including \(selectedHost ?? "unknown"); inspect individual calls for hosts. Endpoints do not necessarily identify model vendors."
        }
        return ProviderIdentity(id: id, displayName: displayName, host: selectedHost, evidence: evidence, isUnverified: unverified)
    }

    private struct KnownProvider {
        let id: String
        let name: String
    }

    private static let aliases: [String: KnownProvider] = {
        var result: [String: KnownProvider] = [:]
        let entries: [(String, String, [String])] = [
            ("anthropic", "Anthropic", ["anthropic", "claude"]),
            ("openai", "OpenAI", ["openai", "open-ai"]),
            ("openai-codex", "OpenAI Codex", ["openai-codex", "openai_codex", "chatgpt", "codex"]),
            ("gemini", "Google Gemini", ["gemini", "google", "google-gemini", "google-vertex", "vertex", "vertexai"]),
            ("deepseek", "DeepSeek", ["deepseek"]),
            ("openrouter", "OpenRouter", ["openrouter", "open-router"]),
            ("xai", "xAI", ["xai", "x-ai", "x.ai"]),
            ("groq", "Groq", ["groq"]),
            ("cerebras", "Cerebras", ["cerebras"]),
            ("mistral", "Mistral", ["mistral"]),
            ("moonshot", "Moonshot", ["moonshot", "kimi"]),
            ("zai", "Z.ai", ["zai", "z-ai", "z.ai", "zhipu"]),
            ("together", "Together AI", ["together", "togetherai", "together-ai"]),
            ("fireworks", "Fireworks AI", ["fireworks", "fireworks-ai"]),
            ("dashscope", "Alibaba DashScope", ["dashscope", "alibaba", "qwen"]),
            ("ollama", "Ollama (reported local)", ["ollama"]),
            ("lmstudio", "LM Studio (reported local)", ["lmstudio", "lm-studio", "lm_studio"]),
        ]
        for (id, name, names) in entries {
            for nameAlias in names { result[nameAlias] = KnownProvider(id: id, name: name) }
        }
        return result
    }()

    private static let hosts: [String: KnownProvider] = {
        let entries: [(String, [String])] = [
            ("anthropic", ["api.anthropic.com"]),
            ("openai", ["api.openai.com"]),
            ("openai-codex", ["chatgpt.com", "www.chatgpt.com", "chat.openai.com"]),
            ("gemini", ["generativelanguage.googleapis.com", "cloudcode-pa.googleapis.com", "daily-cloudcode-pa.googleapis.com", "aiplatform.googleapis.com"]),
            ("deepseek", ["api.deepseek.com"]),
            ("openrouter", ["openrouter.ai", "api.openrouter.ai"]),
            ("xai", ["api.x.ai"]),
            ("groq", ["api.groq.com"]),
            ("cerebras", ["api.cerebras.ai"]),
            ("mistral", ["api.mistral.ai"]),
            ("moonshot", ["api.moonshot.ai", "api.moonshot.cn", "api.kimi.com"]),
            ("zai", ["api.z.ai", "open.bigmodel.cn"]),
            ("together", ["api.together.xyz", "api.together.ai"]),
            ("fireworks", ["api.fireworks.ai"]),
            ("dashscope", ["dashscope.aliyuncs.com", "dashscope-intl.aliyuncs.com"]),
        ]
        var result: [String: KnownProvider] = [:]
        for (alias, hostnames) in entries {
            for hostname in hostnames { result[hostname] = aliases[alias] }
        }
        return result
    }()

    private static let labelCharacters = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-_. "))
    private static let hostCharacters = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-_.:"))

    private static func label(_ value: String) -> String {
        let label = value.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        // Malformed URLs must not become labels that expose userinfo, paths or query credentials.
        return label.unicodeScalars.allSatisfy(labelCharacters.contains) ? label : ""
    }

    private static func endpoint(_ value: String) -> (host: String, authority: String)? {
        let value = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !value.isEmpty else { return nil }
        let hasScheme = value.contains("://")
        guard hasScheme || aliases[label(value)] == nil else { return nil }
        // A bare provider alias is not an endpoint. Single-label hosts require an explicit URL.
        guard hasScheme || value.contains(".") || value.contains(":") || value.lowercased() == "localhost" else { return nil }
        let input: String
        if hasScheme {
            input = value
        } else if value.firstIndex(of: ":") != value.lastIndex(of: ":"), !value.hasPrefix("[") {
            input = "http://[\(value)]"
        } else {
            input = "http://\(value)"
        }
        guard let components = URLComponents(string: input), let rawHost = components.host, !rawHost.isEmpty else { return nil }
        let host = rawHost.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "[]."))
        guard !host.isEmpty, host.unicodeScalars.allSatisfy(hostCharacters.contains) else { return nil }
        // DNS hosts do not include ports in provider identity; local servers do, to distinguish engines.
        let authorityHost = host.contains(":") ? "[\(host)]" : host
        let authority = components.port.map { "\(authorityHost):\($0)" } ?? authorityHost
        return (host, authority)
    }

    private static func isLocal(_ host: String) -> Bool {
        host == "localhost" || host.hasSuffix(".localhost") || host.hasPrefix("127.") || host == "::1" || host == "0.0.0.0" || host == "::"
    }
}

public enum DashboardMetric: String, CaseIterable, Identifiable, Sendable {
    case speed, latency, calls, tokens

    public var id: String { rawValue }
    public var title: String {
        switch self {
        case .speed: return "Speed"
        case .latency: return "TTFT"
        case .calls: return "Calls"
        case .tokens: return "Output tokens"
        }
    }
    public var unit: String {
        switch self {
        case .speed: return "tok/s"
        case .latency: return "s"
        case .calls: return "calls"
        case .tokens: return "tokens"
        }
    }
    public func value(in summary: DashboardSummary) -> Double? {
        switch self {
        case .speed: return summary.medianTPS
        case .latency: return summary.medianTTFT
        case .calls: return Double(summary.count)
        case .tokens: return Double(summary.outputTokens)
        }
    }
}

public struct DashboardQuery: Equatable, Sendable {
    public let from: Date?
    public let through: Date?
    public let harness: String?
    public let providerID: String?
    public let model: String?

    public init(from: Date? = nil, through: Date? = nil, harness: String? = nil, providerID: String? = nil, model: String? = nil) {
        self.from = from
        self.through = through
        self.harness = harness
        self.providerID = providerID
        self.model = model
    }

    /// Date boundaries are inclusive at `from`, exclusive at `through`.
    public func includes(_ record: RequestRecord) -> Bool {
        includesDate(record) && (harness == nil || harness == record.harness)
            && (model == nil || model == record.model)
            && (providerID == nil || providerID == ProviderIdentity(record: record).id)
    }

    fileprivate func includesDate(_ record: RequestRecord) -> Bool {
        record.startedAt.timeIntervalSince1970.isFinite
            && (from == nil || record.startedAt >= from!)
            && (through == nil || record.startedAt < through!)
    }
}

public struct DashboardHistory: Sendable {
    public let records: [RequestRecord]
    public let skippedLines: Int

    public init(records: [RequestRecord], skippedLines: Int) {
        self.records = records
        self.skippedLines = skippedLines
    }
}

/// R7 percentiles: linear interpolation at `(sampleCount - 1) * probability`.
/// Only finite, nonnegative measurements from non-interrupted, non-HTTP-error calls participate.
/// TPS additionally requires a finite, positive generation window; whole-request fallback is separate.
public struct DashboardSummary: Sendable {
    public let count: Int
    public let medianTTFT: Double?
    public let p95TTFT: Double?
    public let medianTPS: Double?
    public let p95TPS: Double?
    public let outputTokens: Int
    public let estimatedCount: Int
    public let interruptedCount: Int
    public let latencyCount: Int
    public let speedCount: Int
    public let roundTripCount: Int

    public init(records: [RequestRecord]) {
        self.init(measurements: DashboardMeasurements(records: records))
    }

    fileprivate init(measurements: DashboardMeasurements) {
        count = measurements.count
        let latency = measurements.latency.sorted()
        let speed = measurements.speed.sorted()
        medianTTFT = Self.percentile(latency, probability: 0.5)
        p95TTFT = Self.percentile(latency, probability: 0.95)
        medianTPS = Self.percentile(speed, probability: 0.5)
        p95TPS = Self.percentile(speed, probability: 0.95)
        outputTokens = measurements.outputTokens
        estimatedCount = measurements.estimatedCount
        interruptedCount = measurements.interruptedCount
        latencyCount = latency.count
        speedCount = speed.count
        roundTripCount = measurements.roundTripCount
    }

    private static func percentile(_ sorted: [Double], probability: Double) -> Double? {
        guard !sorted.isEmpty else { return nil }
        let position = Double(sorted.count - 1) * probability
        let lower = Int(position)
        let upper = min(lower + 1, sorted.count - 1)
        return sorted[lower] + (sorted[upper] - sorted[lower]) * (position - Double(lower))
    }
}

public struct DashboardGroup: Identifiable, Sendable {
    public let id: String
    public let provider: ProviderIdentity
    public let harness: String
    public let summary: DashboardSummary

    public init(id: String, provider: ProviderIdentity, harness: String, summary: DashboardSummary) {
        self.id = id
        self.provider = provider
        self.harness = harness
        self.summary = summary
    }
}

public struct DashboardTrend: Identifiable, Sendable {
    public let id: Date
    public let date: Date
    public let summary: DashboardSummary

    public init(date: Date, summary: DashboardSummary) {
        id = date
        self.date = date
        self.summary = summary
    }
}

public struct DashboardBin: Identifiable, Sendable {
    public let id: Int
    public let lower: Double
    public let upper: Double
    public let count: Int

    public init(id: Int, lower: Double, upper: Double, count: Int) {
        self.id = id
        self.lower = lower
        self.upper = upper
        self.count = count
    }
}

public struct DashboardSourceCount: Identifiable, Sendable {
    public let id: String
    public let count: Int

    public init(id: String, count: Int) {
        self.id = id
        self.count = count
    }
}

public struct DashboardReport: Sendable {
    public let records: [RequestRecord]
    public let providers: [ProviderIdentity]
    public let harnesses: [String]
    public let models: [String]
    /// What each filter can still be set to, given the other two. Choosing a harness narrows
    /// the models and providers to the ones it has calls with, and the same in every direction,
    /// so no offered choice leads to an empty result. A dimension's own filter does not narrow it,
    /// which keeps its alternatives on offer.
    public let harnessOptions: [String]
    public let providerOptions: [ProviderIdentity]
    public let modelOptions: [String]
    public let summary: DashboardSummary
    public let groups: [DashboardGroup]
    public let trends: [DashboardTrend]
    public let sources: [DashboardSourceCount]
    private let measurements: DashboardMeasurements

    public init(records: [RequestRecord], query: DashboardQuery = DashboardQuery()) {
        struct GroupKey: Hashable {
            let provider: String
            let harness: String
        }
        struct GroupRecords {
            var provider: ProviderIdentity
            var records: [RequestRecord] = []

            mutating func append(_ record: RequestRecord, provider: ProviderIdentity) {
                self.provider = self.provider.merging(provider)
                records.append(record)
            }
        }
        var seen: Set<DashboardRecordKey> = []
        var selected: [RequestRecord] = []
        var identities: [String: ProviderIdentity] = [:]
        var windowHarnesses: Set<String> = []
        var windowModels: Set<String> = []
        var harnessFacet: Set<String> = []
        var providerFacet: Set<String> = []
        var modelFacet: Set<String> = []
        var grouped: [GroupKey: GroupRecords] = [:]
        var sourceCounts: [String: Int] = [:]
        for record in records {
            guard seen.insert(DashboardRecordKey(record)).inserted, query.includesDate(record) else { continue }
            let provider = ProviderIdentity(record: record)
            identities[provider.id] = identities[provider.id].map { $0.merging(provider) } ?? provider
            windowHarnesses.insert(record.harness)
            windowModels.insert(record.model)
            let harnessMatches = query.harness == nil || query.harness == record.harness
            let providerMatches = query.providerID == nil || query.providerID == provider.id
            let modelMatches = query.model == nil || query.model == record.model
            if providerMatches, modelMatches { harnessFacet.insert(record.harness) }
            if harnessMatches, modelMatches { providerFacet.insert(provider.id) }
            if harnessMatches, providerMatches { modelFacet.insert(record.model) }
            guard harnessMatches, providerMatches, modelMatches else { continue }
            selected.append(record)
            let key = GroupKey(provider: provider.id, harness: record.harness)
            grouped[key, default: GroupRecords(provider: provider)].append(record, provider: provider)
            let source: String
            switch record.source {
            case "log"?, "network"?, "proxy"?: source = record.source!
            default: source = "unknown"
            }
            sourceCounts[source, default: 0] += 1
        }
        providers = identities.values.sorted {
            $0.displayName == $1.displayName ? $0.id < $1.id : $0.displayName < $1.displayName
        }
        harnesses = windowHarnesses.sorted()
        models = windowModels.sorted()
        harnessOptions = harnessFacet.sorted()
        providerOptions = providers.filter { providerFacet.contains($0.id) }
        modelOptions = modelFacet.sorted()
        selected.sort {
            $0.startedAt == $1.startedAt ? $0.id.uuidString < $1.id.uuidString : $0.startedAt > $1.startedAt
        }
        self.records = selected
        measurements = DashboardMeasurements(records: selected)
        summary = DashboardSummary(measurements: measurements)
        groups = grouped.map { key, calls in
            // Length-prefixed components keep IDs collision-free even for custom labels.
            let id = "\(key.provider.utf8.count):\(key.provider)\(key.harness.utf8.count):\(key.harness)"
            return DashboardGroup(id: id, provider: calls.provider, harness: key.harness, summary: DashboardSummary(records: calls.records))
        }.sorted {
            if $0.provider.displayName != $1.provider.displayName { return $0.provider.displayName < $1.provider.displayName }
            if $0.provider.id != $1.provider.id { return $0.provider.id < $1.provider.id }
            return $0.harness < $1.harness
        }
        sources = ["log", "network", "proxy", "unknown"].compactMap { source in
            sourceCounts[source].map { DashboardSourceCount(id: source, count: $0) }
        }
        trends = Self.makeTrends(selected)
    }

    public func histogram(for metric: DashboardMetric) -> [DashboardBin] {
        let values: [Double]
        switch metric {
        case .speed: values = measurements.speed
        case .latency: values = measurements.latency
        case .calls, .tokens: return []
        }
        guard let minimum = values.min(), let maximum = values.max() else { return [] }
        let count = min(12, max(1, Int(ceil(sqrt(Double(values.count))))))
        let width = (maximum - minimum) / Double(count)
        guard width > 0 else {
            let upper = maximum == 0 ? 1 : maximum + maximum * 0.05
            return [DashboardBin(id: 0, lower: minimum * 0.95, upper: upper.isFinite ? upper : maximum, count: values.count)]
        }
        var counts = Array(repeating: 0, count: count)
        for value in values {
            let index = min(count - 1, Int((value - minimum) / width))
            counts[index] += 1
        }
        return counts.enumerated().map { index, binCount in
            DashboardBin(id: index, lower: minimum + Double(index) * width,
                         upper: index == count - 1 ? maximum : minimum + Double(index + 1) * width, count: binCount)
        }
    }

    /// UTC buckets: hourly up to two days, daily up to 120 days, then whole weeks.
    /// Longer histories use a multiple of weeks so there are at most 120 possible bucket intervals.
    /// Only buckets containing calls are emitted; missing timing remains nil, never a fake zero.
    private static func makeTrends(_ records: [RequestRecord]) -> [DashboardTrend] {
        guard let first = records.last?.startedAt, let last = records.first?.startedAt else { return [] }
        let span = last.timeIntervalSince(first)
        let interval: Double
        let anchor: Double
        if span <= 48 * 3600 {
            interval = 3600
            anchor = 0
        } else if span <= 119 * 86400 {
            interval = 86400
            anchor = 0
        } else {
            let weeks = max(1, ceil((span / (7 * 86400) + 1) / 119))
            interval = weeks * 7 * 86400
            anchor = 4 * 86400 // Monday, 1970-01-05.
        }
        var buckets: [Date: [RequestRecord]] = [:]
        for record in records {
            let seconds = record.startedAt.timeIntervalSince1970
            let date = Date(timeIntervalSince1970: anchor + floor((seconds - anchor) / interval) * interval)
            buckets[date, default: []].append(record)
        }
        return buckets.keys.sorted().map { DashboardTrend(date: $0, summary: DashboardSummary(records: buckets[$0]!)) }
    }
}

/// Shared by streaming history and reports: the first occurrence of a stable source key wins.
/// Records without a nonempty source key fall back to their UUID.
enum DashboardRecordKey: Hashable {
    case source(String)
    case id(UUID)

    init(_ record: RequestRecord) {
        if let key = record.sourceKey, !key.isEmpty {
            self = .source(key)
        } else {
            self = .id(record.id)
        }
    }
}

fileprivate struct DashboardMeasurements: Sendable {
    let count: Int
    var latency: [Double] = []
    var speed: [Double] = []
    var outputTokens = 0
    var estimatedCount = 0
    var interruptedCount = 0
    var roundTripCount = 0

    init(records: [RequestRecord]) {
        count = records.count
        for record in records {
            let (tokens, overflow) = outputTokens.addingReportingOverflow(max(0, record.outputTokens))
            outputTokens = overflow ? Int.max : tokens
            if record.tokensEstimated { estimatedCount += 1 }
            if record.aborted || record.status >= 400 {
                interruptedCount += 1
                continue
            }
            if let value = record.ttft, value.isFinite, value >= 0 { latency.append(value) }
            guard let value = record.tps, value.isFinite, value >= 0 else { continue }
            if let generation = record.generation, generation.isFinite, generation > 0 {
                speed.append(value)
            } else if (record.generation == nil || record.generation == 0), record.total.isFinite, record.total > 0 {
                roundTripCount += 1
            }
        }
    }
}
