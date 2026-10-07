import CSQLite
import Foundation
import Testing
@testable import SpeedTrackerCore

private func entry(_ json: String) -> [String: Any] {
    (try? JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any]) ?? [:]
}

private func time(_ text: String) -> Date {
    LogDates.parse(text)!
}

// MARK: - Gemini CLI

/// Lines shaped as Gemini CLI 0.46's ChatRecordingService writes them.
@Suite struct GeminiCLILogParserTests {
    private let header = #"{"sessionId":"s1","projectHash":"abc","startTime":"2026-10-07T16:58:42.431Z","lastUpdated":"2026-10-07T16:58:42.431Z","kind":"main"}"#

    @Test func thinkingReplyThenToolFollowUp() {
        let parser = GeminiCLILogParser()
        var records: [LogRecord] = []
        records += parser.ingest(entry(header))
        records += parser.ingest(entry(#"{"id":"u1","timestamp":"2026-10-07T17:00:00.000Z","type":"user","content":[{"text":"private prompt"}]}"#))
        #expect(parser.awaitingResponse)
        records += parser.ingest(entry(#"{"$set":{"lastUpdated":"2026-10-07T17:00:00.001Z"}}"#))

        // The message is created when its stream ends; the first thought arrived at +1.5 s.
        let reply = #"{"id":"g1","timestamp":"2026-10-07T17:00:06.000Z","type":"gemini","content":"","thoughts":[{"subject":"s","description":"d","timestamp":"2026-10-07T17:00:01.500Z"},{"subject":"s","description":"d","timestamp":"2026-10-07T17:00:03.000Z"}],"tokens":{"input":1000,"output":200,"cached":400,"thoughts":100,"tool":0,"total":1300},"model":"gemini-3-pro""#
        records += parser.ingest(entry(reply + "}"))
        #expect(records.count == 1)
        #expect(!parser.awaitingResponse)

        let first = records[0].makeRecord()
        #expect(records[0].key == "gemini:g1")
        #expect(first.harness == "Gemini CLI")
        #expect(first.model == "gemini-3-pro")
        // Gemini reports thinking apart from the reply; the response produced both.
        #expect(first.outputTokens == 300)
        #expect(first.reasoningTokens == 100)
        #expect(first.inputTokens == 1000)
        #expect(first.cachedInputTokens == 400)
        #expect(abs(first.ttft! - 1.5) < 0.001)
        #expect(abs(first.tps! - 300 / 4.5) < 0.01)

        // The same message is written again once its tool has run. No second record,
        // and the next request leaves when the tool finished.
        records += parser.ingest(entry(reply + #","toolCalls":[{"id":"t1","name":"read_file","status":"success","timestamp":"2026-10-07T17:00:09.000Z"}]}"#))
        #expect(records.count == 1)
        #expect(parser.awaitingResponse)
        #expect(parser.lastRequestAt == time("2026-10-07T17:00:09.000Z"))

        // A reply with no thoughts has no first-token time in the log.
        records += parser.ingest(entry(#"{"id":"g2","timestamp":"2026-10-07T17:00:12.000Z","type":"gemini","content":"done","thoughts":[],"tokens":{"input":1200,"output":60,"cached":0,"thoughts":0,"tool":0,"total":1260},"model":"gemini-3-pro"}"#))
        #expect(records.count == 2)
        let second = records[1].makeRecord()
        #expect(second.ttft == nil)
        #expect(second.reasoningTokens == nil)
        #expect(abs(second.tps! - 20) < 0.01)
        #expect(records[1].requestStart == time("2026-10-07T17:00:09.000Z"))
    }

    @Test func replyWithoutAKnownRequestGetsNoStartTime() {
        let parser = GeminiCLILogParser()
        _ = parser.ingest(entry(#"{"id":"u1","timestamp":"2026-10-07T17:00:00.000Z","type":"user","content":"x"}"#))
        let first = parser.ingest(entry(#"{"id":"g1","timestamp":"2026-10-07T17:00:04.000Z","type":"gemini","content":"a","tokens":{"output":40,"thoughts":0},"model":"m"}"#))
        // A second reply with no new request in between: its start is unknown, not the old one.
        let second = parser.ingest(entry(#"{"id":"g2","timestamp":"2026-10-07T17:00:30.000Z","type":"gemini","content":"b","tokens":{"output":40,"thoughts":0},"model":"m"}"#))
        #expect(first[0].requestStart != nil)
        #expect(second.count == 1)
        #expect(second[0].requestStart == nil)
        #expect(second[0].makeRecord().ttft == nil)
        #expect(second[0].makeRecord().tps == nil)
    }

    @Test func tokensArrivingOnALaterWriteProduceOneRecord() {
        let parser = GeminiCLILogParser()
        _ = parser.ingest(entry(#"{"id":"u1","timestamp":"2026-10-07T17:00:00.000Z","type":"user","content":"x"}"#))
        let early = parser.ingest(entry(#"{"id":"g1","timestamp":"2026-10-07T17:00:05.000Z","type":"gemini","content":"a","tokens":null,"model":"m"}"#))
        let late = parser.ingest(entry(#"{"id":"g1","timestamp":"2026-10-07T17:00:05.000Z","type":"gemini","content":"a","tokens":{"output":100,"thoughts":0},"model":"m"}"#))
        let again = parser.ingest(entry(#"{"id":"g1","timestamp":"2026-10-07T17:00:05.000Z","type":"gemini","content":"a","tokens":{"output":100,"thoughts":0},"model":"m"}"#))
        #expect(early.isEmpty)
        #expect(late.count == 1)
        #expect(again.isEmpty)
        #expect(abs(late[0].makeRecord().tps! - 20) < 0.01)
    }

    @Test func messagesInsideAMetadataUpdateCount() {
        let parser = GeminiCLILogParser()
        _ = parser.ingest(entry(#"{"$set":{"messages":[{"id":"u1","timestamp":"2026-10-07T17:00:00.000Z","type":"user","content":[{"text":"x"}]}],"lastUpdated":"2026-10-07T17:00:00.000Z"}}"#))
        #expect(parser.awaitingResponse)
        #expect(parser.lastRequestAt == time("2026-10-07T17:00:00.000Z"))
        // Status lines are not model calls.
        #expect(parser.ingest(entry(#"{"id":"i1","timestamp":"2026-10-07T17:00:01.000Z","type":"info","content":"note"}"#)).isEmpty)
        #expect(parser.ingest(entry(#"{"id":"e1","timestamp":"2026-10-07T17:00:02.000Z","type":"error","content":"quota"}"#)).isEmpty)
        #expect(!parser.awaitingResponse)
    }

    @Test func watcherReadsSessionFilesUnderTheProjectFolder() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("gemini-fixture-\(UUID().uuidString)")
        let chats = root.appendingPathComponent("project/chats")
        try FileManager.default.createDirectory(at: chats, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let lines = [
            header,
            #"{"id":"u1","timestamp":"2026-10-07T17:00:00.000Z","type":"user","content":"x"}"#,
            #"{"id":"g1","timestamp":"2026-10-07T17:00:05.000Z","type":"gemini","content":"a","thoughts":[{"timestamp":"2026-10-07T17:00:01.000Z"}],"tokens":{"output":80,"thoughts":20},"model":"gemini-3-flash"}"#,
        ]
        try Data((lines.joined(separator: "\n") + "\n").utf8).write(to: chats.appendingPathComponent("session-2026-10-07T17-00-abcd1234.jsonl"))

        let empty = root.appendingPathComponent("none")
        let watcher = SessionLogWatcher(
            sources: [.init(harness: "Gemini CLI", root: root) { GeminiCLILogParser() }],
            openCode: OpenCodeLogWatcher(roots: [empty]), deepSeek: DeepSeekLogWatcher(roots: [empty]),
            antigravity: AntigravityLogWatcher(roots: [empty])
        )
        let records = watcher.poll()
        #expect(records.map(\.key) == ["gemini:g1"])
        #expect(watcher.latestModel["Gemini CLI"] == "gemini-3-flash")
        #expect(watcher.hasLogs("Gemini CLI"))
        #expect(!watcher.hasLogs("Antigravity"))
    }
}

// MARK: - Antigravity

/// Writes protobuf the way Antigravity's rows are laid out, for fixtures.
private enum Wire {
    static func varint(_ value: UInt64) -> Data {
        var value = value
        var data = Data()
        repeat {
            let byte = UInt8(value & 0x7F)
            value >>= 7
            data.append(value == 0 ? byte : byte | 0x80)
        } while value != 0
        return data
    }

    static func number(_ field: Int, _ value: UInt64) -> Data {
        varint(UInt64(field << 3)) + varint(value)
    }

    static func bytes(_ field: Int, _ value: Data) -> Data {
        varint(UInt64(field << 3 | 2)) + varint(UInt64(value.count)) + value
    }

    static func text(_ field: Int, _ value: String) -> Data {
        bytes(field, Data(value.utf8))
    }

    static func seconds(_ value: Double) -> Data {
        let whole = value.rounded(.down)
        return number(1, UInt64(whole)) + number(2, UInt64(((value - whole) * 1_000_000_000).rounded()))
    }

    /// One `gen_metadata` row. The prompt fields are present, as they are in real rows.
    static func generation(
        steps: [UInt64], output: UInt64?, thinking: UInt64 = 0, input: UInt64 = 4000, cacheRead: UInt64 = 20_000,
        ttft: Double?, streaming: Double?, model: String = "gemini-3.8-flash-n", execution: String = "exec-1"
    ) -> Data {
        var chat = text(1, "SYSTEM PROMPT: a secret that must never be read") + bytes(2, text(1, "user said something private"))
        chat += number(3, 1318)
        if let output {
            chat += bytes(4, number(1, 1318) + number(2, input) + number(3, output) + number(5, cacheRead)
                + number(6, 24) + number(9, thinking) + number(10, output - thinking))
        }
        if let ttft { chat += bytes(11, seconds(ttft)) }
        if let streaming { chat += bytes(12, seconds(streaming)) }
        chat += text(19, model)
        return bytes(1, chat) + bytes(2, steps.reduce(Data()) { $0 + varint($1) }) + text(4, execution)
    }

    static func stepMetadata(created: Double, completed: Double?) -> Data {
        var data = bytes(1, seconds(created)) + number(3, 7)
        if let completed { data += bytes(8, seconds(completed)) }
        return data
    }
}

private final class AntigravityFixture {
    let root: URL
    let database: URL
    private var connection: OpaquePointer?
    /// 2026-10-07T17:02:47Z, the start of the first call.
    static let epoch: Double = 1_791_392_567

    init() throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent("antigravity-fixture-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        database = root.appendingPathComponent("11111111-0000-4000-8000-000000000001.db")
        guard sqlite3_open(database.path, &connection) == SQLITE_OK else { throw NSError(domain: "AntigravityFixture", code: 1) }
        try run("CREATE TABLE gen_metadata (idx integer, data blob, size integer NOT NULL DEFAULT 0, PRIMARY KEY (idx))")
        try run("CREATE TABLE steps (idx integer, step_type integer NOT NULL DEFAULT 0, status integer NOT NULL DEFAULT 0, metadata blob, step_payload blob, PRIMARY KEY (idx))")
    }

    deinit {
        sqlite3_close(connection)
        try? FileManager.default.removeItem(at: root)
    }

    func step(_ index: Int, created: Double, completed: Double?) throws {
        try run(
            "INSERT OR REPLACE INTO steps (idx, step_type, status, metadata, step_payload) VALUES (\(index), 15, 3, ?, ?)",
            blobs: [Wire.stepMetadata(created: Self.epoch + created, completed: completed.map { Self.epoch + $0 }), Data("reply text".utf8)]
        )
    }

    func generation(_ index: Int, _ data: Data) throws {
        try run("INSERT OR REPLACE INTO gen_metadata (idx, data, size) VALUES (\(index), ?, \(data.count))", blobs: [data])
    }

    private func run(_ sql: String, blobs: [Data] = []) throws {
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(connection, sql, -1, &statement, nil) == SQLITE_OK, let statement else {
            throw NSError(domain: "AntigravityFixture", code: 2, userInfo: [NSLocalizedDescriptionKey: String(cString: sqlite3_errmsg(connection))])
        }
        defer { sqlite3_finalize(statement) }
        for (offset, blob) in blobs.enumerated() {
            _ = blob.withUnsafeBytes {
                sqlite3_bind_blob(statement, Int32(offset + 1), $0.baseAddress, Int32(blob.count), unsafeBitCast(-1, to: sqlite3_destructor_type.self))
            }
        }
        guard sqlite3_step(statement) == SQLITE_DONE else { throw NSError(domain: "AntigravityFixture", code: 3) }
    }

    func watcher() -> AntigravityLogWatcher {
        AntigravityLogWatcher(roots: [root], backfillWindow: 3600)
    }

    func now(_ offset: Double) -> Date {
        Date(timeIntervalSince1970: Self.epoch + offset)
    }
}

@Suite struct AntigravityLogTests {
    @Test func decodesACallWithoutTouchingThePrompt() throws {
        let generation = try #require(AntigravityGeneration(Wire.generation(steps: [1, 2], output: 212, thinking: 148, ttft: 5.256, streaming: 0.302)))
        #expect(generation.stepIndices == [1, 2])
        #expect(generation.executionID == "exec-1")
        #expect(generation.model == "gemini-3.8-flash-n")
        #expect(generation.outputTokens == 212)
        #expect(generation.thinkingTokens == 148)
        #expect(generation.inputTokens == 4000)
        #expect(generation.cacheReadTokens == 20_000)
        #expect(abs(generation.timeToFirstToken! - 5.256) < 0.0001)
        #expect(abs(generation.streamingDuration! - 0.302) < 0.0001)
        #expect(generation.isComplete)
        // Free text is never accepted where a model name or id is expected.
        #expect(Protobuf.identifier(Data("user said something private".utf8)) == nil)
        #expect(Protobuf.identifier(Data("gemini-3.8-flash-n".utf8)) == "gemini-3.8-flash-n")
    }

    @Test func aBurstIsNotTimedAsAStream() throws {
        // Real shape: five seconds of hidden thinking, then the whole reply in 0.3 s.
        let generation = try #require(AntigravityGeneration(Wire.generation(steps: [1], output: 212, thinking: 148, ttft: 5.256, streaming: 0.302)))
        let stored = try #require(generation.record(conversation: "c", index: 0, startedAt: Date(timeIntervalSince1970: AntigravityFixture.epoch))).makeRecord()
        #expect(abs(stored.ttft! - 5.256) < 0.001)
        #expect(abs(stored.generation! - 0.302) < 0.001)
        #expect(abs(stored.total - 5.558) < 0.001)
        // 212 tokens over the whole 5.558 s, not 702 tok/s over the burst.
        #expect(abs(stored.tps! - 212 / 5.558) < 0.01)
        #expect(stored.outputTokens == 212)
        #expect(stored.reasoningTokens == 148)
        #expect(stored.inputTokens == 24_000)
        #expect(stored.cachedInputTokens == 20_000)
        #expect(stored.harness == "Antigravity")
        #expect(stored.source == "log")
    }

    @Test func aLongStreamIsTimedOnItsVisibleTokens() throws {
        // Real shape: 2556 thinking tokens before the first token, then 1344 visible ones over 31.9 s.
        let generation = try #require(AntigravityGeneration(Wire.generation(steps: [39], output: 3900, thinking: 2556, ttft: 17.926, streaming: 31.942)))
        let record = try #require(generation.record(conversation: "c", index: 19, startedAt: Date(timeIntervalSince1970: AntigravityFixture.epoch)))
        #expect(record.key == "antigravity:c:exec-1:19")
        let stored = record.makeRecord()
        #expect(abs(stored.ttft! - 17.926) < 0.001)
        #expect(abs(stored.tps! - 1344 / 31.942) < 0.01)
    }

    @Test func readsCallsFromAConversationFile() throws {
        let fixture = try AntigravityFixture()
        try fixture.step(1, created: 0.019, completed: 5.577)
        try fixture.generation(0, Wire.generation(steps: [1, 2], output: 212, thinking: 148, ttft: 5.256, streaming: 0.302))
        try fixture.step(3, created: 5.593, completed: 10.633)
        try fixture.generation(1, Wire.generation(steps: [3, 4], output: 154, thinking: 76, ttft: 4.978, streaming: 0.062))

        let watcher = fixture.watcher()
        let records = watcher.poll(now: fixture.now(20))
        #expect(watcher.hasLogs)
        #expect(records.count == 2)
        #expect(records[0].requestStart == Date(timeIntervalSince1970: AntigravityFixture.epoch + 0.019))
        #expect(abs(records[0].end.timeIntervalSince1970 - (AntigravityFixture.epoch + 0.019 + 5.256 + 0.302)) < 0.001)
        #expect(watcher.latestModel == "gemini-3.8-flash-n")
        #expect(!watcher.isAwaiting(now: fixture.now(20)))
        #expect(watcher.lastRequestAt == Date(timeIntervalSince1970: AntigravityFixture.epoch + 5.593))
        // Nothing changed: nothing is reported twice, and reading left no files behind.
        #expect(watcher.poll(now: fixture.now(30)).isEmpty)
        #expect(try FileManager.default.contentsOfDirectory(atPath: fixture.root.path).count == 1)
    }

    @Test func aCallInFlightIsAwaitedThenReported() throws {
        let fixture = try AntigravityFixture()
        try fixture.step(1, created: 0, completed: nil)
        try fixture.generation(0, Wire.generation(steps: [1], output: nil, ttft: nil, streaming: nil))
        let watcher = fixture.watcher()
        #expect(watcher.poll(now: fixture.now(2)).isEmpty)
        #expect(watcher.isAwaiting(now: fixture.now(2)))
        #expect(watcher.lastRequestAt == Date(timeIntervalSince1970: AntigravityFixture.epoch))

        try fixture.step(1, created: 0, completed: 8)
        try fixture.generation(0, Wire.generation(steps: [1], output: 400, thinking: 100, ttft: 3, streaming: 5))
        let records = watcher.poll(now: fixture.now(10))
        #expect(records.count == 1)
        #expect(!watcher.isAwaiting(now: fixture.now(10)))
        #expect(abs(records[0].makeRecord().tps! - 60) < 0.01)
        // A request that never completes stops counting as in flight.
        try fixture.step(5, created: 20, completed: nil)
        _ = watcher.poll(now: fixture.now(30))
        #expect(watcher.isAwaiting(now: fixture.now(30)))
        #expect(!watcher.isAwaiting(now: fixture.now(700)))
    }

    @Test func anAbandonedCallDoesNotHoldUpLaterOnes() throws {
        let fixture = try AntigravityFixture()
        try fixture.step(1, created: 0, completed: nil)
        try fixture.generation(0, Wire.generation(steps: [1], output: nil, ttft: nil, streaming: nil))
        try fixture.step(2, created: 10, completed: 16)
        try fixture.generation(1, Wire.generation(steps: [2], output: 300, ttft: 2, streaming: 4))
        let watcher = fixture.watcher()
        #expect(watcher.poll(now: fixture.now(20)).map(\.key) == ["antigravity:11111111-0000-4000-8000-000000000001:exec-1:1"])
        // The unfinished first row is not read again on every change.
        try fixture.step(3, created: 30, completed: 36)
        try fixture.generation(2, Wire.generation(steps: [3], output: 100, ttft: 1, streaming: 2))
        #expect(watcher.poll(now: fixture.now(40)).count == 1)
    }

    @Test func oldConversationsAreRecognisedButNotReplayed() throws {
        let fixture = try AntigravityFixture()
        try fixture.step(1, created: 0, completed: 6)
        try fixture.generation(0, Wire.generation(steps: [1], output: 300, ttft: 2, streaming: 4))
        try FileManager.default.setAttributes([.modificationDate: Date(timeIntervalSince1970: AntigravityFixture.epoch)], ofItemAtPath: fixture.database.path)
        let watcher = fixture.watcher()
        // Ten days later the call is outside the window, but the harness is still known to keep logs.
        #expect(watcher.poll(now: fixture.now(10 * 86_400)).isEmpty)
        #expect(watcher.hasLogs)
    }

    @Test func malformedRowsAreSkipped() throws {
        #expect(AntigravityGeneration(Data([0xFF, 0xFF, 0xFF])) == nil)
        #expect(Protobuf.fields(Data([0x0A, 0x05, 0x01])) == nil)
        let fixture = try AntigravityFixture()
        try fixture.generation(0, Data([0x0A, 0x7F, 0x00]))
        try fixture.step(2, created: 10, completed: 16)
        try fixture.generation(1, Wire.generation(steps: [2], output: 300, ttft: 2, streaming: 4))
        #expect(fixture.watcher().poll(now: fixture.now(20)).count == 1)
    }
}

@Suite struct GeminiAntigravityDiscoveryTests {
    @Test func classifiesTheirProcesses() {
        #expect(HarnessCatalog.classify(path: "/opt/homebrew/bin/node", arguments: ["node", "/opt/homebrew/bin/gemini"]) == "Gemini CLI")
        #expect(HarnessCatalog.classify(path: "/opt/homebrew/bin/node", arguments: ["node", "/opt/homebrew/Cellar/gemini-cli/0.46.0/libexec/lib/node_modules/@google/gemini-cli/bundle/gemini.js"]) == "Gemini CLI")
        #expect(HarnessCatalog.classify(path: "/Applications/Antigravity.app/Contents/Resources/bin/language_server", arguments: ["language_server"]) == "Antigravity")
        #expect(HarnessCatalog.classify(path: "/Users/a/.local/bin/agy", arguments: ["agy"]) == "Antigravity")
        #expect(HarnessCatalog.classify(path: "/Users/a/.gemini/antigravity/bin/agentapi", arguments: ["agentapi"]) == "Antigravity")
        // The editor's own windows and helpers are not the agent.
        let helper = "/Applications/Antigravity.app/Contents/Frameworks/Antigravity Helper.app/Contents/MacOS/Antigravity Helper"
        #expect(HarnessCatalog.classify(path: helper, arguments: []) == nil)
        #expect(HarnessCatalog.isHostAppProcess(path: helper))
        // Another editor's language server is not Antigravity.
        #expect(HarnessCatalog.classify(path: "/Applications/Other.app/Contents/Resources/bin/language_server", arguments: []) == nil)
    }
}

/// A machine built in a temporary folder: a home, and a root standing in for `/`.
private final class MachineFixture {
    let base: URL
    var home: URL { base.appendingPathComponent("home") }
    var root: URL { base.appendingPathComponent("root") }

    init() throws {
        base = FileManager.default.temporaryDirectory.appendingPathComponent("presence-fixture-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }

    deinit {
        try? FileManager.default.removeItem(at: base)
    }

    func command(_ path: String, in parent: URL, executable: Bool = true) throws {
        let url = parent.appendingPathComponent(path)
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data("#!/bin/sh\n".utf8).write(to: url)
        try FileManager.default.setAttributes([.posixPermissions: executable ? 0o755 : 0o644], ofItemAtPath: url.path)
    }

    func folder(_ path: String, in parent: URL) throws {
        try FileManager.default.createDirectory(at: parent.appendingPathComponent(path), withIntermediateDirectories: true)
    }

    func installed(searchPath: String? = nil) -> Set<String> {
        HarnessPresence.installed(home: home, searchPath: searchPath, systemRoot: root)
    }
}

@Suite struct HarnessPresenceTests {
    @Test func anEmptyMachineHasNoHarnesses() throws {
        #expect(try MachineFixture().installed().isEmpty)
    }

    @Test func findsCommandsWhereTheyAreInstalled() throws {
        let machine = try MachineFixture()
        try machine.command(".local/bin/claude", in: machine.home)
        try machine.command(".bun/bin/omp", in: machine.home)
        try machine.command("opt/homebrew/bin/gemini", in: machine.root)
        try machine.command(".nvm/versions/node/v22.1.0/bin/codex", in: machine.home)
        #expect(machine.installed() == ["Claude Code", "OMP", "Gemini CLI", "Codex"])
    }

    @Test func honoursThePathItIsGiven() throws {
        let machine = try MachineFixture()
        try machine.command("tools/dsh", in: machine.base)
        #expect(machine.installed().isEmpty)
        #expect(machine.installed(searchPath: "/nonexistent:" + machine.base.appendingPathComponent("tools").path) == ["DeepSeek CLI"])
    }

    @Test func findsAppsAndBundledAgents() throws {
        let machine = try MachineFixture()
        try machine.folder("Applications/Antigravity.app", in: machine.root)
        try machine.folder("Applications/ChatGPT.app/Contents/Resources/codex-cli", in: machine.root)
        try machine.folder("Library/Application Support/Claude/claude-code", in: machine.home)
        #expect(machine.installed() == ["Antigravity", "Codex", "Claude Code"])
    }

    @Test func aFileThatCannotRunIsNotACommand() throws {
        let machine = try MachineFixture()
        try machine.command(".local/bin/opencode", in: machine.home, executable: false)
        // A folder passes the file system's executable test, but it is not a command.
        try machine.folder(".local/bin/aider", in: machine.home)
        #expect(machine.installed().isEmpty)
    }

    @Test func everySignatureNamesAHarnessTheClassifierKnows() {
        // The list offered to the user and the name given to a running process must agree.
        for signature in HarnessPresence.signatures {
            let command = signature.commands[0]
            #expect(HarnessCatalog.classify(path: "/usr/local/bin/" + command, arguments: [command]) == signature.harness)
        }
    }
}
