import CSQLite
import Foundation

/// Passive, read-only Antigravity telemetry, for both the editor and the `agy` terminal agent.
/// Call from one queue, like SessionLogWatcher.
///
/// Each conversation is a SQLite file in `~/.gemini/antigravity/conversations`. Its
/// `gen_metadata` table holds one protobuf per model call, and Antigravity measures
/// the call itself: time to first token, streaming time and token counts. The `steps`
/// table says when the call was made. Field numbers below were read from the schema
/// embedded in Antigravity 2.21 and checked against a real conversation.
///
/// The same rows also hold the prompt. Only numbers, the model name and ids are decoded;
/// text fields are skipped without being turned into strings.
public final class AntigravityLogWatcher {
    public let roots: [URL]
    public private(set) var hasLogs = false
    public private(set) var latestModel: String?
    public private(set) var lastActivity: Date?
    public private(set) var lastRequestAt: Date?

    /// New calls land in the write-ahead log first, so the main file alone can look unchanged.
    private struct Stamp: Equatable {
        var size: Int
        var modified: Date
        var logSize: Int?
        var logModified: Date?
    }

    private final class Conversation {
        var stamp: Stamp?
        /// Calls below this index are finished with: emitted, or permanently unusable.
        var nextIndex: Int64 = 0
        var awaitingSince: Date?
    }

    private let backfillWindow: TimeInterval
    private var conversations: [String: Conversation] = [:]
    private var emitted = Set<String>()
    private var lastScan = Date.distantPast
    private var files: [URL] = []
    private var latestModelAt = Date.distantPast

    private static let scanInterval: TimeInterval = 4
    private static let staleAfter: TimeInterval = 600
    /// The newest call keeps its whole prompt until the next one replaces it.
    private static let maxBlobBytes = 64 * 1024 * 1024
    private static let maxRowsPerPoll = 256

    public convenience init(
        home: URL = FileManager.default.homeDirectoryForCurrentUser,
        backfillWindow: TimeInterval = 7 * 24 * 3600
    ) {
        self.init(roots: [home.appendingPathComponent(".gemini/antigravity/conversations")], backfillWindow: backfillWindow)
    }

    /// Each root is a folder of `<conversation id>.db` files.
    public init(roots: [URL], backfillWindow: TimeInterval = 7 * 24 * 3600) {
        self.roots = roots.map(\.standardizedFileURL)
        self.backfillWindow = max(0, backfillWindow)
    }

    /// A call has been made, by Antigravity's own record, and its reply is not complete.
    public func isAwaiting(now: Date = Date()) -> Bool {
        conversations.values.contains { conversation in
            conversation.awaitingSince.map { $0 <= now && now.timeIntervalSince($0) < Self.staleAfter } ?? false
        }
    }

    public func poll(now: Date = Date()) -> [LogRecord] {
        if now.timeIntervalSince(lastScan) >= Self.scanInterval {
            lastScan = now
            files = roots.flatMap { root in
                ((try? FileManager.default.contentsOfDirectory(
                    at: root, includingPropertiesForKeys: [.contentModificationDateKey, .fileSizeKey]
                )) ?? []).filter { $0.pathExtension == "db" }
            }
            let paths = Set(files.map(\.path))
            conversations = conversations.filter { paths.contains($0.key) }
            if !hasLogs { hasLogs = probe() }
        }

        let cutoff = now.addingTimeInterval(-backfillWindow)
        var records: [LogRecord] = []
        for file in files {
            guard let values = try? file.resourceValues(forKeys: [.contentModificationDateKey, .fileSizeKey]),
                  let modified = values.contentModificationDate, let size = values.fileSize
            else { continue }
            let log = try? AntigravityDatabase.writeAheadLog(of: file).resourceValues(forKeys: [.contentModificationDateKey, .fileSizeKey])
            let stamp = Stamp(size: size, modified: modified, logSize: log?.fileSize, logModified: log?.contentModificationDate)
            let conversation = conversations[file.path] ?? Conversation()
            conversations[file.path] = conversation
            // An untouched file has nothing new. Old conversations are never opened at all.
            guard conversation.stamp != stamp, max(modified, log?.contentModificationDate ?? modified) >= cutoff else { continue }
            guard let database = AntigravityDatabase(url: file) else { continue }
            let id = file.deletingPathExtension().lastPathComponent
            guard let result = database.read(conversation: id, from: conversation.nextIndex, limit: Self.maxRowsPerPoll, maxBlob: Self.maxBlobBytes) else {
                continue
            }
            hasLogs = true
            conversation.nextIndex = result.nextIndex
            conversation.awaitingSince = result.awaitingSince
            if let request = result.lastRequestAt { lastRequestAt = max(lastRequestAt ?? request, request) }
            if let activity = result.lastActivity { lastActivity = max(lastActivity ?? activity, activity) }
            for record in result.records where record.end >= cutoff && emitted.insert(record.key).inserted {
                records.append(record)
                if record.end >= latestModelAt {
                    latestModelAt = record.end
                    latestModel = record.model
                }
            }
            // More rows than one pass reads: leave the stamp unset so the next poll continues.
            if !result.truncated { conversation.stamp = stamp }
        }
        return records.sorted { $0.end < $1.end }
    }

    /// Whether Antigravity keeps conversations here in a form this reader understands,
    /// even if none is recent enough to read. One look at the newest file answers it.
    private func probe() -> Bool {
        let newest = files.max { lhs, rhs in
            let left = (try? lhs.resourceValues(forKeys: [.contentModificationDateKey]))?.contentModificationDate ?? .distantPast
            let right = (try? rhs.resourceValues(forKeys: [.contentModificationDateKey]))?.contentModificationDate ?? .distantPast
            return left < right
        }
        return newest.flatMap(AntigravityDatabase.init(url:))?.hasGenerations ?? false
    }
}

// MARK: - One call

/// What one `gen_metadata` row says about a model call.
struct AntigravityGeneration: Equatable {
    var stepIndices: [Int64] = []
    var executionID = ""
    var model: String?
    var inputTokens: Int?
    var outputTokens: Int?
    var cacheWriteTokens: Int?
    var cacheReadTokens: Int?
    var thinkingTokens: Int?
    var timeToFirstToken: Double?
    var streamingDuration: Double?

    /// A stream shorter than this is one or two network chunks: it shows that the reply
    /// arrived, not how fast it was written.
    static let shortestTimedStream: TimeInterval = 1

    /// `CortexStepGeneratorMetadata`: chat_model = 1, step_indices = 2, execution_id = 4.
    init?(_ data: Data) {
        guard let fields = Protobuf.fields(data) else { return nil }
        var chatModel: Data?
        for field in fields {
            switch (field.number, field.value) {
            case (1, .bytes(let value)): chatModel = value
            case (2, .bytes(let value)): stepIndices += Protobuf.packedVarints(value).map { Int64(truncatingIfNeeded: $0) }
            case (2, .varint(let value)): stepIndices.append(Int64(truncatingIfNeeded: value))
            case (4, .bytes(let value)): executionID = Protobuf.identifier(value) ?? ""
            default: break
            }
        }
        // `ChatModelMetadata`: usage = 4, time_to_first_token = 11, streaming_duration = 12, response_model = 19.
        guard let chatModel, let chat = Protobuf.fields(chatModel) else { return nil }
        for field in chat {
            guard case .bytes(let value) = field.value else { continue }
            switch field.number {
            case 4: readUsage(value)
            case 11: timeToFirstToken = Protobuf.duration(value)
            case 12: streamingDuration = Protobuf.duration(value)
            case 19: model = Protobuf.identifier(value)
            default: break
            }
        }
    }

    /// `ModelUsageStats`: input = 2, output = 3, cache_write = 4, cache_read = 5,
    /// thinking_output = 9. Output already includes thinking.
    private mutating func readUsage(_ data: Data) {
        for field in Protobuf.fields(data) ?? [] {
            guard case .varint(let raw) = field.value, raw <= UInt64(Int32.max) else { continue }
            let value = Int(raw)
            switch field.number {
            case 2: inputTokens = value
            case 3: outputTokens = value
            case 4: cacheWriteTokens = value
            case 5: cacheReadTokens = value
            case 9: thinkingTokens = value
            default: break
            }
        }
    }

    /// The call is finished: Antigravity writes usage and timing when the reply ends.
    var isComplete: Bool {
        (outputTokens ?? 0) > 0 && (timeToFirstToken != nil || streamingDuration != nil)
    }

    func record(conversation: String, index: Int64, startedAt: Date) -> LogRecord? {
        guard isComplete, let output = outputTokens else { return nil }
        let streaming = max(streamingDuration ?? 0, 0)
        let firstToken = timeToFirstToken.map { startedAt.addingTimeInterval(max($0, 0)) }
        let end = (firstToken ?? startedAt).addingTimeInterval(streaming)
        let total = end.timeIntervalSince(startedAt)
        let thinking = min(thinkingTokens ?? 0, output)

        // Antigravity's first token is the first visible one: thinking is over by then.
        // So the streaming window holds only the visible reply, and counting every output
        // token in it would inflate the rate. When the window is too short to be a rate at
        // all, the honest figure is the whole call.
        var speed: Double?
        if streaming >= Self.shortestTimedStream, output > thinking {
            speed = Double(output - thinking) / streaming
        } else if total >= 0.05 {
            speed = Double(output) / total
        }
        let cached = cacheReadTokens
        let input = (inputTokens ?? 0) + (cached ?? 0) + (cacheWriteTokens ?? 0)
        return LogRecord(
            key: "antigravity:\(conversation):\(executionID.isEmpty ? "-" : executionID):\(index)",
            harness: "Antigravity", model: model ?? "unknown", provider: "google",
            requestStart: startedAt, firstToken: firstToken, firstVisible: firstToken, end: end,
            outputTokens: output, reasoningTokens: thinking > 0 ? thinking : nil,
            inputTokens: input > 0 ? input : nil, cachedInputTokens: cached, speed: speed
        )
    }
}

// MARK: - Database

private final class AntigravityDatabase {
    struct Result {
        var records: [LogRecord] = []
        var nextIndex: Int64
        var truncated = false
        var awaitingSince: Date?
        var lastRequestAt: Date?
        var lastActivity: Date?
    }

    private var connection: OpaquePointer?

    static func writeAheadLog(of url: URL) -> URL {
        URL(fileURLWithPath: url.path + "-wal")
    }

    init?(url: URL) {
        // These files are in write-ahead-log mode. Opening one normally makes SQLite create
        // its `-shm` and `-wal` companions, and a read-only connection cannot remove them
        // again, so a plain open would leave files behind in Antigravity's folder. With no
        // log present the main file is complete on its own and is read as an immutable
        // snapshot, which creates nothing. With a log present the companions already exist.
        var flags = SQLITE_OPEN_READONLY | SQLITE_OPEN_NOMUTEX
        var name = url.path
        if !FileManager.default.fileExists(atPath: Self.writeAheadLog(of: url).path),
           let escaped = url.path.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) {
            flags |= SQLITE_OPEN_URI
            name = "file:\(escaped)?immutable=1"
        }
        guard sqlite3_open_v2(name, &connection, flags, nil) == SQLITE_OK else {
            if let connection { sqlite3_close(connection) }
            connection = nil
            return nil
        }
        // Antigravity may be writing. Give up quickly and come back on the next poll.
        sqlite3_busy_timeout(connection, 25)
    }

    deinit {
        if let connection { sqlite3_close(connection) }
    }

    var hasGenerations: Bool {
        integers("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('gen_metadata', 'steps')", [])?.first?.first == 2
    }

    /// Reads calls from `first` on. Returns nil when the file is not a conversation or is busy.
    func read(conversation: String, from first: Int64, limit: Int, maxBlob: Int) -> Result? {
        guard let rows = integers("SELECT idx, length(data) FROM gen_metadata WHERE idx >= ? ORDER BY idx LIMIT ?", [first, Int64(limit + 1)]) else {
            return nil
        }
        var result = Result(nextIndex: first)
        result.truncated = rows.count > limit
        var blocked = false
        let newest = result.truncated ? nil : rows.last?[0]
        for row in rows.prefix(limit) {
            let index = row[0]
            var finished = true
            if row[1] > 0, row[1] <= Int64(maxBlob), let data = blob("SELECT data FROM gen_metadata WHERE idx = ?", index),
               let generation = AntigravityGeneration(data) {
                if generation.isComplete {
                    if let step = generation.stepIndices.first, let started = stepTime(step, field: 1),
                       let record = generation.record(conversation: conversation, index: index, startedAt: started) {
                        result.records.append(record)
                        result.lastActivity = max(result.lastActivity ?? record.end, record.end)
                    }
                } else if index == newest {
                    // Written before its reply finished; look again when the file changes.
                    // An unfinished call with later calls after it was abandoned and never will be.
                    finished = false
                }
            }
            if !finished { blocked = true }
            if !blocked { result.nextIndex = index + 1 }
        }
        readLatestStep(into: &result)
        return result
    }

    /// The newest model step tells when the last request went out, and whether its reply is still due.
    /// `CortexStepMetadata`: created_at = 1, completed_at = 8. Step type 15 is the model's response.
    private func readLatestStep(into result: inout Result) {
        guard let row = integers("SELECT idx FROM steps WHERE step_type = 15 ORDER BY idx DESC LIMIT 1", [])?.first,
              let created = stepTime(row[0], field: 1)
        else { return }
        result.lastRequestAt = created
        result.lastActivity = max(result.lastActivity ?? created, created)
        if stepTime(row[0], field: 8) == nil { result.awaitingSince = created }
    }

    private func stepTime(_ index: Int64, field: Int) -> Date? {
        guard let data = blob("SELECT metadata FROM steps WHERE idx = ? AND length(metadata) <= 1048576", index),
              let fields = Protobuf.fields(data)
        else { return nil }
        for candidate in fields where candidate.number == field {
            if case .bytes(let value) = candidate.value { return Protobuf.timestamp(value) }
        }
        return nil
    }

    private func integers(_ sql: String, _ arguments: [Int64]) -> [[Int64]]? {
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(connection, sql, -1, &statement, nil) == SQLITE_OK, let statement else { return nil }
        defer { sqlite3_finalize(statement) }
        for (offset, value) in arguments.enumerated() { sqlite3_bind_int64(statement, Int32(offset + 1), value) }
        var rows: [[Int64]] = []
        while true {
            let step = sqlite3_step(statement)
            if step == SQLITE_DONE { return rows }
            guard step == SQLITE_ROW else { return nil }
            rows.append((0..<sqlite3_column_count(statement)).map { sqlite3_column_int64(statement, $0) })
        }
    }

    private func blob(_ sql: String, _ argument: Int64) -> Data? {
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(connection, sql, -1, &statement, nil) == SQLITE_OK, let statement else { return nil }
        defer { sqlite3_finalize(statement) }
        sqlite3_bind_int64(statement, 1, argument)
        guard sqlite3_step(statement) == SQLITE_ROW, let bytes = sqlite3_column_blob(statement, 0) else { return nil }
        return Data(bytes: bytes, count: Int(sqlite3_column_bytes(statement, 0)))
    }
}

// MARK: - Protobuf

/// Just enough of the protobuf wire format to pick numbered fields out of a message
/// without its schema. Unknown fields are stepped over, never interpreted.
enum Protobuf {
    enum Value: Equatable {
        case varint(UInt64)
        case fixed64(UInt64)
        case fixed32(UInt32)
        case bytes(Data)
    }

    struct Field: Equatable {
        var number: Int
        var value: Value
    }

    /// The top-level fields of a message, or nil if the bytes are not a well-formed one.
    static func fields(_ data: Data) -> [Field]? {
        var fields: [Field] = []
        var index = data.startIndex
        while index < data.endIndex {
            guard let key = varint(data, &index) else { return nil }
            let number = Int(key >> 3)
            guard number > 0 else { return nil }
            switch key & 7 {
            case 0:
                guard let value = varint(data, &index) else { return nil }
                fields.append(Field(number: number, value: .varint(value)))
            case 1:
                guard data.endIndex - index >= 8 else { return nil }
                let value = data[index..<index + 8].enumerated().reduce(UInt64(0)) { $0 | UInt64($1.element) << (8 * UInt64($1.offset)) }
                fields.append(Field(number: number, value: .fixed64(value)))
                index += 8
            case 2:
                guard let length = varint(data, &index), length <= UInt64(data.endIndex - index) else { return nil }
                let end = index + Int(length)
                fields.append(Field(number: number, value: .bytes(data[index..<end])))
                index = end
            case 5:
                guard data.endIndex - index >= 4 else { return nil }
                let value = data[index..<index + 4].enumerated().reduce(UInt32(0)) { $0 | UInt32($1.element) << (8 * UInt32($1.offset)) }
                fields.append(Field(number: number, value: .fixed32(value)))
                index += 4
            default:
                return nil
            }
        }
        return fields
    }

    static func packedVarints(_ data: Data) -> [UInt64] {
        var values: [UInt64] = []
        var index = data.startIndex
        while index < data.endIndex, let value = varint(data, &index) { values.append(value) }
        return values
    }

    /// `google.protobuf.Duration`: seconds = 1, nanos = 2.
    static func duration(_ data: Data) -> Double? {
        guard let parts = secondsAndNanos(data) else { return nil }
        let value = Double(parts.seconds) + Double(parts.nanos) / 1_000_000_000
        return value >= 0 && value < 86_400 ? value : nil
    }

    /// `google.protobuf.Timestamp`: seconds = 1, nanos = 2.
    static func timestamp(_ data: Data) -> Date? {
        guard let parts = secondsAndNanos(data), parts.seconds > 1_000_000_000, parts.seconds < 10_000_000_000 else { return nil }
        return Date(timeIntervalSince1970: Double(parts.seconds) + Double(parts.nanos) / 1_000_000_000)
    }

    /// A short ASCII token such as a model name or UUID. Anything else is refused, so free
    /// text sitting in a neighbouring field can never be mistaken for one.
    static func identifier(_ data: Data) -> String? {
        guard !data.isEmpty, data.count <= 128 else { return nil }
        let allowed = data.allSatisfy { byte in
            (byte >= 0x30 && byte <= 0x39) || (byte >= 0x41 && byte <= 0x5A) || (byte >= 0x61 && byte <= 0x7A)
                || byte == 0x2D || byte == 0x2E || byte == 0x5F || byte == 0x2F || byte == 0x3A
        }
        return allowed ? String(decoding: data, as: UTF8.self) : nil
    }

    private static func secondsAndNanos(_ data: Data) -> (seconds: Int64, nanos: Int64)? {
        guard let fields = fields(data) else { return nil }
        var seconds: Int64 = 0
        var nanos: Int64 = 0
        for field in fields {
            guard case .varint(let value) = field.value else { return nil }
            if field.number == 1 { seconds = Int64(bitPattern: value) }
            if field.number == 2 { nanos = Int64(bitPattern: value) }
        }
        return (seconds, nanos)
    }

    private static func varint(_ data: Data, _ index: inout Data.Index) -> UInt64? {
        var value: UInt64 = 0
        var shift: UInt64 = 0
        while index < data.endIndex, shift < 64 {
            let byte = data[index]
            index += 1
            value |= UInt64(byte & 0x7F) << shift
            if byte & 0x80 == 0 { return value }
            shift += 7
        }
        return nil
    }
}
