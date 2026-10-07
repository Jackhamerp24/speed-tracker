import Foundation

/// Wire format of the upstream response, detected from the stream itself.
public enum APIFormat: String, Codable, Sendable {
    case anthropic
    case openAIChat = "openai-chat"
    case openAIResponses = "openai-responses"
    case gemini
    case ollama
    case unknown
}

/// One completed model call. This is what gets appended to `history.jsonl`,
/// so keep it additive: the dashboard will read old files.
public struct RequestRecord: Codable, Identifiable, Sendable, Equatable {
    public var id: UUID
    public var startedAt: Date
    public var harness: String
    public var route: String
    public var upstreamHost: String
    public var format: APIFormat
    public var model: String
    public var streamed: Bool
    public var status: Int
    /// Seconds from request sent to the first generated token of any kind (thinking included).
    public var ttft: Double?
    /// Seconds from request sent to the first visible answer token (text or tool call).
    public var firstVisible: Double?
    /// Seconds from request sent to response headers.
    public var ttfb: Double?
    /// Seconds from first to last generated token.
    public var generation: Double?
    /// Seconds from request sent to the end of the response.
    public var total: Double
    public var inputTokens: Int?
    public var cachedInputTokens: Int?
    public var outputTokens: Int
    public var reasoningTokens: Int?
    /// True when the provider reported no usage and `outputTokens` comes from character counts.
    public var tokensEstimated: Bool
    /// Output tokens per second over the generation window.
    public var tps: Double?
    /// The client hung up before the stream ended.
    public var aborted: Bool
    /// How the call was observed: "log" (harness session log), "network" (traffic timing only) or "proxy".
    public var source: String?
    /// Stable identity of the call in its source, used to avoid recording it twice.
    public var sourceKey: String?

    public init(
        id: UUID, startedAt: Date, harness: String, route: String, upstreamHost: String,
        format: APIFormat, model: String, streamed: Bool, status: Int, ttft: Double?,
        firstVisible: Double?, ttfb: Double?, generation: Double?, total: Double,
        inputTokens: Int?, cachedInputTokens: Int?, outputTokens: Int, reasoningTokens: Int?,
        tokensEstimated: Bool, tps: Double?, aborted: Bool, source: String? = nil, sourceKey: String? = nil
    ) {
        self.id = id
        self.startedAt = startedAt
        self.harness = harness
        self.route = route
        self.upstreamHost = upstreamHost
        self.format = format
        self.model = model
        self.streamed = streamed
        self.status = status
        self.ttft = ttft
        self.firstVisible = firstVisible
        self.ttfb = ttfb
        self.generation = generation
        self.total = total
        self.inputTokens = inputTokens
        self.cachedInputTokens = cachedInputTokens
        self.outputTokens = outputTokens
        self.reasoningTokens = reasoningTokens
        self.tokensEstimated = tokensEstimated
        self.tps = tps
        self.aborted = aborted
        self.source = source
        self.sourceKey = sourceKey
    }
}

/// What a detector tells the UI about a call, from first sight to completion.
public enum TrackerEvent: Sendable {
    case started(LiveSnapshot)
    case updated(LiveSnapshot)
    /// A nil record means the call turned out not to be a generation, or is recorded by another source.
    case finished(UUID, RequestRecord?)
}

/// State of a call that is still in flight. All `*Uptime` values share the
/// `ProcessInfo.systemUptime` clock so the UI can tick against them.
public struct LiveSnapshot: Identifiable, Sendable, Equatable {
    public enum Phase: Sendable { case waiting, thinking, streaming }

    public var id: UUID
    public var startedAt: Date
    public var startUptime: TimeInterval
    public var harness: String
    public var route: String
    public var model: String
    public var firstTokenUptime: TimeInterval?
    public var firstCharUptime: TimeInterval?
    public var firstVisibleUptime: TimeInterval?
    public var lastTokenUptime: TimeInterval?
    /// Running estimate from streamed characters; the provider's count replaces it at the end.
    public var estimatedTokens: Double

    public init(
        id: UUID, startedAt: Date, startUptime: TimeInterval, harness: String, route: String,
        model: String, firstTokenUptime: TimeInterval?, firstCharUptime: TimeInterval?,
        firstVisibleUptime: TimeInterval?, lastTokenUptime: TimeInterval?, estimatedTokens: Double
    ) {
        self.id = id
        self.startedAt = startedAt
        self.startUptime = startUptime
        self.harness = harness
        self.route = route
        self.model = model
        self.firstTokenUptime = firstTokenUptime
        self.firstCharUptime = firstCharUptime
        self.firstVisibleUptime = firstVisibleUptime
        self.lastTokenUptime = lastTokenUptime
        self.estimatedTokens = estimatedTokens
    }

    public var phase: Phase {
        if firstTokenUptime == nil { return .waiting }
        if firstVisibleUptime == nil { return .thinking }
        return .streaming
    }

    public var ttft: Double? { firstTokenUptime.map { $0 - startUptime } }
}

public enum Clock {
    public static var now: TimeInterval { ProcessInfo.processInfo.systemUptime }
}
