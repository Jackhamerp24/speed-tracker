import Foundation

/// Reads a model response as it streams past and works out when the first
/// token arrived and how fast the rest followed.
///
/// It understands Anthropic Messages, OpenAI Chat Completions (which covers
/// DeepSeek, OpenRouter, Groq and most gateways), OpenAI Responses, Gemini and
/// Ollama. The format is detected from the events, not from the URL, so a
/// provider serving several formats needs no configuration.
public struct StreamMeter {
    public enum BodyKind: Sendable { case eventStream, ndjson, json, other }
    private enum Mode { case undecided, lines, whole, ignore }
    private enum Kind { case text, reasoning, tool }

    public struct Result: Equatable, Sendable {
        public var outputTokens: Int
        public var tokensEstimated: Bool
        public var ttft: Double?
        public var firstVisible: Double?
        public var generation: Double?
        public var tps: Double?
    }

    public private(set) var format: APIFormat = .unknown
    public private(set) var model: String?
    public private(set) var inputTokens: Int?
    public private(set) var cachedInputTokens: Int?
    public private(set) var reportedOutputTokens: Int?
    public private(set) var reportedReasoningTokens: Int?
    public private(set) var textChars = 0
    public private(set) var reasoningChars = 0
    public private(set) var toolChars = 0
    /// Timed token events seen so far. Too few and a stream rate is meaningless.
    public private(set) var signalCount = 0
    public private(set) var sawReasoningSignal = false
    public private(set) var firstTokenAt: TimeInterval?
    public private(set) var firstCharAt: TimeInterval?
    public private(set) var firstVisibleAt: TimeInterval?
    public private(set) var lastTokenAt: TimeInterval?
    public private(set) var finished = false
    public private(set) var errorMessage: String?
    /// Characters per token used for the live estimate. The proxy calibrates this per model.
    public var charsPerToken: Double

    private var mode: Mode = .undecided
    private let kind: BodyKind
    private var pending = Data()
    private static let maxWholeBody = 16 * 1024 * 1024

    public init(kind: BodyKind, charsPerToken: Double = 4.0) {
        self.kind = kind
        self.charsPerToken = charsPerToken
    }

    public var streamed: Bool { mode == .lines }
    public var totalChars: Int { textChars + reasoningChars + toolChars }
    public var estimatedTokens: Double { Double(totalChars) / max(charsPerToken, 0.5) }

    // MARK: Feeding

    public mutating func ingest(_ data: Data, at now: TimeInterval) {
        if mode == .undecided { decideMode(peeking: data) }
        switch mode {
        case .lines:
            pending.append(data)
            drainLines(at: now)
        case .whole:
            if pending.count + data.count <= Self.maxWholeBody {
                pending.append(data)
            } else {
                pending.removeAll()
                mode = .ignore
            }
        case .ignore, .undecided:
            break
        }
    }

    /// Call once the response has ended.
    public mutating func finish(at now: TimeInterval) {
        switch mode {
        case .lines:
            if !pending.isEmpty {
                pending.append(0x0A)
                drainLines(at: now)
            }
        case .whole:
            if let json = try? JSONSerialization.jsonObject(with: pending) {
                if let object = json as? [String: Any] {
                    handle(object, at: nil)
                } else if let array = json as? [[String: Any]] {
                    array.forEach { handle($0, at: nil) }
                }
            }
            pending.removeAll()
        case .ignore, .undecided:
            break
        }
    }

    public func result(startedAt: TimeInterval, endedAt: TimeInterval) -> Result {
        let estimate = Int(estimatedTokens.rounded())
        let reported = reportedOutputTokens.flatMap { $0 > 0 ? $0 : nil }
        let output = reported ?? estimate
        var result = Result(
            outputTokens: output, tokensEstimated: reported == nil,
            ttft: nil, firstVisible: nil, generation: nil, tps: nil
        )
        let total = max(endedAt - startedAt, 0.001)
        guard output > 0 else { return result }

        guard streamed, let first = firstTokenAt, let last = lastTokenAt else {
            // Not streamed: all we can time is the whole round trip.
            result.tps = Double(output) / total
            return result
        }
        result.ttft = first - startedAt
        result.firstVisible = firstVisibleAt.map { $0 - startedAt }
        let generation = last - first
        result.generation = generation

        var generated = Double(output)
        // Reasoning that was billed but never showed up in the stream happened
        // before the first token, so it does not belong in the stream rate.
        if !sawReasoningSignal, let hidden = reportedReasoningTokens, hidden > 0, hidden < output {
            generated -= Double(hidden)
        }
        if signalCount >= 3, generation >= 0.05 {
            result.tps = generated / generation
        } else {
            result.tps = Double(output) / total
        }
        return result
    }

    // MARK: Framing

    private mutating func decideMode(peeking data: Data) {
        guard let first = data.first(where: { !Self.isWhitespace($0) }) else { return }
        switch kind {
        case .eventStream, .ndjson:
            mode = .lines
        case .json:
            // Some gateways label SSE as application/json.
            mode = (first == UInt8(ascii: "{") || first == UInt8(ascii: "[")) ? .whole : .lines
        case .other:
            let looksLikeSSE = first == UInt8(ascii: "d") || first == UInt8(ascii: "e") || first == UInt8(ascii: ":")
            mode = looksLikeSSE ? .lines : .ignore
        }
    }

    private static func isWhitespace(_ byte: UInt8) -> Bool {
        byte == 0x20 || byte == 0x0A || byte == 0x0D || byte == 0x09
    }

    private mutating func drainLines(at now: TimeInterval) {
        var start = pending.startIndex
        while let newline = pending[start...].firstIndex(of: 0x0A) {
            var line = pending[start..<newline]
            if line.last == 0x0D { line = line.dropLast() }
            if !line.isEmpty { handleLine(Data(line), at: now) }
            start = pending.index(after: newline)
        }
        pending = Data(pending[start...])
        if pending.count > Self.maxWholeBody {
            pending.removeAll()
            mode = .ignore
        }
    }

    private static let dataPrefix = Data("data:".utf8)

    private mutating func handleLine(_ line: Data, at now: TimeInterval) {
        var payload: Data
        if line.starts(with: Self.dataPrefix) {
            payload = Data(line.dropFirst(Self.dataPrefix.count))
            while payload.first == 0x20 { payload = Data(payload.dropFirst()) }
        } else if line.first == UInt8(ascii: "{") {
            payload = line
        } else {
            return
        }
        if payload.starts(with: Data("[DONE]".utf8)) {
            finished = true
            return
        }
        guard payload.first == UInt8(ascii: "{"),
              let object = try? JSONSerialization.jsonObject(with: payload) as? [String: Any]
        else { return }
        handle(object, at: now)
    }

    // MARK: Events

    /// `now` is nil for a body that arrived in one piece: its text still counts, but it carries no timing.
    private mutating func handle(_ object: [String: Any], at now: TimeInterval?) {
        if let type = object["type"] as? String {
            if type.hasPrefix("response.") {
                handleResponsesEvent(type, object, at: now)
            } else {
                handleAnthropic(type, object, at: now)
            }
        } else if object["choices"] != nil {
            handleChat(object, at: now)
        } else if object["candidates"] != nil || object["usageMetadata"] != nil {
            handleGemini(object, at: now)
        } else if object["done"] != nil {
            handleOllama(object, at: now)
        } else if object["object"] as? String == "response" {
            format = .openAIResponses
            absorbResponsesObject(object)
        } else if let error = object["error"] {
            noteError(error)
        }
    }

    private mutating func handleAnthropic(_ type: String, _ object: [String: Any], at now: TimeInterval?) {
        switch type {
        case "message_start":
            format = .anthropic
            if let message = object["message"] as? [String: Any] {
                if let name = message["model"] as? String { model = name }
                // The output count here is a placeholder; the real one comes in message_delta.
                absorbAnthropicUsage(message["usage"], includingOutput: false)
            }
        case "content_block_start":
            // The block opening is the first sign of generation, and the only
            // one before the answer when thinking is hidden.
            guard let block = object["content_block"] as? [String: Any] else { return }
            let blockType = block["type"] as? String ?? ""
            if blockType.contains("thinking") {
                sawReasoningSignal = true
                mark(.reasoning, chars: 0, at: now)
            } else if blockType.contains("tool_use") {
                mark(.tool, chars: (block["name"] as? String)?.count ?? 0, at: now)
            } else {
                mark(.text, chars: (block["text"] as? String)?.count ?? 0, at: now, visible: false)
            }
        case "content_block_delta":
            guard let delta = object["delta"] as? [String: Any] else { return }
            if let text = delta["text"] as? String {
                mark(.text, chars: text.count, at: now)
            } else if let thinking = delta["thinking"] as? String {
                mark(.reasoning, chars: thinking.count, at: now)
            } else if let json = delta["partial_json"] as? String {
                mark(.tool, chars: json.count, at: now)
            }
        case "message_delta":
            absorbAnthropicUsage(object["usage"])
        case "message_stop":
            finished = true
        case "message":
            format = .anthropic
            if let name = object["model"] as? String { model = name }
            absorbAnthropicUsage(object["usage"])
            for block in object["content"] as? [[String: Any]] ?? [] {
                if let text = block["text"] as? String { mark(.text, chars: text.count, at: now) }
                if let thinking = block["thinking"] as? String { mark(.reasoning, chars: thinking.count, at: now) }
                if let input = block["input"] { mark(.tool, chars: Self.jsonLength(input), at: now) }
            }
        case "error":
            noteError(object["error"] as Any)
        default:
            break
        }
    }

    private mutating func absorbAnthropicUsage(_ value: Any?, includingOutput: Bool = true) {
        guard let usage = value as? [String: Any] else { return }
        let fresh = Self.int(usage["input_tokens"])
        let cacheRead = Self.int(usage["cache_read_input_tokens"])
        let cacheWrite = Self.int(usage["cache_creation_input_tokens"])
        if fresh != nil || cacheRead != nil || cacheWrite != nil {
            let total = (fresh ?? 0) + (cacheRead ?? 0) + (cacheWrite ?? 0)
            inputTokens = max(inputTokens ?? 0, total)
            if let cacheRead { cachedInputTokens = cacheRead }
        }
        if includingOutput, let output = Self.int(usage["output_tokens"]) {
            reportedOutputTokens = max(reportedOutputTokens ?? 0, output)
        }
    }

    private mutating func handleResponsesEvent(_ type: String, _ object: [String: Any], at now: TimeInterval?) {
        format = .openAIResponses
        if let response = object["response"] as? [String: Any] { absorbResponsesObject(response) }

        if type == "response.output_item.added", let item = object["item"] as? [String: Any] {
            let itemType = item["type"] as? String ?? ""
            if itemType == "reasoning" {
                // Encrypted reasoning streams nothing, so its start is all we get.
                sawReasoningSignal = true
                mark(.reasoning, chars: 0, at: now)
            } else if itemType.contains("call") {
                mark(.tool, chars: (item["name"] as? String)?.count ?? 0, at: now)
            }
        } else if type.hasSuffix(".delta"), let delta = object["delta"] as? String {
            if type.contains("audio") && !type.contains("transcript") { return }
            if type.contains("reasoning") {
                sawReasoningSignal = true
                mark(.reasoning, chars: delta.count, at: now)
            } else if type.contains("call") || type.contains("tool") {
                mark(.tool, chars: delta.count, at: now)
            } else {
                mark(.text, chars: delta.count, at: now)
            }
        } else if type == "response.completed" || type == "response.incomplete" || type == "response.done" {
            finished = true
        } else if type == "response.failed" {
            finished = true
            if let response = object["response"] as? [String: Any], let error = response["error"] { noteError(error) }
        }
    }

    private mutating func absorbResponsesObject(_ response: [String: Any]) {
        if let name = response["model"] as? String, !name.isEmpty { model = name }
        guard let usage = response["usage"] as? [String: Any] else { return }
        if let input = Self.int(usage["input_tokens"]) { inputTokens = input }
        if let output = Self.int(usage["output_tokens"]) { reportedOutputTokens = output }
        if let details = usage["input_tokens_details"] as? [String: Any],
           let cached = Self.int(details["cached_tokens"]) { cachedInputTokens = cached }
        if let details = usage["output_tokens_details"] as? [String: Any],
           let reasoning = Self.int(details["reasoning_tokens"]) { reportedReasoningTokens = reasoning }
    }

    private mutating func handleChat(_ object: [String: Any], at now: TimeInterval?) {
        format = .openAIChat
        if let name = object["model"] as? String, !name.isEmpty { model = name }
        for choice in object["choices"] as? [[String: Any]] ?? [] {
            if let text = choice["text"] as? String { mark(.text, chars: text.count, at: now) }
            guard let delta = (choice["delta"] ?? choice["message"]) as? [String: Any] else { continue }
            if let reasoning = (delta["reasoning_content"] ?? delta["reasoning"]) as? String, !reasoning.isEmpty {
                sawReasoningSignal = true
                mark(.reasoning, chars: reasoning.count, at: now)
            }
            if let content = delta["content"] as? String {
                mark(.text, chars: content.count, at: now)
            }
            for call in delta["tool_calls"] as? [[String: Any]] ?? [] {
                guard let function = call["function"] as? [String: Any] else { continue }
                let chars = ((function["name"] as? String)?.count ?? 0) + ((function["arguments"] as? String)?.count ?? 0)
                mark(.tool, chars: chars, at: now)
            }
        }
        if let usage = object["usage"] as? [String: Any] {
            if let input = Self.int(usage["prompt_tokens"]) { inputTokens = input }
            if let output = Self.int(usage["completion_tokens"]) { reportedOutputTokens = output }
            if let details = usage["completion_tokens_details"] as? [String: Any],
               let reasoning = Self.int(details["reasoning_tokens"]) { reportedReasoningTokens = reasoning }
            if let details = usage["prompt_tokens_details"] as? [String: Any],
               let cached = Self.int(details["cached_tokens"]) { cachedInputTokens = cached }
            if let cached = Self.int(usage["prompt_cache_hit_tokens"]) { cachedInputTokens = cached }
        }
        if object["error"] != nil { noteError(object["error"] as Any) }
    }

    private mutating func handleGemini(_ object: [String: Any], at now: TimeInterval?) {
        format = .gemini
        if let name = object["modelVersion"] as? String { model = name }
        for candidate in object["candidates"] as? [[String: Any]] ?? [] {
            guard let content = candidate["content"] as? [String: Any] else { continue }
            for part in content["parts"] as? [[String: Any]] ?? [] {
                if let text = part["text"] as? String {
                    if part["thought"] as? Bool == true {
                        sawReasoningSignal = true
                        mark(.reasoning, chars: text.count, at: now)
                    } else {
                        mark(.text, chars: text.count, at: now)
                    }
                } else if let call = part["functionCall"] {
                    mark(.tool, chars: Self.jsonLength(call), at: now)
                }
            }
        }
        if let usage = object["usageMetadata"] as? [String: Any] {
            if let input = Self.int(usage["promptTokenCount"]) { inputTokens = input }
            if let cached = Self.int(usage["cachedContentTokenCount"]) { cachedInputTokens = cached }
            let thoughts = Self.int(usage["thoughtsTokenCount"])
            if let thoughts { reportedReasoningTokens = thoughts }
            if let candidates = Self.int(usage["candidatesTokenCount"]) {
                reportedOutputTokens = candidates + (thoughts ?? 0)
            }
        }
    }

    private mutating func handleOllama(_ object: [String: Any], at now: TimeInterval?) {
        format = .ollama
        if let name = object["model"] as? String { model = name }
        if let message = object["message"] as? [String: Any] {
            if let thinking = message["thinking"] as? String, !thinking.isEmpty {
                sawReasoningSignal = true
                mark(.reasoning, chars: thinking.count, at: now)
            }
            if let content = message["content"] as? String { mark(.text, chars: content.count, at: now) }
        }
        if let response = object["response"] as? String { mark(.text, chars: response.count, at: now) }
        if let input = Self.int(object["prompt_eval_count"]) { inputTokens = input }
        if let output = Self.int(object["eval_count"]) { reportedOutputTokens = output }
        if object["done"] as? Bool == true { finished = true }
    }

    // MARK: Bookkeeping

    /// Records generation activity. Zero-character events (a block opening) count
    /// for timing only. `visible` is false for events that prove generation
    /// started without yet showing the user anything.
    private mutating func mark(_ kind: Kind, chars: Int, at now: TimeInterval?, visible: Bool = true) {
        switch kind {
        case .text: textChars += chars
        case .reasoning: reasoningChars += chars
        case .tool: toolChars += chars
        }
        guard let now else { return }
        if kind == .reasoning || chars > 0 || !visible {
            if firstTokenAt == nil { firstTokenAt = now }
            lastTokenAt = now
            signalCount += 1
        }
        guard chars > 0 else { return }
        if firstCharAt == nil { firstCharAt = now }
        if kind != .reasoning, visible, firstVisibleAt == nil { firstVisibleAt = now }
    }

    private mutating func noteError(_ value: Any) {
        if let dictionary = value as? [String: Any] {
            errorMessage = dictionary["message"] as? String ?? dictionary["type"] as? String ?? "error"
        } else if let text = value as? String {
            errorMessage = text
        }
    }

    private static func int(_ value: Any?) -> Int? {
        (value as? NSNumber)?.intValue
    }

    private static func jsonLength(_ value: Any) -> Int {
        guard JSONSerialization.isValidJSONObject(value),
              let data = try? JSONSerialization.data(withJSONObject: value) else { return 0 }
        return data.count
    }
}
