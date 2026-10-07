import Foundation
import Testing
@testable import SpeedTrackerCore

private func entry(_ json: String) -> [String: Any] {
    (try? JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any]) ?? [:]
}

private func time(_ text: String) -> Date {
    LogDates.parse(text)!
}

@Suite struct SessionLogParserTests {
    @Test func claudeCodeThinkingFirstGivesFirstToken() {
        let parser = ClaudeCodeLogParser()
        var records: [LogRecord] = []
        records += parser.ingest(entry(#"{"type":"user","timestamp":"2026-10-05T20:27:45.992Z","message":{"role":"user","content":[]}}"#))
        #expect(parser.awaitingResponse)
        // The thinking block ended at :57.202 after 9266 ms, so generation began at :47.936.
        records += parser.ingest(entry(#"{"type":"assistant","timestamp":"2026-10-05T20:27:57.202Z","thinkingDurationMs":9266,"message":{"id":"msg_A","model":"claude-opus-5-5","content":[{"type":"thinking"}],"usage":{"input_tokens":10,"cache_read_input_tokens":90,"output_tokens":1840}}}"#))
        records += parser.ingest(entry(#"{"type":"assistant","timestamp":"2026-10-05T20:28:04.491Z","message":{"id":"msg_A","model":"claude-opus-5-5","content":[{"type":"tool_use"}],"usage":{"output_tokens":1840}}}"#))
        // A tool result does not close the message; the next message does.
        records += parser.ingest(entry(#"{"type":"user","timestamp":"2026-10-05T20:28:04.973Z","message":{"role":"user","content":[]}}"#))
        #expect(records.isEmpty)
        records += parser.ingest(entry(#"{"type":"assistant","timestamp":"2026-10-05T20:28:09.000Z","message":{"id":"msg_B","model":"claude-opus-5-5","content":[{"type":"text"}],"usage":{"output_tokens":50}}}"#))

        #expect(records.count == 1)
        let record = records[0]
        #expect(record.key == "claude:msg_A")
        #expect(record.outputTokens == 1840)
        #expect(record.inputTokens == 100)
        #expect(record.requestStart == time("2026-10-05T20:27:45.992Z"))
        #expect(abs(record.firstToken!.timeIntervalSince(time("2026-10-05T20:27:47.936Z"))) < 0.001)
        #expect(record.end == time("2026-10-05T20:28:04.491Z"))

        let stored = record.makeRecord()
        #expect(abs(stored.ttft! - 1.944) < 0.001)
        #expect(abs(stored.tps! - 1840 / 16.555) < 0.01)
        #expect(stored.source == "log")

        // The second message opens with text, so the log alone has no first-token time.
        let rest = parser.flush(idleFor: 5)
        #expect(rest.count == 1)
        #expect(rest[0].firstToken == nil)
        #expect(rest[0].makeRecord().ttft == nil)
    }

    @Test func networkTimingFillsInMissingFirstToken() {
        let log = LogRecord(
            key: "claude:x", harness: "Claude Code", model: "m", requestStart: time("2026-10-05T20:00:00.000Z"),
            firstToken: nil, end: time("2026-10-05T20:00:12.000Z"), outputTokens: 1000
        )
        let observed = (requestStart: time("2026-10-05T20:00:00.300Z"), firstByte: time("2026-10-05T20:00:02.000Z"))
        let stored = log.makeRecord(observed: observed)
        // Request time stays the log's; only the first byte comes from the network.
        #expect(abs(stored.ttft! - 2.0) < 0.001)
        #expect(abs(stored.tps! - 100) < 0.01)
    }

    @Test func codexResponseTimingFromItems() {
        let parser = CodexLogParser()
        var records: [LogRecord] = []
        records += parser.ingest(entry(#"{"type":"turn_context","timestamp":"2026-10-05T19:40:00.000Z","payload":{"model":"gpt-6.1-sol"}}"#))
        records += parser.ingest(entry(#"{"type":"response_item","timestamp":"2026-10-05T19:40:25.140Z","payload":{"type":"custom_tool_call_output"}}"#))
        #expect(parser.awaitingResponse)
        // Reasoning began at 19:40:27.020, the message at 19:40:35.879 (epoch milliseconds).
        records += parser.ingest(entry(#"{"type":"event_msg","timestamp":"2026-10-05T19:40:35.873Z","payload":{"type":"item_completed","item":{"type":"Reasoning"},"started_at_ms":1791229227020,"completed_at_ms":1791229235873}}"#))
        records += parser.ingest(entry(#"{"type":"event_msg","timestamp":"2026-10-05T19:40:40.041Z","payload":{"type":"item_completed","item":{"type":"AgentMessage"},"started_at_ms":1791229235879,"completed_at_ms":1791229240041}}"#))
        records += parser.ingest(entry(#"{"type":"token_usage_record","timestamp":"2026-10-05T19:40:40.160Z","payload":{"response_id":"resp_1","usage":{"input_tokens":156244,"cached_input_tokens":154112,"output_tokens":530,"reasoning_output_tokens":323}}}"#))
        // The repeated token_count that follows must not produce a second record.
        records += parser.ingest(entry(#"{"type":"event_msg","timestamp":"2026-10-05T19:40:40.162Z","payload":{"type":"token_count","info":{"last_token_usage":{"output_tokens":530},"total_token_usage":{"total_tokens":9}}}}"#))

        #expect(records.count == 1)
        #expect(!parser.awaitingResponse)
        let stored = records[0].makeRecord()
        #expect(records[0].key == "codex:resp_1")
        #expect(stored.model == "gpt-6.1-sol")
        #expect(stored.outputTokens == 530)
        #expect(stored.reasoningTokens == 323)
        #expect(abs(stored.ttft! - 1.88) < 0.001)
        #expect(abs(stored.firstVisible! - 10.739) < 0.001)
        #expect(abs(stored.tps! - 530 / 13.14) < 0.01)
    }

    @Test func ompUsesItsOwnMeasurements() {
        let parser = OMPLogParser()
        _ = parser.ingest(entry(#"{"type":"message","id":"u1","timestamp":"2026-09-30T15:02:04.180Z","message":{"role":"toolResult","content":[]}}"#))
        #expect(parser.awaitingResponse)
        let records = parser.ingest(entry(#"{"type":"message","id":"a1","timestamp":"2026-09-30T15:02:25.874Z","message":{"role":"assistant","model":"gpt-6-luna","provider":"openai-codex","timestamp":1790780524211,"duration":21662.5,"ttft":4752.5,"stopReason":"stop","usage":{"input":987,"output":406,"cacheRead":100864,"cacheWrite":0}}}"#))
        #expect(records.count == 1)
        #expect(!parser.awaitingResponse)
        let stored = records[0].makeRecord()
        #expect(stored.harness == "OMP")
        #expect(stored.model == "gpt-6-luna")
        #expect(stored.inputTokens == 101_851)
        #expect(abs(stored.ttft! - 4.7525) < 0.001)
        #expect(abs(stored.tps! - 406 / 16.91) < 0.01)
    }
}

/// Drives a tracker with one connection's counters over time.
private struct Wire {
    let tracker = FlowTracker()
    var rx: UInt64 = 10_000
    var tx: UInt64 = 10_000
    var events: [FlowTracker.Event] = []

    init() {
        _ = step(at: 0)
    }

    @discardableResult
    mutating func step(at time: TimeInterval, sent: UInt64 = 0, received: UInt64 = 0) -> [FlowTracker.Event] {
        tx += sent
        rx += received
        let sample = FlowSample(pid: 7, processName: "claude", localPort: 50_000, remoteAddress: "160.79.104.10", remotePort: 443, rx: rx, tx: tx)
        let new = tracker.ingest([sample], uptime: time, wall: Date(timeIntervalSince1970: 1_000_000 + time))
        events += new
        return new
    }

    var ended: [FlowCall] {
        events.compactMap { if case .ended(let call) = $0 { return call } else { return nil } }
    }
}

@Suite struct FlowTrackerTests {
    @Test func requestThenThinkingPingsThenStream() {
        var wire = Wire()
        // A large upload, with the server's flow-control frames arriving during and just after it.
        wire.step(at: 1.0, sent: 400_000, received: 300)
        wire.step(at: 1.1, sent: 417_000, received: 450)
        wire.step(at: 1.2, received: 60)
        #expect(wire.tracker.isAwaitingFirstByte)
        // Headers and the start of the message.
        wire.step(at: 2.9, received: 4_300)
        // Thinking: a small event about once a second.
        var clock = 3.0
        while clock < 12 {
            wire.step(at: clock, received: clock.truncatingRemainder(dividingBy: 1.2) < 0.1 ? 140 : 0)
            clock += 0.1
        }
        #expect(wire.ended.isEmpty)
        let thinking = wire.tracker.openCalls[0]
        #expect(!thinking.sustained)
        #expect(!thinking.dormant)
        // Text streams.
        while clock < 19 {
            wire.step(at: clock, received: 450)
            clock += 0.1
        }
        #expect(wire.tracker.openCalls[0].sustained)
        // Silence ends it.
        while clock < 22 {
            wire.step(at: clock)
            clock += 0.25
        }
        #expect(wire.ended.count == 1)
        let call = wire.ended[0]
        #expect(call.isStream)
        #expect(abs(call.ttft! - 1.9) < 0.001)
        #expect(abs(call.lastRxUptime - 18.9) < 0.11)
    }

    @Test func smallReplyGoesDormantAndIsNotAStream() {
        var wire = Wire()
        wire.step(at: 1.0, sent: 25_000)
        wire.step(at: 1.5, received: 750)
        var clock = 1.6
        while clock < 6 {
            wire.step(at: clock)
            clock += 0.25
        }
        #expect(wire.tracker.openCalls.first?.dormant == true)
        #expect(!wire.tracker.hasActiveCalls)
        // The next request closes it.
        wire.step(at: 30, sent: 2_000)
        #expect(wire.ended.count == 1)
        #expect(!wire.ended[0].isStream)
    }

    @Test func silentReasoningKeepsOneCall() {
        var wire = Wire()
        wire.step(at: 1.0, sent: 60_000)
        wire.step(at: 2.0, received: 1_500)
        var clock = 2.25
        // Twelve seconds of nothing while the model reasons.
        while clock < 14 {
            wire.step(at: clock)
            clock += 0.25
        }
        #expect(wire.ended.isEmpty)
        #expect(wire.tracker.openCalls.first?.dormant == true)
        while clock < 18 {
            wire.step(at: clock, received: 900)
            clock += 0.25
        }
        while clock < 21 {
            wire.step(at: clock)
            clock += 0.25
        }
        #expect(wire.ended.count == 1)
        #expect(abs(wire.ended[0].ttft! - 1.0) < 0.001)
        #expect(wire.ended[0].isStream)
    }

    @Test func trickleOnAnOpenConnectionIsNotAStream() {
        var wire = Wire()
        // A desktop app's sync connection: a request, then small updates for minutes.
        wire.step(at: 1.0, sent: 2_000)
        var clock = 1.5
        while clock < 120 {
            let second = Int(clock * 4) % 12
            wire.step(at: clock, received: second < 4 ? 40 : 0)
            clock += 0.25
        }
        #expect(wire.tracker.openCalls.allSatisfy { !$0.sustained && !$0.isStream })
        wire.step(at: 300, sent: 2_000)
        #expect(wire.ended.allSatisfy { !$0.isStream })
    }

    @Test func handshakeIsNotMistakenForAReply() {
        var wire = Wire()
        wire.step(at: 1.0, sent: 1_700)           // ClientHello
        wire.step(at: 1.3, received: 5_000)       // ServerHello and certificates, slow enough to be counted
        wire.step(at: 1.4, sent: 90_000)          // the actual request
        wire.step(at: 3.4, received: 3_000)
        #expect(abs(wire.tracker.openCalls[0].firstByteUptime! - 3.4) < 0.001)
    }
}

@Suite struct OversizedLineTests {
    @Test func outlinesAHugeToolResult() {
        let head = Data(#"{"parentUuid":"p","isSidechain":false,"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"AAAA"#.utf8)
        let tail = Data(#"AAAA"}]},"uuid":"u","timestamp":"2026-10-05T20:27:45.992Z"}"#.utf8)
        let entry = SessionLogWatcher.outline(head: head, tail: tail)
        #expect(entry?["type"] as? String == "user")
        #expect(entry?["timestamp"] as? String == "2026-10-05T20:27:45.992Z")
        #expect(entry?["isSidechain"] as? Bool == false)

        let parser = ClaudeCodeLogParser()
        _ = parser.ingest(entry ?? [:])
        #expect(parser.awaitingResponse)
    }

    @Test func outlinesACodexToolOutput() {
        let head = Data(#"{"timestamp":"2026-10-05T19:40:25.140Z","type":"response_item","payload":{"type":"function_call_output","output":"BBBB"#.utf8)
        let entry = SessionLogWatcher.outline(head: head, tail: Data("BBBB\"}}".utf8))
        let parser = CodexLogParser()
        _ = parser.ingest(entry ?? [:])
        #expect(parser.awaitingResponse)
    }

    @Test func neverInventsAModelReply() {
        let head = Data(#"{"type":"assistant","timestamp":"2026-10-05T19:40:25.140Z","message":{"id":"m""#.utf8)
        #expect(SessionLogWatcher.outline(head: head, tail: Data()) == nil)
    }
}

@Suite struct DiscoveryTests {
    @Test func parsesNettopOutput() {
        let output = """
        ,bytes_in,bytes_out,
        launchd.1,0,0,
        tcp6 *.445<->*.*,,,
        claude.59479,869201,31938006,
        tcp4 192.168.1.5:49768<->160.79.104.10:443,671694,29485456,
        Claude Helper.59363,100,200,
        tcp6 2001:db8::5.50000<->2606:4700::6810:202f.443,31346,2720,
        """
        let samples = Nettop.parse(output)
        #expect(samples.count == 2)
        #expect(samples[0] == FlowSample(pid: 59479, processName: "claude", localPort: 49768, remoteAddress: "160.79.104.10", remotePort: 443, rx: 671_694, tx: 29_485_456))
        #expect(samples[1].processName == "Claude Helper")
        #expect(samples[1].remoteAddress == "2606:4700::6810:202f")
        #expect(samples[1].remotePort == 443)
    }

    @Test func classifiesHarnessProcesses() {
        #expect(HarnessCatalog.classify(path: "/Users/a/Library/Application Support/Claude/claude-code/2.1.288/x/claude.app/Contents/MacOS/claude", arguments: ["claude"]) == "Claude Code")
        #expect(HarnessCatalog.classify(path: "/Users/a/.local/share/claude/versions/2.1.284", arguments: ["claude"]) == "Claude Code")
        #expect(HarnessCatalog.classify(path: "/opt/homebrew/Caskroom/codex/0.159.2/bin/codex", arguments: ["codex", "app-server"]) == "Codex")
        #expect(HarnessCatalog.classify(path: "/Users/a/.bun/bin/bun", arguments: ["bun", "/Users/a/.bun/bin/omp"]) == "OMP")
        #expect(HarnessCatalog.classify(path: "/opt/homebrew/bin/node", arguments: ["node", "/opt/homebrew/bin/gemini"]) == "Gemini CLI")
        #expect(HarnessCatalog.classify(path: "/Applications/CodexBar.app/Contents/MacOS/CodexBar", arguments: []) == nil)
        #expect(HarnessCatalog.classify(path: "/Applications/Safari.app/Contents/MacOS/Safari", arguments: []) == nil)
        #expect(HarnessCatalog.classify(path: "/opt/homebrew/bin/node", arguments: ["node", "server.js"]) == nil)
        #expect(HarnessCatalog.classify(path: "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex", arguments: ["codex"]) == "Codex")
    }

    @Test func desktopAppInternalsAreNotHarnesses() {
        let helper = "/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper"
        #expect(HarnessCatalog.classify(path: helper, arguments: []) == nil)
        #expect(HarnessCatalog.classify(path: "/Applications/Claude.app/Contents/MacOS/Claude", arguments: []) == nil)
        #expect(HarnessCatalog.isHostAppProcess(path: helper))
        #expect(HarnessCatalog.isHostAppProcess(path: "/Applications/ChatGPT.app/Contents/Frameworks/Codex Framework.framework/Versions/1/Helpers/Codex (Service).app/Contents/MacOS/Codex (Service)"))
        #expect(!HarnessCatalog.isHostAppProcess(path: "/opt/homebrew/bin/aider"))
    }

    @Test func findsGatewayHostsWithoutReadingSecrets() {
        let config = """
        providers:
          shop:
            baseUrl: https://api.example-gateway.dev/v1
            apiKey: sk-should-never-be-read-https://not-a-host.invalid
        [model_providers.x]
        base_url = "http://127.0.0.1:4141/openai/v1"
        """
        #expect(HostCatalog.baseURLHosts(in: config) == ["api.example-gateway.dev"])
    }
}
