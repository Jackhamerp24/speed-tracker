import Foundation

/// One model response as a harness wrote it to its own session log.
public struct LogRecord: Equatable, Sendable {
    /// Stable across restarts, so a response is never recorded twice.
    public var key: String
    public var harness: String
    public var model: String
    public var provider: String
    /// When the harness sent the request. Nil when the log does not say.
    public var requestStart: Date?
    /// When generation began, thinking included. Nil when the log does not say.
    public var firstToken: Date?
    public var firstVisible: Date?
    public var end: Date
    public var outputTokens: Int
    public var reasoningTokens: Int?
    public var inputTokens: Int?
    public var cachedInputTokens: Int?
    public var aborted: Bool
    /// Tokens per second as the harness's own measurements give it, for a log whose timing
    /// does not fit "all output tokens between first token and end". Nil uses that rule.
    public var speed: Double?

    public init(
        key: String, harness: String, model: String, provider: String = "", requestStart: Date?,
        firstToken: Date?, firstVisible: Date? = nil, end: Date, outputTokens: Int,
        reasoningTokens: Int? = nil, inputTokens: Int? = nil, cachedInputTokens: Int? = nil, aborted: Bool = false,
        speed: Double? = nil
    ) {
        self.key = key
        self.harness = harness
        self.model = model
        self.provider = provider
        self.requestStart = requestStart
        self.firstToken = firstToken
        self.firstVisible = firstVisible
        self.end = end
        self.outputTokens = outputTokens
        self.reasoningTokens = reasoningTokens
        self.inputTokens = inputTokens
        self.cachedInputTokens = cachedInputTokens
        self.aborted = aborted
        self.speed = speed
    }

    /// Turns the log entry into a stored record. `observed` supplies request and
    /// first-byte times seen on the network when the log has no first-token time.
    public func makeRecord(observed: (requestStart: Date, firstByte: Date)? = nil) -> RequestRecord {
        var start = requestStart
        var first = firstToken
        if first == nil, let observed {
            // The log's own request time is the better one when it has it.
            start = start ?? observed.requestStart
            first = observed.firstByte
        }
        var ttft: Double?
        var generation: Double?
        var tps: Double?
        if let start, let first {
            let value = first.timeIntervalSince(start)
            if value >= 0, value < 3600 { ttft = value }
        }
        // Some logs timestamp generation but not request dispatch; TPS is still measurable.
        if let first, start == nil || ttft != nil {
            let window = end.timeIntervalSince(first)
            generation = max(window, 0)
            if window >= 0.05 { tps = Double(outputTokens) / window }
        }
        let total = start.map { max(end.timeIntervalSince($0), 0) } ?? generation ?? 0
        if tps == nil, total >= 0.05 {
            // No first-token time: the best available is the whole round trip.
            tps = Double(outputTokens) / total
        }
        if let speed, speed.isFinite, speed > 0 { tps = speed }
        return RequestRecord(
            id: UUID(), startedAt: start ?? first ?? end, harness: harness, route: provider,
            upstreamHost: provider, format: .unknown, model: model, streamed: true, status: 200,
            ttft: ttft, firstVisible: firstVisible.flatMap { visible in start.map { visible.timeIntervalSince($0) } },
            ttfb: nil, generation: generation, total: total, inputTokens: inputTokens,
            cachedInputTokens: cachedInputTokens, outputTokens: outputTokens, reasoningTokens: reasoningTokens,
            tokensEstimated: false, tps: tps, aborted: aborted, source: "log", sourceKey: key
        )
    }
}

/// Reads one session file's entries in order and reports each finished response.
public protocol SessionLogParser: AnyObject {
    var harness: String { get }
    /// The model the session is using right now, for labelling a call still in flight.
    var currentModel: String? { get }
    /// A request has been logged and its response has not finished yet.
    var awaitingResponse: Bool { get }
    /// When the harness last sent a request, by its own log.
    var lastRequestAt: Date? { get }
    func ingest(_ entry: [String: Any]) -> [LogRecord]
    /// Called when the file has been quiet for `seconds`; emits anything held back.
    func flush(idleFor seconds: TimeInterval) -> [LogRecord]
}

enum LogDates {
    private static let fractional: ISO8601DateFormatter = {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter
    }()

    private static let whole = ISO8601DateFormatter()

    static func parse(_ value: Any?) -> Date? {
        if let text = value as? String {
            return fractional.date(from: text) ?? whole.date(from: text)
        }
        if let number = value as? NSNumber {
            return milliseconds(number.doubleValue)
        }
        return nil
    }

    static func milliseconds(_ value: Double) -> Date? {
        value > 0 ? Date(timeIntervalSince1970: value / 1000) : nil
    }
}

private func integer(_ value: Any?) -> Int? {
    (value as? NSNumber)?.intValue
}

// MARK: - Claude Code

/// `~/.claude/projects/<project>/<session>.jsonl`.
///
/// Each content block of a response is its own line, stamped when the block
/// finished. A thinking block also carries how long it took, which gives the
/// moment generation began. A response that opens with text or a tool call has
/// no such marker, so its first-token time has to come from the network.
public final class ClaudeCodeLogParser: SessionLogParser {
    private struct Pending {
        var id: String
        var model: String
        var requestStart: Date?
        var firstToken: Date?
        var end: Date
        var output = 0
        var input: Int?
        var cached: Int?
    }

    public let harness = "Claude Code"
    public private(set) var currentModel: String?
    public private(set) var awaitingResponse = false
    public var lastRequestAt: Date? { lastInput[false] }
    /// Subagent turns interleave with the main thread, so each keeps its own state.
    private var lastInput: [Bool: Date] = [:]
    private var pending: [Bool: Pending] = [:]

    public init() {}

    public func ingest(_ entry: [String: Any]) -> [LogRecord] {
        guard let type = entry["type"] as? String else { return [] }
        let sidechain = entry["isSidechain"] as? Bool ?? false
        switch type {
        case "user":
            if let time = LogDates.parse(entry["timestamp"]) { lastInput[sidechain] = time }
            if !sidechain { awaitingResponse = true }
            return []
        case "system":
            if !sidechain { awaitingResponse = false }
            return finalize(sidechain)
        case "assistant":
            guard let time = LogDates.parse(entry["timestamp"]),
                  let message = entry["message"] as? [String: Any],
                  let id = message["id"] as? String,
                  let model = message["model"] as? String, model != "<synthetic>"
            else { return [] }

            var finished: [LogRecord] = []
            if let previous = pending[sidechain], previous.id != id { finished = finalize(sidechain) }
            let isFirstLine = pending[sidechain] == nil
            var current = pending[sidechain]
                ?? Pending(id: id, model: model, requestStart: lastInput[sidechain], end: time)
            current.end = max(current.end, time)
            if !sidechain {
                currentModel = model
                awaitingResponse = false
            }

            if let usage = message["usage"] as? [String: Any] {
                current.output = max(current.output, integer(usage["output_tokens"]) ?? 0)
                let cacheRead = integer(usage["cache_read_input_tokens"])
                let total = (integer(usage["input_tokens"]) ?? 0) + (cacheRead ?? 0)
                    + (integer(usage["cache_creation_input_tokens"]) ?? 0)
                if total > 0 { current.input = total }
                if let cacheRead { current.cached = cacheRead }
            }
            let firstBlock = (message["content"] as? [[String: Any]])?.first?["type"] as? String
            if isFirstLine, firstBlock == "thinking", let duration = (entry["thinkingDurationMs"] as? NSNumber)?.doubleValue {
                current.firstToken = time.addingTimeInterval(-duration / 1000)
            }
            pending[sidechain] = current
            return finished
        default:
            return []
        }
    }

    public func flush(idleFor seconds: TimeInterval) -> [LogRecord] {
        guard seconds >= 3 else { return [] }
        return finalize(false) + finalize(true)
    }

    private func finalize(_ sidechain: Bool) -> [LogRecord] {
        guard let done = pending.removeValue(forKey: sidechain), done.output > 0 else { return [] }
        var firstToken = done.firstToken
        if let start = done.requestStart, let first = firstToken, first < start { firstToken = nil }
        return [LogRecord(
            key: "claude:\(done.id)", harness: harness, model: done.model, provider: "anthropic",
            requestStart: done.requestStart, firstToken: firstToken, end: done.end,
            outputTokens: done.output, inputTokens: done.input, cachedInputTokens: done.cached
        )]
    }
}

// MARK: - Codex

/// `~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl`.
///
/// A response ends with a `token_usage_record`. The reasoning and message
/// items inside it log when they started, so the earliest of those is the
/// first token. The request went out when the user message or the last tool
/// output before it was written.
public final class CodexLogParser: SessionLogParser {
    public let harness = "Codex"
    public private(set) var currentModel: String?
    public var awaitingResponse: Bool { requestStart != nil }
    public private(set) var lastRequestAt: Date?
    private var provider = "openai"
    private var requestStart: Date?
    private var firstToken: Date?
    private var firstVisible: Date?
    private var sawUsageRecord = false
    private var lastTokenCountTotal: Int?
    private var fallbackIndex = 0
    private var sessionID = UUID().uuidString

    public init() {}

    public func ingest(_ entry: [String: Any]) -> [LogRecord] {
        guard let type = entry["type"] as? String else { return [] }
        let payload = entry["payload"] as? [String: Any] ?? [:]
        let time = LogDates.parse(entry["timestamp"])
        switch type {
        case "session_meta":
            if let id = (payload["session_id"] ?? payload["id"]) as? String { sessionID = id }
            if let name = payload["model_provider"] as? String, !name.isEmpty { provider = name }
        case "turn_context":
            if let model = payload["model"] as? String { currentModel = model }
        case "response_item":
            let kind = payload["type"] as? String ?? ""
            let isUserMessage = kind == "message" && payload["role"] as? String == "user"
            if isUserMessage || kind.hasSuffix("_output") {
                requestStart = time
                lastRequestAt = time
                firstToken = nil
                firstVisible = nil
            }
        case "event_msg":
            let kind = payload["type"] as? String ?? ""
            if kind == "item_completed" {
                noteItem(payload)
            } else if kind == "task_complete" || kind == "turn_aborted" {
                requestStart = nil
            } else if kind == "token_count", !sawUsageRecord,
                      let info = payload["info"] as? [String: Any],
                      let usage = info["last_token_usage"] as? [String: Any] {
                // Older builds have no usage record. This event can repeat, so skip unchanged totals.
                let total = integer((info["total_token_usage"] as? [String: Any])?["total_tokens"])
                guard total != lastTokenCountTotal else { return [] }
                lastTokenCountTotal = total
                fallbackIndex += 1
                return emit(usage: usage, key: "codex:\(sessionID):\(fallbackIndex)", at: time)
            }
        case "token_usage_record":
            sawUsageRecord = true
            guard let usage = payload["usage"] as? [String: Any] else { return [] }
            fallbackIndex += 1
            let id = payload["response_id"] as? String ?? "\(sessionID):\(fallbackIndex)"
            return emit(usage: usage, key: "codex:\(id)", at: time)
        default:
            break
        }
        return []
    }

    public func flush(idleFor seconds: TimeInterval) -> [LogRecord] { [] }

    private func noteItem(_ payload: [String: Any]) {
        guard let item = payload["item"] as? [String: Any],
              let kind = (item["type"] as? String)?.lowercased(),
              let started = (payload["started_at_ms"] as? NSNumber).flatMap({ LogDates.milliseconds($0.doubleValue) })
        else { return }
        let isReasoning = kind == "reasoning"
        let isMessage = kind == "agentmessage" || kind == "agent_message"
        guard isReasoning || isMessage else { return }
        if let start = requestStart, started < start.addingTimeInterval(-0.5) { return }
        firstToken = min(firstToken ?? started, started)
        if isMessage { firstVisible = min(firstVisible ?? started, started) }
    }

    private func emit(usage: [String: Any], key: String, at time: Date?) -> [LogRecord] {
        defer {
            requestStart = nil
            firstToken = nil
            firstVisible = nil
        }
        guard let time, let output = integer(usage["output_tokens"]), output > 0 else { return [] }
        return [LogRecord(
            key: key, harness: harness, model: currentModel ?? "unknown", provider: provider,
            requestStart: requestStart, firstToken: firstToken, firstVisible: firstVisible, end: time,
            outputTokens: output, reasoningTokens: integer(usage["reasoning_output_tokens"]),
            inputTokens: integer(usage["input_tokens"]), cachedInputTokens: integer(usage["cached_input_tokens"])
        )]
    }
}

// MARK: - OMP / Pi

/// `~/.omp/agent/sessions/<project>/<session>.jsonl`.
///
/// OMP measures each response itself: the assistant entry carries `ttft` and
/// `duration` in milliseconds next to the token usage, so nothing is inferred.
public final class OMPLogParser: SessionLogParser {
    public let harness: String
    public private(set) var currentModel: String?
    public private(set) var awaitingResponse = false
    public private(set) var lastRequestAt: Date?

    public init(harness: String = "OMP") {
        self.harness = harness
    }

    public func ingest(_ entry: [String: Any]) -> [LogRecord] {
        guard let type = entry["type"] as? String else { return [] }
        if type == "model_change" {
            if let model = entry["model"] as? String { currentModel = model.split(separator: "/").last.map(String.init) }
            return []
        }
        guard type == "message", let message = entry["message"] as? [String: Any] else { return [] }
        let role = message["role"] as? String
        awaitingResponse = role == "user" || role == "toolResult"
        if awaitingResponse { lastRequestAt = LogDates.parse(entry["timestamp"]) }
        guard role == "assistant",
              let usage = message["usage"] as? [String: Any],
              let output = integer(usage["output"]), output > 0
        else { return [] }

        let model = message["model"] as? String ?? currentModel ?? "unknown"
        currentModel = model
        let written = LogDates.parse(entry["timestamp"])
        let duration = (message["duration"] as? NSNumber)?.doubleValue
        var start = LogDates.parse(message["timestamp"])
        if start == nil, let written, let duration { start = written.addingTimeInterval(-duration / 1000) }
        guard let end = start.flatMap({ begin in duration.map { begin.addingTimeInterval($0 / 1000) } }) ?? written else {
            return []
        }
        let ttft = (message["ttft"] as? NSNumber)?.doubleValue
        let firstToken = start.flatMap { begin in ttft.map { begin.addingTimeInterval($0 / 1000) } }
        let id = entry["id"] as? String ?? message["responseId"] as? String ?? "\(end.timeIntervalSince1970)"
        let stop = message["stopReason"] as? String ?? ""
        let cached = integer(usage["cacheRead"])
        return [LogRecord(
            key: "\(harness.lowercased()):\(id)", harness: harness, model: model,
            provider: message["provider"] as? String ?? "", requestStart: start, firstToken: firstToken, end: end,
            outputTokens: output, reasoningTokens: integer(usage["reasoningTokens"]),
            inputTokens: integer(usage["input"]).map { $0 + (cached ?? 0) + (integer(usage["cacheWrite"]) ?? 0) },
            cachedInputTokens: cached, aborted: stop == "aborted" || stop == "error"
        )]
    }

    public func flush(idleFor seconds: TimeInterval) -> [LogRecord] { [] }
}

// MARK: - Gemini CLI

/// `~/.gemini/tmp/<project>/chats/session-*.jsonl` (subagents one folder deeper).
///
/// The first line is session metadata; `{"$set": …}` lines update it. Every other
/// line is a message, and a message is written again, whole, each time it changes,
/// so the same id can appear several times.
///
/// A `gemini` message is created the moment its stream ends, so its timestamp is
/// the end of the response. Each thought summary carries the time it arrived; the
/// first is when generation began. Requests after tool calls leave no line of
/// their own: they go out when the tools finish, and each tool call records that.
public final class GeminiCLILogParser: SessionLogParser {
    public let harness = "Gemini CLI"
    public private(set) var currentModel: String?
    public private(set) var awaitingResponse = false
    public private(set) var lastRequestAt: Date?
    /// The last request time has been matched to a response; the next response
    /// needs a new one or its start is unknown.
    private var requestUsed = true
    private var emitted = Set<String>()

    public init() {}

    public func ingest(_ entry: [String: Any]) -> [LogRecord] {
        if let update = entry["$set"] as? [String: Any] {
            // A session can start with messages delivered inside its first metadata update.
            return (update["messages"] as? [[String: Any]] ?? []).flatMap(ingest(message:))
        }
        if entry["$rewindTo"] != nil {
            awaitingResponse = false
            requestUsed = true
            return []
        }
        return ingest(message: entry)
    }

    public func flush(idleFor seconds: TimeInterval) -> [LogRecord] { [] }

    private func ingest(message: [String: Any]) -> [LogRecord] {
        guard let id = message["id"] as? String, let type = message["type"] as? String,
              let time = LogDates.parse(message["timestamp"])
        else { return [] }
        switch type {
        case "user":
            noteRequest(at: time)
            return []
        case "error":
            awaitingResponse = false
            return []
        case "gemini":
            break
        default:
            return []
        }

        if let model = message["model"] as? String, !model.isEmpty { currentModel = model }
        var records: [LogRecord] = []
        if !emitted.contains(id), let record = record(id: id, message: message, end: time) {
            emitted.insert(id)
            records.append(record)
            requestUsed = true
        }

        // The message is written again once its tools have run; the next request leaves then.
        let finished = (message["toolCalls"] as? [[String: Any]] ?? []).compactMap { LogDates.parse($0["timestamp"]) }.max()
        if let finished, finished >= time {
            if finished > (lastRequestAt ?? .distantPast) { noteRequest(at: finished) }
        } else {
            awaitingResponse = false
        }
        return records
    }

    private func noteRequest(at time: Date) {
        lastRequestAt = time
        requestUsed = false
        awaitingResponse = true
    }

    private func record(id: String, message: [String: Any], end: Date) -> LogRecord? {
        // Tokens can be missing on the first write of a message and arrive with a later one.
        guard let tokens = message["tokens"] as? [String: Any] else { return nil }
        let thoughts = integer(tokens["thoughts"]) ?? 0
        // Gemini counts thinking apart from the visible reply; the response produced both.
        let output = (integer(tokens["output"]) ?? 0) + thoughts
        guard output > 0 else { return nil }

        var start: Date?
        if !requestUsed, let requested = lastRequestAt, requested <= end, end.timeIntervalSince(requested) < 3600 {
            start = requested
        }
        var firstToken = (message["thoughts"] as? [[String: Any]] ?? []).compactMap { LogDates.parse($0["timestamp"]) }.min()
        if let first = firstToken, first > end || (start.map { first < $0 } ?? false) { firstToken = nil }

        return LogRecord(
            key: "gemini:\(id)", harness: harness, model: currentModel ?? "unknown", provider: "google",
            requestStart: start, firstToken: firstToken, end: end, outputTokens: output,
            reasoningTokens: thoughts > 0 ? thoughts : nil, inputTokens: integer(tokens["input"]),
            cachedInputTokens: integer(tokens["cached"])
        )
    }
}
