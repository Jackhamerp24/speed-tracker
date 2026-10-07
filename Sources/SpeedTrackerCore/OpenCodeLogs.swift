import Foundation
import CoreFoundation
import CSQLite

/// Passive, read-only OpenCode telemetry. Call from one queue, like SessionLogWatcher.
/// SQL projects metadata only; prompt, reply, tool output and credentials are never returned.
public final class OpenCodeLogWatcher {
    public let roots: [URL]
    public private(set) var hasLogs = false
    public private(set) var latestModel: String?
    public private(set) var lastActivity: Date?
    public private(set) var lastRequestAt: Date?

    private let backfillWindow: TimeInterval
    private var databases: [String: OpenCodeDatabase] = [:]
    private var scans: [String: FileManager.DirectoryEnumerator] = [:]
    private var nextScan: [String: Date] = [:]
    private var files: [String: FileStamp] = [:]
    private var pending: [String: Pending] = [:]
    private var emitted: [String: Date] = [:]
    private var latestSession: [String: (created: Date, id: String)] = [:]
    private var latestModelAt = Date.distantPast
    private var readableJSON = false
    private var jsonBytesRemaining = 0
    private static let staleAfter: TimeInterval = 600
    private static let maxPending = 512
    private static let maxRemembered = 32_768
    private static let maxFileBytes = 1024 * 1024

    private struct FileStamp: Equatable {
        let size: Int
        let modified: Date
    }

    private struct Pending {
        var telemetry: OpenCodeTelemetry
        let database: String?
        let table: String?
        let file: URL?
        let storage: URL?
    }

    public convenience init(
        home: URL = FileManager.default.homeDirectoryForCurrentUser,
        backfillWindow: TimeInterval = 7 * 24 * 3600
    ) {
        var roots = [home.appendingPathComponent(".local/share/opencode"), home.appendingPathComponent("Library/Application Support/opencode")]
        if let xdg = ProcessInfo.processInfo.environment["XDG_DATA_HOME"], xdg.hasPrefix("/") {
            roots.insert(URL(fileURLWithPath: xdg).appendingPathComponent("opencode"), at: 0)
        }
        self.init(roots: roots, backfillWindow: backfillWindow)
    }

    /// Roots may name the OpenCode data directory, its storage directory, or a database.
    public init(roots: [URL], backfillWindow: TimeInterval = 7 * 24 * 3600) {
        var paths = Set<String>()
        self.roots = roots.map(\.standardizedFileURL).filter { paths.insert($0.path).inserted }
        self.backfillWindow = max(0, backfillWindow)
    }

    public func isAwaiting(now: Date = Date()) -> Bool {
        pending.values.contains {
            let telemetry = $0.telemetry
            return telemetry.awaiting && latestSession[telemetry.session]?.id == telemetry.id && telemetry.created <= now
                && now.timeIntervalSince(telemetry.updated) < Self.staleAfter
                && now.timeIntervalSince(telemetry.created) < Self.staleAfter
        }
    }

    public func poll(now: Date = Date()) -> [LogRecord] {
        jsonBytesRemaining = 4 * 1024 * 1024
        var records: [LogRecord] = []
        let cutoff = now.addingTimeInterval(-backfillWindow)
        for root in roots {
            let databaseURL = root.pathExtension == "db" ? root : root.appendingPathComponent("opencode.db")
            if FileManager.default.isReadableFile(atPath: databaseURL.path) {
                if databases[databaseURL.path]?.isCurrentFile != true {
                    databases[databaseURL.path] = OpenCodeDatabase(url: databaseURL, cutoff: cutoff)
                }
            } else {
                databases.removeValue(forKey: databaseURL.path)
            }
            let storage = root.lastPathComponent == "storage" ? root : root.appendingPathComponent("storage")
            if root.pathExtension != "db" {
                records += scanJSON(storage: storage, cutoff: cutoff, now: now)
            }
        }
        for (path, database) in databases {
            let unresolved = pending.values.filter { $0.database == path }
            let requests = unresolved.compactMap { value -> (String, String)? in
                guard let table = value.table else { return nil }
                return (table, value.telemetry.id)
            }
            for (table, telemetry) in database.poll(refresh: requests, now: now) {
                records += accept(telemetry, database: path, table: table, file: nil, storage: nil, cutoff: cutoff)
            }
        }
        // Entity files can be rewritten without their containing directory's mtime changing.
        // Keep only unresolved messages; completed files are visited by the bounded periodic scan.
        for value in Array(pending.values) where value.file != nil {
            guard let file = value.file, let storage = value.storage,
                  let telemetry = readJSONMessage(file, storage: storage)
            else { continue }
            records += accept(telemetry, database: nil, table: nil, file: file, storage: storage, cutoff: cutoff)
        }
        pending = pending.filter { $0.value.telemetry.updated >= cutoff }
        if pending.count > Self.maxPending {
            let keep = pending.sorted { $0.value.telemetry.updated > $1.value.telemetry.updated }.prefix(Self.maxPending)
            pending = Dictionary(uniqueKeysWithValues: keep.map { ($0.key, $0.value) })
        }
        emitted = emitted.filter { $0.value >= cutoff }
        trim(&emitted, maximum: Self.maxRemembered)
        latestSession = latestSession.filter { $0.value.created >= cutoff }
        if latestSession.count > Self.maxRemembered {
            latestSession = Dictionary(uniqueKeysWithValues: latestSession.sorted { $0.value.created > $1.value.created }
                .prefix(Self.maxRemembered).map { ($0.key, $0.value) })
        }
        if files.count > Self.maxRemembered {
            let keep = files.sorted { $0.value.modified > $1.value.modified }.prefix(Self.maxRemembered)
            files = Dictionary(uniqueKeysWithValues: keep.map { ($0.key, $0.value) })
        }
        hasLogs = readableJSON || databases.values.contains { $0.hasTelemetry }
        return records.sorted { $0.end < $1.end }
    }

    private func trim(_ dates: inout [String: Date], maximum: Int) {
        guard dates.count > maximum else { return }
        dates = Dictionary(uniqueKeysWithValues: dates.sorted { $0.value > $1.value }.prefix(maximum).map { ($0.key, $0.value) })
    }

    private func accept(
        _ telemetry: OpenCodeTelemetry, database: String?, table: String?, file: URL?, storage: URL?, cutoff: Date
    ) -> [LogRecord] {
        guard telemetry.updated >= cutoff else { return [] }
        let key = telemetry.key
        // Historical incomplete rows must not resurrect a session that already finished a newer call.
        let previous = latestSession[telemetry.session]
        let newest = previous.map {
            telemetry.created > $0.created || (telemetry.created == $0.created && telemetry.id >= $0.id)
        } ?? true
        if newest {
            latestSession[telemetry.session] = (telemetry.created, telemetry.id)
        }
        if telemetry.created >= latestModelAt {
            latestModelAt = telemetry.created
            latestModel = telemetry.model
        }
        if let start = telemetry.requestStart { lastRequestAt = max(lastRequestAt ?? start, start) }
        // Activity is the source timestamp, never the time we happened to discover old files.
        lastActivity = max(lastActivity ?? telemetry.updated, telemetry.updated)
        if telemetry.record != nil {
            pending.removeValue(forKey: key)
        } else {
            // Finished rows with delayed usage stay refreshable, but never count as awaiting.
            pending[key] = Pending(telemetry: telemetry, database: database, table: table, file: file, storage: storage)
        }
        guard let record = telemetry.record, record.end >= cutoff, emitted[key] == nil else { return [] }
        emitted[key] = record.end
        return [record]
    }

    private func scanJSON(storage: URL, cutoff: Date, now: Date) -> [LogRecord] {
        let messageRoot = storage.appendingPathComponent("message")
        if scans[storage.path] == nil, now >= (nextScan[storage.path] ?? .distantPast) {
            scans[storage.path] = FileManager.default.enumerator(
                at: messageRoot, includingPropertiesForKeys: [.isRegularFileKey, .fileSizeKey, .contentModificationDateKey],
                options: [.skipsHiddenFiles, .skipsPackageDescendants]
            )
            nextScan[storage.path] = now.addingTimeInterval(4)
        }
        guard let enumerator = scans[storage.path] else { return [] }
        var result: [LogRecord] = []
        // Enumeration and reads both have a per-poll ceiling; retain the iterator, not all paths.
        for _ in 0..<256 {
            guard let file = enumerator.nextObject() as? URL else {
                scans.removeValue(forKey: storage.path)
                break
            }
            guard file.pathExtension == "json",
                  let values = try? file.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey, .contentModificationDateKey]),
                  values.isRegularFile == true, let size = values.fileSize, size <= Self.maxFileBytes,
                  let modified = values.contentModificationDate, modified >= cutoff || !readableJSON
            else { continue }
            let stamp = FileStamp(size: size, modified: modified)
            guard files[file.path] != stamp, let telemetry = readJSONMessage(file, storage: storage) else { continue }
            files[file.path] = stamp
            readableJSON = true
            result += accept(telemetry, database: nil, table: nil, file: file, storage: storage, cutoff: cutoff)
        }
        return result
    }

    private func readJSONMessage(_ file: URL, storage: URL) -> OpenCodeTelemetry? {
        guard let object = json(file), object["role"] as? String == "assistant",
              let id = object["id"] as? String,
              let session = object["sessionID"] as? String,
              Self.safeComponent(id), Self.safeComponent(session)
        else { return nil }
        let partRoot = storage.appendingPathComponent("part").appendingPathComponent(id)
        var parts: [[String: Any]] = []
        var partsComplete = true
        if let iterator = FileManager.default.enumerator(
            at: partRoot, includingPropertiesForKeys: [.isRegularFileKey], options: [.skipsHiddenFiles, .skipsSubdirectoryDescendants]
        ) {
            for _ in 0..<128 {
                guard let partFile = iterator.nextObject() as? URL else { break }
                if partFile.pathExtension == "json", let part = json(partFile) {
                    // Retain telemetry fields only, not text or tool content.
                    parts.append([
                        "type": part["type"] ?? NSNull(), "time": part["time"] ?? NSNull(),
                        "synthetic": part["synthetic"] ?? false, "ignored": part["ignored"] ?? false,
                        "state": ["status": (part["state"] as? [String: Any])?["status"] ?? NSNull()]
                    ])
                } else if partFile.pathExtension == "json" { partsComplete = false }
            }
            if iterator.nextObject() != nil { partsComplete = false }
        }
        let modified = (try? FileManager.default.attributesOfItem(atPath: file.path))?[.modificationDate] as? Date
        return OpenCodeTelemetry(id: id, session: session, current: false, metadata: object, parts: parts, updated: modified, partsComplete: partsComplete)
    }

    private static func safeComponent(_ value: String) -> Bool {
        !value.isEmpty && !value.contains("/") && !value.contains("\\") && value != "." && value != ".."
    }

    private func json(_ file: URL) -> [String: Any]? {
        // Stored URL resource values can cache the pre-rewrite size across atomic replacement.
        guard let attributes = try? FileManager.default.attributesOfItem(atPath: file.path),
              attributes[.type] as? FileAttributeType == .typeRegular,
              let count = (attributes[.size] as? NSNumber)?.intValue, count <= Self.maxFileBytes,
              count + 1 <= jsonBytesRemaining, let handle = try? FileHandle(forReadingFrom: file)
        else { return nil }
        defer { try? handle.close() }
        guard let data = try? handle.read(upToCount: count + 1) else { return nil }
        jsonBytesRemaining -= data.count
        guard data.count == count else { return nil }
        return try? JSONSerialization.jsonObject(with: data) as? [String: Any]
    }
}

/// A small metadata projection, shared by all three persistence generations.
private struct OpenCodeTelemetry {
    let id: String
    let session: String
    let model: String
    let provider: String
    let created: Date
    let updated: Date
    let requestStart: Date?
    let terminal: Bool
    let awaiting: Bool
    let record: LogRecord?
    var key: String { "opencode:\(id)" }

    init?(id: String, session: String, current: Bool, metadata: [String: Any], parts: [[String: Any]], updated: Date?, partsComplete: Bool = true) {
        let time = metadata["time"] as? [String: Any] ?? [:]
        let modelRef = metadata["model"] as? [String: Any] ?? [:]
        guard let created = LogDates.parse(time["created"]),
              let model = (current ? modelRef["id"] : metadata["modelID"]) as? String, !model.isEmpty
        else { return nil }
        self.id = id
        self.session = session
        self.model = model
        provider = ((current ? modelRef["providerID"] : metadata["providerID"]) as? String) ?? ""
        self.created = created
        let completed = LogDates.parse(time["completed"])
        let finish = metadata["finish"] as? String
        let failed = metadata["error"] != nil && !(metadata["error"] is NSNull) || finish == "error"
        terminal = completed != nil || finish != nil || failed
        let generated = parts.filter {
            let type = $0["type"] as? String
            return (type == "text" || type == "reasoning") && !($0["synthetic"] as? Bool ?? false)
                && !($0["ignored"] as? Bool ?? false)
        }
        let toolPause = parts.contains {
            $0["type"] as? String == "tool" && ["running", "completed", "error"].contains(
                ($0["state"] as? [String: Any])?["status"] as? String ?? ""
            )
        }
        awaiting = partsComplete && !terminal && !toolPause && !(metadata["summary"] as? Bool ?? false)
        // V2's assistant is created lazily by the first content event in current upstream.
        // It is NOT request-sent telemetry. Never turn created/streamed into synthetic TTFT.
        requestStart = current ? nil : created
        self.updated = max(created, updated ?? completed ?? created)
        let first: Date?
        let visible: Date?
        if current {
            let firstPart = parts.first
            let type = firstPart?["type"] as? String
            first = (type == "reasoning" || type == "tool")
                ? LogDates.parse((firstPart?["time"] as? [String: Any])?["created"]) : nil
            let firstVisiblePart = parts.first { ["text", "tool"].contains($0["type"] as? String ?? "") }
            visible = firstVisiblePart?["type"] as? String == "tool"
                ? LogDates.parse((firstVisiblePart?["time"] as? [String: Any])?["created"]) : nil
        } else {
            first = generated.compactMap { LogDates.parse(($0["time"] as? [String: Any])?["start"]) }.min()
            visible = generated.filter { $0["type"] as? String == "text" }
                .compactMap { LogDates.parse(($0["time"] as? [String: Any])?["start"]) }.min()
        }
        // step-finish is persisted before tools run; message.completed can include tool wait.
        let stepEnd = parts.filter { $0["type"] as? String == "step-finish" }
            .compactMap { LogDates.parse($0["persistedAt"]) }.max()
        let end = current ? completed : stepEnd ?? completed
        guard terminal, let end, end >= created,
              let usage = metadata["tokens"] as? [String: Any],
              let output = Self.count(usage["output"])
        else { record = nil; return }
        let reasoning = Self.count(usage["reasoning"])
        guard let totalOutput = Self.sum([output, reasoning ?? 0]) else { record = nil; return }
        let cache = usage["cache"] as? [String: Any] ?? [:]
        let read = Self.count(cache["read"])
        let write = Self.count(cache["write"])
        let input = Self.count(usage["input"]).flatMap { Self.sum([$0, read ?? 0, write ?? 0]) }
        guard totalOutput > 0 || (input ?? 0) > 0 else { record = nil; return }
        let validFirst = partsComplete ? first.flatMap { $0 >= created && $0 <= end ? $0 : nil } : nil
        let validVisible = partsComplete ? visible.flatMap { $0 >= created && $0 <= end ? $0 : nil } : nil
        record = LogRecord(
            key: "opencode:\(id)", harness: "opencode", model: model, provider: provider,
            requestStart: requestStart, firstToken: validFirst, firstVisible: validVisible, end: end,
            outputTokens: totalOutput, reasoningTokens: reasoning, inputTokens: input,
            cachedInputTokens: read, aborted: failed || ["abort", "aborted", "cancelled"].contains(finish ?? "")
        )
    }

    private static func count(_ value: Any?) -> Int? {
        guard let number = value as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID() else { return nil }
        let count = number.doubleValue
        guard count.isFinite, count >= 0, count.rounded(.towardZero) == count, count < Double(Int.max) else { return nil }
        return Int(count)
    }

    private static func sum(_ values: [Int]) -> Int? {
        var sum = 0
        for value in values {
            let (next, overflow) = sum.addingReportingOverflow(value)
            if overflow { return nil }
            sum = next
        }
        return sum
    }
}

private final class OpenCodeDatabase {
    private final class Table {
        let name: String
        let current: Bool
        var highWater: Int64 = 0
        var historyTime: Int64
        var historyID = ""
        var historyDone = false
        var probeDone = false
        var work: [String] = []
        var workOffset = 0
        var queued = Set<String>()

        func enqueue(_ id: String) {
            if queued.insert(id).inserted { work.append(id) }
        }
        var sessionRow: Int64 = 0
        var sessionDone = false
        var sessions: [String] = []
        var indexedHistory = false
        var sessionHistory = false
        var fallbackRow: Int64 = 0
        init(name: String, current: Bool, cutoff: Int64) {
            self.name = name
            self.current = current
            historyTime = cutoff
        }
    }
    private var connection: OpaquePointer?
    private let url: URL
    private let fileNumber: NSNumber?
    var isCurrentFile: Bool {
        ((try? FileManager.default.attributesOfItem(atPath: url.path))?[.systemFileNumber] as? NSNumber) == fileNumber
    }
    private var tables: [Table] = []
    private var schemaVersion: Int64 = -1
    private var dataVersion: Int64 = -1
    private var cutoff: Int64
    private var remainingSteps = 0
    private var remainingJSONBytes = 0
    private var exhausted = false
    private(set) var hasTelemetry = false
    private static let batch = 128
    private static let maxJSONBytes = 4 * 1024 * 1024
    private static let transient = unsafeBitCast(-1, to: sqlite3_destructor_type.self)

    init?(url: URL, cutoff: Date) {
        self.url = url
        fileNumber = (try? FileManager.default.attributesOfItem(atPath: url.path))?[.systemFileNumber] as? NSNumber
        self.cutoff = Int64(cutoff.timeIntervalSince1970 * 1000)
        guard sqlite3_open_v2(url.path, &connection, SQLITE_OPEN_READONLY | SQLITE_OPEN_NOMUTEX, nil) == SQLITE_OK else {
            if let connection { sqlite3_close(connection) }
            connection = nil
            return nil
        }
        sqlite3_busy_timeout(connection, 25)
        sqlite3_progress_handler(connection, 1000, { context in
            guard let context else { return 1 }
            let database = Unmanaged<OpenCodeDatabase>.fromOpaque(context).takeUnretainedValue()
            database.remainingSteps -= 1000
            return database.remainingSteps <= 0 ? 1 : 0
        }, Unmanaged.passUnretained(self).toOpaque())
    }

    deinit {
        sqlite3_progress_handler(connection, 0, nil, nil)
        if let connection { sqlite3_close(connection) }
    }

    func poll(refresh: [(String, String)], now: Date) -> [(String, OpenCodeTelemetry)] {
        remainingSteps = 2_000_000
        remainingJSONBytes = 16 * 1024 * 1024
        exhausted = false
        guard let schema = scalar("PRAGMA schema_version"), let version = scalar("PRAGMA data_version") else { return [] }
        if schema != schemaVersion {
            guard discover() else { return [] }
            schemaVersion = schema
            dataVersion = -1
        }
        let changed = version != dataVersion
        var result: [(String, OpenCodeTelemetry)] = []
        // Refresh by primary key, not global time_updated: upstream updates rows in place in WAL.
        if changed {
            for (tableName, id) in refresh.prefix(512) {
                tables.first(where: { $0.name == tableName })?.enqueue(id)
            }
        }
        var moreInsertions = false
        for table in tables {
            if !table.probeDone {
                // Old-but-supported logs can be detected without replaying the entire database.
                let order = table.indexedHistory ? "time_created DESC" : "rowid DESC"
                if let rows = query("SELECT id FROM \(table.name) ORDER BY \(order) LIMIT 8") {
                    for row in rows { if let id = row.first { table.enqueue(id) } }
                    table.probeDone = true
                }
            }
            if changed, table.work.count - table.workOffset < 512 {
                if let rows = query("SELECT rowid,id,time_created FROM \(table.name) WHERE rowid>? ORDER BY rowid LIMIT \(Self.batch)", integers: [table.highWater]) {
                    for row in rows {
                        guard row.count == 3, let rowID = Int64(row[0]), let created = Int64(row[2]) else { continue }
                        table.highWater = max(table.highWater, rowID)
                        if created >= cutoff { table.enqueue(row[1]) }
                    }
                    moreInsertions = moreInsertions || rows.count == Self.batch
                } else { moreInsertions = true }
            } else if changed { moreInsertions = true }
            if !table.historyDone, table.work.count - table.workOffset < 512 {
                for id in history(table: table) { table.enqueue(id) }
            }
            while table.workOffset < table.work.count && !exhausted {
                let id = table.work[table.workOffset]
                let value = telemetry(table: table, id: id)
                guard !exhausted else { break }
                table.workOffset += 1
                table.queued.remove(id)
                if let value { result.append((table.name, value)) }
            }
            if table.workOffset > 0 {
                table.work.removeFirst(table.workOffset)
                table.workOffset = 0
            }
        }
        dataVersion = moreInsertions ? -1 : version
        return result
    }

    private func discover() -> Bool {
        guard let names = query("SELECT name FROM sqlite_master WHERE type='table' AND name IN ('session_message','message','part','session')") else { return false }
        let available = Set(names.compactMap(\.first))
        var discovered: [Table] = []
        for (name, current) in [("session_message", true), ("message", false)] where available.contains(name) {
            guard let columns = query("PRAGMA table_info(\(name))") else { continue }
            let columnNames = Set(columns.compactMap { $0.count > 1 ? $0[1] : nil })
            let required: Set<String> = ["id", "session_id", "time_created", "time_updated", "data"]
            guard required.isSubset(of: columnNames), !current || columnNames.contains("type") else { continue }
            let table = Table(name: name, current: current, cutoff: cutoff)
            table.highWater = scalar("SELECT rowid FROM \(name) ORDER BY rowid DESC LIMIT 1") ?? 0
            table.fallbackRow = table.highWater + 1
            let indices = query("PRAGMA index_list(\(name))") ?? []
            for index in indices where index.count > 1 {
                let escaped = index[1].replacingOccurrences(of: "'", with: "''")
                let fields = (query("PRAGMA index_info('\(escaped)')") ?? []).compactMap { $0.count > 2 ? $0[2] : nil }
                if fields.first == "time_created" { table.indexedHistory = true }
                if fields.prefix(2).elementsEqual(["session_id", "time_created"]) { table.sessionHistory = available.contains("session") }
            }
            discovered.append(table)
        }
        tables = discovered
        return true
    }

    private func history(table: Table) -> [String] {
        if table.indexedHistory {
            guard let rows = query(
                "SELECT id,time_created FROM \(table.name) WHERE time_created>=? AND (time_created>? OR (time_created=? AND id>?)) ORDER BY time_created,id LIMIT \(Self.batch)",
                integers: [cutoff, table.historyTime, table.historyTime], strings: [table.historyID]
            ) else { return [] }
            for row in rows where row.count == 2 {
                table.historyID = row[0]
                table.historyTime = Int64(row[1]) ?? table.historyTime
            }
            table.historyDone = rows.count < Self.batch
            return rows.compactMap(\.first)
        }
        if table.sessionHistory {
            if table.sessions.isEmpty && !table.sessionDone {
                guard let rows = query("SELECT rowid,id,time_updated FROM session WHERE rowid>? ORDER BY rowid LIMIT 32", integers: [table.sessionRow]) else { return [] }
                for row in rows where row.count == 3 {
                    table.sessionRow = Int64(row[0]) ?? table.sessionRow
                    if (Int64(row[2]) ?? 0) >= cutoff { table.sessions.append(row[1]) }
                }
                table.sessionDone = rows.count < 32
            }
            guard let session = table.sessions.first else {
                table.historyDone = table.sessionDone
                return []
            }
            guard let rows = query(
                "SELECT id,time_created FROM \(table.name) WHERE session_id=? AND time_created>=? AND (time_created>? OR (time_created=? AND id>?)) ORDER BY time_created,id LIMIT \(Self.batch)",
                integers: [cutoff, table.historyTime, table.historyTime], strings: [session, table.historyID], firstString: true
            ) else { return [] }
            for row in rows where row.count == 2 {
                table.historyID = row[0]
                table.historyTime = Int64(row[1]) ?? table.historyTime
            }
            if rows.count < Self.batch {
                table.sessions.removeFirst()
                table.historyTime = cutoff
                table.historyID = ""
            }
            return rows.compactMap(\.first)
        }
        // Old/custom schemas without time indices: walk bounded B-tree headers only.
        // No JSON is read for historical rows outside the window, and no database is copied.
        guard let rows = query("SELECT rowid,id,time_created FROM \(table.name) WHERE rowid<? ORDER BY rowid DESC LIMIT \(Self.batch)", integers: [table.fallbackRow]) else { return [] }
        table.historyDone = rows.count < Self.batch
        return rows.compactMap { row in
            guard row.count == 3 else { return nil }
            table.fallbackRow = Int64(row[0]) ?? table.fallbackRow
            return (Int64(row[2]) ?? 0) >= cutoff ? row[1] : nil
        }
    }

    private func telemetry(table: Table, id: String) -> OpenCodeTelemetry? {
        guard remainingSteps > 0 else { exhausted = true; return nil }
        guard let sizes = query("SELECT length(CAST(data AS BLOB)) FROM \(table.name) WHERE id=?", strings: [id]),
              let text = sizes.first?.first, let bytes = Int(text), bytes <= Self.maxJSONBytes
        else { return nil }
        let reservation = bytes + (table.current ? 0 : Self.maxJSONBytes)
        guard reservation <= remainingJSONBytes else { exhausted = true; return nil }
        remainingJSONBytes -= bytes
        let role = table.current ? "type='assistant'" : "json_extract(data,'$.role')='assistant'"
        let projection = """
        json_object('time',json_extract(data,'$.time'),'model',json_extract(data,'$.model'),
          'modelID',json_extract(data,'$.modelID'),'providerID',json_extract(data,'$.providerID'),
          'tokens',json_extract(data,'$.tokens'),'finish',json_extract(data,'$.finish'),
          'error',CASE WHEN json_type(data,'$.error') NOT IN ('null') THEN 1 ELSE NULL END,
          'summary',json_extract(data,'$.summary'))
        """
        guard let rows = query(
            "SELECT session_id,time_updated,\(projection) FROM \(table.name) WHERE id=? AND json_valid(data)=1 AND \(role)",
            strings: [id]
        ), let row = rows.first, row.count == 3,
              let metadata = Self.object(row[2])
        else { return nil }
        var parts: [[String: Any]] = []
        var partsComplete = true
        if table.current {
            let sql = """
            SELECT json_object('type',json_extract(value,'$.type'),'time',json_extract(value,'$.time'),
              'state',json_object('status',json_extract(value,'$.state.status')))
            FROM \(table.name),json_each(data,'$.content') WHERE \(table.name).id=? LIMIT 129
            """
            if let content = query(sql, strings: [id]) {
                partsComplete = content.count <= 128
                parts = content.prefix(128).compactMap { $0.first.flatMap(Self.object) }
            } else { partsComplete = false }
        } else {
            if let headers = query("SELECT id,length(CAST(data AS BLOB)) FROM part WHERE message_id=? ORDER BY id LIMIT 129", strings: [id]) {
                var partBytes = 0
                if headers.count > 128 { partsComplete = false }
                for header in headers.prefix(128) where header.count == 2 {
                    guard let bytes = Int(header[1]), bytes <= Self.maxJSONBytes,
                          partBytes + bytes <= Self.maxJSONBytes else { partsComplete = false; continue }
                    partBytes += bytes
                    guard bytes <= remainingJSONBytes else { exhausted = true; return nil }
                    remainingJSONBytes -= bytes
                    let sql = """
                    SELECT json_object('type',json_extract(data,'$.type'),'time',json_extract(data,'$.time'),
                      'synthetic',json_extract(data,'$.synthetic'),'ignored',json_extract(data,'$.ignored'),
                      'state',json_object('status',json_extract(data,'$.state.status')),'persistedAt',time_created)
                    FROM part WHERE id=? AND json_valid(data)=1
                    """
                    if let row = query(sql, strings: [header[0]])?.first,
                       let metadata = row.first.flatMap(Self.object) { parts.append(metadata) }
                    else { partsComplete = false }
                }
            } else { partsComplete = false }
        }
        guard let telemetry = OpenCodeTelemetry(
            id: id, session: row[0], current: table.current, metadata: metadata, parts: parts,
            updated: Double(row[1]).flatMap(LogDates.milliseconds), partsComplete: partsComplete
        ) else { return nil }
        hasTelemetry = true
        return telemetry
    }

    private static func object(_ text: String) -> [String: Any]? {
        try? JSONSerialization.jsonObject(with: Data(text.utf8)) as? [String: Any]
    }

    private func scalar(_ sql: String) -> Int64? {
        query(sql)?.first?.first.flatMap(Int64.init)
    }

    private func query(_ sql: String, integers: [Int64] = [], strings: [String] = [], firstString: Bool = false) -> [[String]]? {
        guard remainingSteps > 0 else { return nil }
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(connection, sql, -1, &statement, nil) == SQLITE_OK, let statement else { return nil }
        defer { sqlite3_finalize(statement) }
        var index: Int32 = 1
        func bind(_ value: String) {
            value.withCString { pointer in _ = sqlite3_bind_text(statement, index, pointer, -1, Self.transient) }
            index += 1
        }
        if firstString, let value = strings.first { bind(value) }
        for value in integers { sqlite3_bind_int64(statement, index, value); index += 1 }
        for value in strings.dropFirst(firstString ? 1 : 0) { bind(value) }
        var rows: [[String]] = []
        while true {
            let step = sqlite3_step(statement)
            if step == SQLITE_DONE { return rows }
            if step == SQLITE_INTERRUPT { exhausted = true; return nil }
            guard step == SQLITE_ROW, rows.count < 512 else { return nil }
            var row: [String] = []
            for column in 0..<sqlite3_column_count(statement) {
                guard sqlite3_column_bytes(statement, column) <= 32 * 1024 else { return nil }
                row.append(sqlite3_column_text(statement, column).map { String(cString: $0) } ?? "")
            }
            rows.append(row)
        }
    }
}
