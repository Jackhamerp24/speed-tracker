import Foundation
import Testing
import CSQLite
@testable import SpeedTrackerCore

private final class OpenCodeFixture {
    let root: URL
    let database: URL
    private var connection: OpaquePointer?
    static let epoch: Int64 = 1_790_000_000_000
    var now: Date { Date(timeIntervalSince1970: Double(Self.epoch + 30_000) / 1000) }

    init(current: Bool = true, legacy: Bool = false, wal: Bool = false) throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent("opencode-fixture-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        database = root.appendingPathComponent("opencode.db")
        guard sqlite3_open(database.path, &connection) == SQLITE_OK else { throw NSError(domain: "OpenCodeFixture", code: 1) }
        if wal { try sql("PRAGMA journal_mode=WAL") }
        if current { try currentSchema() }
        if legacy {
            try sql("""
                CREATE TABLE session(id TEXT PRIMARY KEY,time_updated INTEGER NOT NULL);
                CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,time_created INTEGER NOT NULL,time_updated INTEGER NOT NULL,data TEXT NOT NULL);
                CREATE INDEX message_session_time_created_id_idx ON message(session_id,time_created,id);
                CREATE TABLE part(id TEXT PRIMARY KEY,message_id TEXT NOT NULL,session_id TEXT NOT NULL,time_created INTEGER NOT NULL,time_updated INTEGER NOT NULL,data TEXT NOT NULL);
                CREATE INDEX part_message_id_id_idx ON part(message_id,id);
                """)
            try sql("INSERT INTO session VALUES('ses_fixture',\(Self.epoch + 30_000))")
        }
    }

    deinit {
        sqlite3_close(connection)
        try? FileManager.default.removeItem(at: root)
    }

    func currentSchema() throws {
        try sql("""
            CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,type TEXT NOT NULL,seq INTEGER NOT NULL,time_created INTEGER NOT NULL,time_updated INTEGER NOT NULL,data TEXT NOT NULL);
            CREATE INDEX session_message_time_created_idx ON session_message(time_created);
            CREATE UNIQUE INDEX session_message_session_seq_idx ON session_message(session_id,seq);
            """)
    }

    func sql(_ sql: String) throws {
        var error: UnsafeMutablePointer<CChar>?
        guard sqlite3_exec(connection, sql, nil, nil, &error) == SQLITE_OK else {
            let message = error.map { String(cString: $0) } ?? "SQLite fixture error"
            sqlite3_free(error)
            throw NSError(domain: "OpenCodeFixture", code: 2, userInfo: [NSLocalizedDescriptionKey: message])
        }
    }

    func message(
        _ id: String, current: Bool = true, created: Int64 = OpenCodeFixture.epoch, updated: Int64 = OpenCodeFixture.epoch + 20_000,
        metadata: [String: Any], seq: Int = 0
    ) throws {
        let table = current ? "session_message" : "message"
        let typeAndSeq = current ? ",'assistant',\(seq)" : ""
        try bind("INSERT OR REPLACE INTO \(table) VALUES(?,'ses_fixture'\(typeAndSeq),\(created),\(updated),?)", values: [id, Self.json(metadata)])
    }

    func update(_ id: String, current: Bool = true, metadata: [String: Any], updated: Int64 = OpenCodeFixture.epoch + 25_000) throws {
        try bind("UPDATE \(current ? "session_message" : "message") SET data=?,time_updated=\(updated) WHERE id=?", values: [Self.json(metadata), id])
    }

    func part(_ id: String, message: String, type: String, time: [String: Int64]? = nil, at: Int64 = OpenCodeFixture.epoch + 10_000, extra: [String: Any] = [:]) throws {
        var data: [String: Any] = ["type": type]
        if let time { data["time"] = time }
        for (key, value) in extra { data[key] = value }
        try bind("INSERT INTO part VALUES(?,?,'ses_fixture',\(at),\(at),?)", values: [id, message, Self.json(data)])
    }

    private func bind(_ sql: String, values: [String]) throws {
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(connection, sql, -1, &statement, nil) == SQLITE_OK, let statement else {
            throw NSError(domain: "OpenCodeFixture", code: 3)
        }
        defer { sqlite3_finalize(statement) }
        for (index, value) in values.enumerated() {
            value.withCString { pointer in
                _ = sqlite3_bind_text(statement, Int32(index + 1), pointer, -1, unsafeBitCast(-1, to: sqlite3_destructor_type.self))
            }
        }
        guard sqlite3_step(statement) == SQLITE_DONE else { throw NSError(domain: "OpenCodeFixture", code: 4) }
    }

    static func json(_ object: [String: Any]) -> String {
        String(decoding: try! JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]), as: UTF8.self)
    }

    static func assistant(current: Bool = true, completed: Bool = true, created: Int64 = epoch, content: [[String: Any]] = []) -> [String: Any] {
        var result: [String: Any] = [
            "time": completed ? ["created": created, "completed": created + 20_000] : ["created": created],
            "tokens": ["input": 100, "output": 80, "reasoning": 20, "cache": ["read": 40, "write": 10]]
        ]
        if current {
            result["model"] = ["id": "fixture-model", "providerID": "fixture-provider"]
            result["content"] = content
        } else {
            result["role"] = "assistant"
            result["modelID"] = "fixture-model"
            result["providerID"] = "fixture-provider"
        }
        if completed { result["finish"] = "stop" }
        return result
    }
}

@Suite struct OpenCodeTests {
    @Test func currentSchemaUsesExactSplitUsageWithoutSyntheticRequestTiming() throws {
        let fixture = try OpenCodeFixture()
        let start = OpenCodeFixture.epoch
        let content: [[String: Any]] = [
            ["type": "reasoning", "time": ["created": start + 2000, "completed": start + 8000]],
            ["type": "text", "text": "fixture output is irrelevant to telemetry"]
        ]
        var message = OpenCodeFixture.assistant(content: content)
        message["time"] = ["created": start, "streamed": start + 1000, "completed": start + 20_000]
        try fixture.message("msg_current", metadata: message)
        let watcher = OpenCodeLogWatcher(roots: [fixture.root])
        let records = watcher.poll(now: fixture.now)
        let record = try #require(records.first)
        #expect(records.count == 1)
        #expect(watcher.hasLogs)
        #expect(record.key == "opencode:msg_current")
        #expect(record.harness == "opencode")
        #expect(record.provider == "fixture-provider")
        #expect(record.model == "fixture-model")
        #expect(record.outputTokens == 100)
        #expect(record.reasoningTokens == 20)
        #expect(record.inputTokens == 150)
        #expect(record.cachedInputTokens == 40)
        #expect(record.requestStart == nil)
        #expect(record.firstToken == LogDates.milliseconds(Double(start + 2000)))
        #expect(record.firstVisible == nil)
        #expect(record.makeRecord().ttft == nil)
        #expect(!watcher.isAwaiting(now: fixture.now))
        #expect(watcher.poll(now: fixture.now).isEmpty)
    }

    @Test func currentTextBeforeReasoningHasNoInventedFirstToken() throws {
        let fixture = try OpenCodeFixture()
        try fixture.message("msg_text_first", metadata: OpenCodeFixture.assistant(content: [
            ["type": "text"], ["type": "reasoning", "time": ["created": OpenCodeFixture.epoch + 2000]]
        ]))
        let watcher = OpenCodeLogWatcher(roots: [fixture.database])
        let record = try #require(watcher.poll(now: fixture.now).first)
        #expect(record.firstToken == nil)
        #expect(record.firstVisible == nil)
        #expect(record.makeRecord().ttft == nil)
    }

    @Test func currentToolInputTimeIsVisibleButToolExecutionIsNotAwaiting() throws {
        let fixture = try OpenCodeFixture()
        let content: [[String: Any]] = [["type": "tool", "time": ["created": OpenCodeFixture.epoch + 2000], "state": ["status": "running"]]]
        try fixture.message("msg_tool", metadata: OpenCodeFixture.assistant(content: content))
        let watcher = OpenCodeLogWatcher(roots: [fixture.root])
        let record = try #require(watcher.poll(now: fixture.now).first)
        #expect(record.firstVisible == record.firstToken)
        var paused = OpenCodeFixture.assistant(completed: false, created: OpenCodeFixture.epoch + 21_000, content: content)
        paused.removeValue(forKey: "tokens")
        try fixture.message("msg_tool_pause", created: OpenCodeFixture.epoch + 21_000, metadata: paused, seq: 1)
        #expect(watcher.poll(now: fixture.now).isEmpty)
        #expect(!watcher.isAwaiting(now: fixture.now))
    }

    @Test func walInPlaceCompletionRefreshesPendingAndDeduplicates() throws {
        let fixture = try OpenCodeFixture(wal: true)
        var incomplete = OpenCodeFixture.assistant(completed: false)
        incomplete.removeValue(forKey: "tokens")
        try fixture.message("msg_pending", metadata: incomplete)
        let watcher = OpenCodeLogWatcher(roots: [fixture.root])
        #expect(watcher.poll(now: fixture.now).isEmpty)
        #expect(watcher.hasLogs)
        #expect(watcher.isAwaiting(now: fixture.now))
        #expect(watcher.lastRequestAt == nil)
        try fixture.update("msg_pending", metadata: OpenCodeFixture.assistant())
        let result = watcher.poll(now: fixture.now.addingTimeInterval(1))
        #expect(result.count == 1)
        #expect(!watcher.isAwaiting(now: fixture.now.addingTimeInterval(1)))
        #expect(watcher.poll(now: fixture.now.addingTimeInterval(2)).isEmpty)
        try fixture.update("msg_pending", metadata: OpenCodeFixture.assistant(), updated: OpenCodeFixture.epoch + 29_000)
        #expect(watcher.poll(now: fixture.now.addingTimeInterval(3)).isEmpty)
    }

    @Test func finishedMissingUsageAndErrorsStopActivityWithoutFabricatingCalls() throws {
        let fixture = try OpenCodeFixture(wal: true)
        var incomplete = OpenCodeFixture.assistant(completed: false)
        incomplete.removeValue(forKey: "tokens")
        try fixture.message("msg_error", metadata: incomplete)
        let watcher = OpenCodeLogWatcher(roots: [fixture.root])
        _ = watcher.poll(now: fixture.now)
        #expect(watcher.isAwaiting(now: fixture.now))
        var error = OpenCodeFixture.assistant()
        error.removeValue(forKey: "tokens")
        error["finish"] = "error"
        error["error"] = ["type": "unknown", "message": "fixture error"]
        try fixture.update("msg_error", metadata: error)
        #expect(watcher.poll(now: fixture.now).isEmpty)
        #expect(!watcher.isAwaiting(now: fixture.now))
        error["tokens"] = ["input": 1, "output": 4, "reasoning": 2]
        try fixture.update("msg_error", metadata: error)
        let record = try #require(watcher.poll(now: fixture.now).first)
        #expect(record.aborted)
        #expect(record.outputTokens == 6)
    }

    @Test func staleIncompleteAndSupersededRowsNeverReactivate() throws {
        let fixture = try OpenCodeFixture()
        let old = OpenCodeFixture.epoch - 700_000
        try fixture.message("msg_stale", created: old, updated: old, metadata: OpenCodeFixture.assistant(completed: false, created: old), seq: 0)
        let watcher = OpenCodeLogWatcher(roots: [fixture.root])
        #expect(watcher.poll(now: fixture.now).isEmpty)
        #expect(!watcher.isAwaiting(now: fixture.now))
        try fixture.message("msg_new", metadata: OpenCodeFixture.assistant(), seq: 1)
        #expect(watcher.poll(now: fixture.now).count == 1)
        try fixture.update("msg_stale", metadata: OpenCodeFixture.assistant(completed: false, created: old))
        #expect(watcher.poll(now: fixture.now).isEmpty)
        #expect(!watcher.isAwaiting(now: fixture.now))
    }

    @Test func legacySQLiteUsesPartTimingAndGenerationCompletionBeforeTools() throws {
        let fixture = try OpenCodeFixture(current: false, legacy: true)
        let start = OpenCodeFixture.epoch
        try fixture.message("msg_legacy", current: false, metadata: OpenCodeFixture.assistant(current: false))
        try fixture.part("prt_reason", message: "msg_legacy", type: "reasoning", time: ["start": start + 2000, "end": start + 6000])
        try fixture.part("prt_text", message: "msg_legacy", type: "text", time: ["start": start + 7000, "end": start + 10_000])
        try fixture.part("prt_finish", message: "msg_legacy", type: "step-finish", at: start + 11_000)
        try fixture.part("prt_tool", message: "msg_legacy", type: "tool", at: start + 18_000, extra: ["state": ["status": "completed", "time": ["start": start + 11_000, "end": start + 18_000]]])
        let watcher = OpenCodeLogWatcher(roots: [fixture.root])
        let record = try #require(watcher.poll(now: fixture.now).first)
        #expect(record.key == "opencode:msg_legacy")
        #expect(record.requestStart == LogDates.milliseconds(Double(start)))
        #expect(record.firstToken == LogDates.milliseconds(Double(start + 2000)))
        #expect(record.firstVisible == LogDates.milliseconds(Double(start + 7000)))
        #expect(record.end == LogDates.milliseconds(Double(start + 11_000)))
        #expect(record.outputTokens == 100)
        #expect(record.makeRecord().ttft == 2)
        #expect(!watcher.isAwaiting(now: fixture.now))
    }

    @Test func legacyPendingFinishIsInactiveBeforeMessageCleanup() throws {
        let fixture = try OpenCodeFixture(current: false, legacy: true, wal: true)
        try fixture.message("msg_legacy_pending", current: false, metadata: OpenCodeFixture.assistant(current: false, completed: false))
        let watcher = OpenCodeLogWatcher(roots: [fixture.root])
        #expect(watcher.poll(now: fixture.now).isEmpty)
        #expect(watcher.isAwaiting(now: fixture.now))
        #expect(watcher.lastRequestAt == LogDates.milliseconds(Double(OpenCodeFixture.epoch)))
        var finished = OpenCodeFixture.assistant(current: false, completed: false)
        finished["finish"] = "tool-calls"
        try fixture.part("prt_finish", message: "msg_legacy_pending", type: "step-finish")
        try fixture.update("msg_legacy_pending", current: false, metadata: finished)
        #expect(watcher.poll(now: fixture.now).count == 1)
        #expect(!watcher.isAwaiting(now: fixture.now))
    }

    @Test func legacyJSONRewritesAndMigrationHaveOneStableKey() throws {
        let fixture = try OpenCodeFixture(current: false, legacy: true)
        let storage = fixture.root.appendingPathComponent("storage")
        let messageDir = storage.appendingPathComponent("message/ses_fixture")
        let partDir = storage.appendingPathComponent("part/msg_json")
        try FileManager.default.createDirectory(at: messageDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: partDir, withIntermediateDirectories: true)
        let file = messageDir.appendingPathComponent("msg_json.json")
        var message = OpenCodeFixture.assistant(current: false, completed: false)
        message["id"] = "msg_json"
        message["sessionID"] = "ses_fixture"
        try Data(OpenCodeFixture.json(message).utf8).write(to: file)
        let watcher = OpenCodeLogWatcher(roots: [fixture.root])
        #expect(watcher.poll(now: fixture.now).isEmpty)
        #expect(watcher.hasLogs)
        #expect(watcher.isAwaiting(now: fixture.now))
        message["time"] = ["created": OpenCodeFixture.epoch, "completed": OpenCodeFixture.epoch + 20_000]
        message["finish"] = "stop"
        try Data(OpenCodeFixture.json(["type": "reasoning", "time": ["start": OpenCodeFixture.epoch + 3000, "end": OpenCodeFixture.epoch + 9000]]).utf8)
            .write(to: partDir.appendingPathComponent("prt_reason.json"))
        try Data(OpenCodeFixture.json(message).utf8).write(to: file, options: .atomic)
        let result = watcher.poll(now: fixture.now.addingTimeInterval(1))
        #expect(result.count == 1)
        #expect(result.first?.firstToken == LogDates.milliseconds(Double(OpenCodeFixture.epoch + 3000)))
        #expect(!watcher.isAwaiting(now: fixture.now))
        try fixture.message("msg_json", current: false, metadata: message)
        #expect(watcher.poll(now: fixture.now.addingTimeInterval(2)).isEmpty)
    }

    @Test func detectsSchemaAddedAfterOpeningAndIgnoresEmptyRoots() throws {
        let fixture = try OpenCodeFixture(current: false)
        let watcher = OpenCodeLogWatcher(roots: [fixture.root])
        #expect(watcher.poll(now: fixture.now).isEmpty)
        #expect(!watcher.hasLogs)
        try fixture.currentSchema()
        try fixture.message("msg_new_schema", metadata: OpenCodeFixture.assistant())
        #expect(watcher.poll(now: fixture.now).count == 1)
        #expect(watcher.hasLogs)
    }

    @Test func oldReadableTelemetryDoesNotBecomeLiveOnDiscovery() throws {
        let fixture = try OpenCodeFixture()
        let old = OpenCodeFixture.epoch - 10 * 24 * 3600 * 1000
        try fixture.message("msg_old", created: old, updated: old + 20_000, metadata: OpenCodeFixture.assistant(created: old))
        let watcher = OpenCodeLogWatcher(roots: [fixture.root])
        #expect(watcher.poll(now: fixture.now).isEmpty)
        #expect(watcher.hasLogs)
        #expect(!watcher.isAwaiting(now: fixture.now))
        #expect(watcher.lastActivity == nil)
    }

    @Test func backfillIsPagedAndLargeDatabaseIsNeverLoadedOrCopied() throws {
        let fixture = try OpenCodeFixture()
        for index in 0..<400 {
            let created = OpenCodeFixture.epoch + Int64(index)
            try fixture.message("msg_page_\(index)", created: created, metadata: OpenCodeFixture.assistant(created: created), seq: index)
        }
        // A sparse 2.2 GB SQLite file exposes an accidental Data(contentsOf: database)
        // without writing a multi-gigabyte fixture or reading unrelated table payload.
        let handle = try FileHandle(forWritingTo: fixture.database)
        try handle.truncate(atOffset: 2_200_000_000)
        try handle.close()
        let watcher = OpenCodeLogWatcher(roots: [fixture.root])
        let first = watcher.poll(now: fixture.now)
        #expect(first.count <= 136)
        var keys = Set(first.map(\.key))
        for _ in 0..<8 { keys.formUnion(watcher.poll(now: fixture.now).map(\.key)) }
        #expect(keys.count == 400)
        let bytes = (try FileManager.default.attributesOfItem(atPath: fixture.database.path)[.size] as? NSNumber)?.uint64Value
        #expect(bytes == 2_200_000_000)
        #expect(watcher.poll(now: fixture.now).isEmpty)
    }

    @Test func malformedUsageAndMissingPartTimesDoNotCreateSyntheticLatency() throws {
        let fixture = try OpenCodeFixture(current: false, legacy: true)
        var malformed = OpenCodeFixture.assistant(current: false)
        malformed["tokens"] = ["input": 1, "output": -1, "reasoning": 2]
        try fixture.message("msg_invalid", current: false, metadata: malformed)
        try fixture.message("msg_no_parts", current: false, metadata: OpenCodeFixture.assistant(current: false))
        let watcher = OpenCodeLogWatcher(roots: [fixture.root])
        let result = watcher.poll(now: fixture.now)
        #expect(result.count == 1)
        #expect(result.first?.key == "opencode:msg_no_parts")
        #expect(result.first?.firstToken == nil)
        #expect(result.first?.makeRecord().ttft == nil)
        #expect(result.first?.makeRecord().generation == nil)
    }
}
