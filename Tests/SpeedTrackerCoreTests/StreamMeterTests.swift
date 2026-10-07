import Foundation
import Testing
@testable import SpeedTrackerCore

/// Feeds SSE events to a meter at chosen times.
private func feed(_ meter: inout StreamMeter, _ events: [(time: TimeInterval, json: String)]) {
    for event in events {
        meter.ingest(Data("data: \(event.json)\n\n".utf8), at: event.time)
    }
}

@Suite struct StreamMeterTests {
    @Test func anthropicStream() {
        var meter = StreamMeter(kind: .eventStream)
        var events: [(TimeInterval, String)] = [
            (0.3, #"{"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":10,"cache_read_input_tokens":90,"output_tokens":1}}}"#),
            (0.5, #"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
        ]
        for step in 0..<10 {
            events.append((0.5 + Double(step) * 0.1, #"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello world "}}"#))
        }
        events.append((1.5, #"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":50}}"#))
        events.append((1.5, #"{"type":"message_stop"}"#))
        feed(&meter, events)
        meter.finish(at: 1.5)

        let result = meter.result(startedAt: 0, endedAt: 1.5)
        #expect(meter.format == .anthropic)
        #expect(meter.model == "claude-opus-5-5")
        #expect(meter.inputTokens == 100)
        #expect(meter.cachedInputTokens == 90)
        #expect(meter.finished)
        #expect(result.outputTokens == 50)
        #expect(!result.tokensEstimated)
        #expect(abs(result.ttft! - 0.5) < 0.001)
        // 50 tokens between t=0.5 and t=1.4.
        #expect(abs(result.tps! - 50 / 0.9) < 0.01)
    }

    @Test func anthropicHiddenThinkingCountsFromBlockStart() {
        var meter = StreamMeter(kind: .eventStream)
        feed(&meter, [
            (0.2, #"{"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":5}}}"#),
            (0.4, #"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#),
            (4.0, #"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#),
            (4.4, #"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#),
            (4.4, #"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Done."}}"#),
            (4.4, #"{"type":"message_delta","usage":{"output_tokens":400}}"#),
        ])
        let result = meter.result(startedAt: 0, endedAt: 4.5)
        #expect(abs(result.ttft! - 0.4) < 0.001)
        #expect(abs(result.firstVisible! - 4.4) < 0.001)
        // The thinking tokens were generated inside the 4 s window, so they count.
        #expect(abs(result.tps! - 100) < 0.01)
    }

    @Test func interruptedAnthropicStreamFallsBackToEstimate() {
        var meter = StreamMeter(kind: .eventStream, charsPerToken: 4)
        var events: [(TimeInterval, String)] = [
            (0.3, #"{"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":10,"output_tokens":1}}}"#),
        ]
        for step in 0..<10 {
            events.append((0.5 + Double(step) * 0.1, #"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"12345678"}}"#))
        }
        feed(&meter, events)
        // No message_delta: the client hung up. The placeholder count of 1 must not win.
        let result = meter.result(startedAt: 0, endedAt: 1.4)
        #expect(!meter.finished)
        #expect(result.tokensEstimated)
        #expect(result.outputTokens == 20)
    }

    @Test func chatCompletionsWithReasoningContent() {
        var meter = StreamMeter(kind: .eventStream)
        feed(&meter, [
            (0.2, #"{"model":"deepseek-reasoner","choices":[{"delta":{"role":"assistant","content":""}}]}"#),
            (0.6, #"{"model":"deepseek-reasoner","choices":[{"delta":{"reasoning_content":"Let me think"}}]}"#),
            (1.0, #"{"model":"deepseek-reasoner","choices":[{"delta":{"reasoning_content":" about it"}}]}"#),
            (1.6, #"{"model":"deepseek-reasoner","choices":[{"delta":{"content":"Answer"}}]}"#),
            (2.6, #"{"model":"deepseek-reasoner","choices":[{"delta":{"content":" here"},"finish_reason":"stop"}]}"#),
            (2.6, #"{"model":"deepseek-reasoner","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":80,"completion_tokens_details":{"reasoning_tokens":30},"prompt_cache_hit_tokens":4}}"#),
        ])
        meter.ingest(Data("data: [DONE]\n\n".utf8), at: 2.6)
        let result = meter.result(startedAt: 0, endedAt: 2.6)
        #expect(meter.format == .openAIChat)
        #expect(meter.model == "deepseek-reasoner")
        #expect(meter.finished)
        #expect(meter.cachedInputTokens == 4)
        // The empty role chunk is not a token.
        #expect(abs(result.ttft! - 0.6) < 0.001)
        #expect(abs(result.firstVisible! - 1.6) < 0.001)
        // Reasoning was streamed, so all 80 tokens fall inside the 2 s window.
        #expect(abs(result.tps! - 40) < 0.01)
    }

    @Test func chatCompletionsSubtractsUnseenReasoning() {
        var meter = StreamMeter(kind: .eventStream)
        var events: [(TimeInterval, String)] = []
        for step in 0..<5 {
            events.append((3.0 + Double(step) * 0.25, #"{"model":"o-model","choices":[{"delta":{"content":"word "}}]}"#))
        }
        events.append((4.0, #"{"model":"o-model","choices":[],"usage":{"prompt_tokens":9,"completion_tokens":300,"completion_tokens_details":{"reasoning_tokens":260}}}"#))
        feed(&meter, events)
        let result = meter.result(startedAt: 0, endedAt: 4.0)
        #expect(result.outputTokens == 300)
        // 260 reasoning tokens were spent before the first chunk; 40 streamed over 1 s.
        #expect(abs(result.tps! - 40) < 0.01)
    }

    @Test func responsesStream() {
        var meter = StreamMeter(kind: .eventStream)
        feed(&meter, [
            (0.1, #"{"type":"response.created","response":{"model":"gpt-6.1-sol"}}"#),
            (0.5, #"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#),
            (2.0, #"{"type":"response.output_item.added","item":{"type":"message"}}"#),
            (2.0, #"{"type":"response.output_text.delta","delta":"Hello"}"#),
            (2.5, #"{"type":"response.output_text.delta","delta":" there"}"#),
            (2.5, #"{"type":"response.completed","response":{"model":"gpt-6.1-sol","usage":{"input_tokens":20,"output_tokens":120,"input_tokens_details":{"cached_tokens":8},"output_tokens_details":{"reasoning_tokens":100}}}}"#),
        ])
        let result = meter.result(startedAt: 0, endedAt: 2.5)
        #expect(meter.format == .openAIResponses)
        #expect(meter.model == "gpt-6.1-sol")
        #expect(meter.finished)
        #expect(meter.cachedInputTokens == 8)
        #expect(meter.reportedReasoningTokens == 100)
        #expect(abs(result.ttft! - 0.5) < 0.001)
        #expect(abs(result.firstVisible! - 2.0) < 0.001)
        #expect(abs(result.tps! - 60) < 0.01)
    }

    @Test func eventsSplitAcrossChunksAndCRLF() {
        var meter = StreamMeter(kind: .eventStream)
        let event = "event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"abcdefgh\"}}\r\n\r\n"
        let bytes = Data(event.utf8)
        meter.ingest(bytes.prefix(40), at: 1.0)
        #expect(meter.textChars == 0)
        meter.ingest(bytes.dropFirst(40), at: 1.2)
        #expect(meter.textChars == 8)
        #expect(meter.firstTokenAt == 1.2)
    }

    @Test func estimatesTokensWhenUsageIsMissing() {
        var meter = StreamMeter(kind: .eventStream, charsPerToken: 4)
        var events: [(TimeInterval, String)] = []
        for step in 0..<10 {
            events.append((1.0 + Double(step) * 0.1, #"{"choices":[{"delta":{"content":"12345678"}}]}"#))
        }
        feed(&meter, events)
        let result = meter.result(startedAt: 0, endedAt: 2.0)
        #expect(result.tokensEstimated)
        #expect(result.outputTokens == 20)
    }

    @Test func nonStreamedJSONBody() {
        var meter = StreamMeter(kind: .json)
        let body = #"{"id":"x","type":"message","model":"claude-haiku-4-5","content":[{"type":"text","text":"Hi there"}],"usage":{"input_tokens":8,"output_tokens":40}}"#
        meter.ingest(Data(body.utf8), at: 2.0)
        meter.finish(at: 2.0)
        let result = meter.result(startedAt: 0, endedAt: 2.0)
        #expect(!meter.streamed)
        #expect(meter.model == "claude-haiku-4-5")
        #expect(result.ttft == nil)
        #expect(result.outputTokens == 40)
        #expect(abs(result.tps! - 20) < 0.01)
    }

    @Test func ollamaNDJSON() {
        var meter = StreamMeter(kind: .ndjson)
        meter.ingest(Data(#"{"model":"llama3","message":{"content":"Hi"},"done":false}"#.utf8 + [0x0A]), at: 0.4)
        meter.ingest(Data(#"{"model":"llama3","message":{"content":" all"},"done":false}"#.utf8 + [0x0A]), at: 0.6)
        meter.ingest(Data(#"{"model":"llama3","message":{"content":"!"},"done":false}"#.utf8 + [0x0A]), at: 0.9)
        meter.ingest(Data(#"{"model":"llama3","done":true,"prompt_eval_count":5,"eval_count":25}"#.utf8 + [0x0A]), at: 0.9)
        let result = meter.result(startedAt: 0, endedAt: 0.9)
        #expect(meter.format == .ollama)
        #expect(meter.finished)
        #expect(result.outputTokens == 25)
        #expect(abs(result.tps! - 50) < 0.01)
    }

    @Test func geminiStream() {
        var meter = StreamMeter(kind: .eventStream)
        feed(&meter, [
            (0.5, #"{"candidates":[{"content":{"parts":[{"text":"plan","thought":true}]}}],"modelVersion":"gemini-3-pro"}"#),
            (1.0, #"{"candidates":[{"content":{"parts":[{"text":"Hello"}]}}],"modelVersion":"gemini-3-pro"}"#),
            (1.5, #"{"candidates":[{"content":{"parts":[{"text":" world"}]}}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":20,"thoughtsTokenCount":30}}"#),
        ])
        let result = meter.result(startedAt: 0, endedAt: 1.5)
        #expect(meter.format == .gemini)
        #expect(meter.model == "gemini-3-pro")
        #expect(result.outputTokens == 50)
        #expect(abs(result.tps! - 50) < 0.01)
    }
}

@Suite struct ProxyConfigTests {
    let config = ProxyConfig()

    @Test func namedRoute() {
        let resolved = config.resolve(target: "/anthropic/v1/messages?beta=true")
        #expect(resolved?.url.absoluteString == "https://api.anthropic.com/v1/messages?beta=true")
        #expect(resolved?.route == "anthropic")
        #expect(resolved?.harnessTag == nil)
    }

    @Test func routeWithBasePath() {
        let resolved = config.resolve(target: "/chatgpt/responses")
        #expect(resolved?.url.absoluteString == "https://chatgpt.com/backend-api/codex/responses")
    }

    @Test func harnessTag() {
        let resolved = config.resolve(target: "/openai@omp/v1/responses")
        #expect(resolved?.url.absoluteString == "https://api.openai.com/v1/responses")
        #expect(resolved?.harnessTag == "omp")
    }

    @Test func anyHost() {
        let resolved = config.resolve(target: "/_@omp/api.example.com/v1/chat/completions")
        #expect(resolved?.url.absoluteString == "https://api.example.com/v1/chat/completions")
        #expect(resolved?.route == "api.example.com")
        #expect(resolved?.harnessTag == "omp")
    }

    @Test func rejectsUnknownAndMalformed() {
        #expect(config.resolve(target: "/nope/v1/messages") == nil)
        #expect(config.resolve(target: "/_/localhost/v1") == nil)
        #expect(config.resolve(target: "/_/") == nil)
        #expect(config.resolve(target: "/") == nil)
    }
}

@Suite struct HTTPParsingTests {
    @Test func dechunkWaitsForCompleteBody() throws {
        let full = Data("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\nNEXT".utf8)
        #expect(try HTTPParsing.dechunk(full.prefix(12)) == nil)
        let decoded = try #require(try HTTPParsing.dechunk(full))
        #expect(String(decoding: decoded.body, as: UTF8.self) == "hello world")
        #expect(decoded.consumed == full.count - 4)
    }

    @Test func parsesRequestHead() throws {
        let head = try #require(HTTPParsing.head(from: Data("POST /anthropic/v1/messages?x=1 HTTP/1.1\r\nHost: 127.0.0.1:4141\r\nUser-Agent: claude-cli/2.1 (external, cli)".utf8)))
        #expect(head.method == "POST")
        #expect(head.path == "/anthropic/v1/messages")
        #expect(head.value("user-agent") == "claude-cli/2.1 (external, cli)")
    }

    @Test func harnessDetection() {
        func detect(_ tag: String?, agent: String) -> String {
            HarnessDetector.detect(tag: tag) { $0 == "User-Agent" ? agent : nil }
        }
        #expect(detect(nil, agent: "claude-cli/2.1.284 (external, cli)") == "Claude Code")
        #expect(detect(nil, agent: "codex_cli_rs/0.159.2 (Mac OS 27.0; arm64)") == "Codex")
        #expect(detect("omp", agent: "claude-cli/2.1.284") == "OMP")
        #expect(detect(nil, agent: "OpenAI/JS 6.2.0") == "Unknown")
        #expect(detect(nil, agent: "my-agent/1.0") == "my-agent")
    }
}
