import Foundation

/// Follows the session logs harnesses already write, with no setup: finds the
/// files, reads what is appended, and hands each line to that harness's parser.
/// Not thread-safe; call it from one queue.
public final class SessionLogWatcher {
    public struct Source {
        public let harness: String
        public let root: URL
        let makeParser: () -> SessionLogParser

        public init(harness: String, root: URL, makeParser: @escaping () -> SessionLogParser) {
            self.harness = harness
            self.root = root
            self.makeParser = makeParser
        }

        public var exists: Bool {
            FileManager.default.fileExists(atPath: root.path)
        }
    }

    private final class Tail {
        let url: URL
        let parser: SessionLogParser
        var offset: UInt64 = 0
        var carry = Data()
        /// Start of a line too large to hold; set while the rest of it is being skipped.
        var oversizedHead: Data?
        var lastGrowth: Date
        var flushed = true

        init(url: URL, parser: SessionLogParser, now: Date) {
            self.url = url
            self.parser = parser
            lastGrowth = now
        }
    }

    public let sources: [Source]
    /// Model most recently seen per harness, for labelling calls still in flight.
    public private(set) var latestModel: [String: String] = [:]
    /// When each harness last wrote to a log.
    public private(set) var lastActivity: [String: Date] = [:]

    private var tails: [String: Tail] = [:]
    private var lastScan = Date.distantPast
    private var firstScanDone = false
    private let backfillWindow: TimeInterval
    private let openCode: OpenCodeLogWatcher
    private let deepSeek: DeepSeekLogWatcher
    private let antigravity: AntigravityLogWatcher

    public var detectedHarnesses: [String] {
        var names = sources.filter(\.exists).map(\.harness)
        if openCode.hasLogs { names.append("opencode") }
        if deepSeek.hasLogs { names.append("DeepSeek CLI") }
        if antigravity.hasLogs { names.append("Antigravity") }
        return Array(Set(names)).sorted()
    }

    public func hasLogs(_ harness: String) -> Bool {
        if harness == "opencode" { return openCode.hasLogs }
        if harness == "DeepSeek CLI" { return deepSeek.hasLogs }
        if harness == "Antigravity" { return antigravity.hasLogs }
        return sources.contains { $0.harness == harness && $0.exists }
    }
    private static let scanInterval: TimeInterval = 4
    private static let chunkSize = 256 * 1024
    /// A response never produces a line this long; longer ones are pasted files and tool output.
    private static let maxLineBytes = 1024 * 1024
    private static let edgeBytes = 8 * 1024

    public static func defaultSources(home: URL = FileManager.default.homeDirectoryForCurrentUser) -> [Source] {
        [
            Source(harness: "Claude Code", root: home.appendingPathComponent(".claude/projects")) { ClaudeCodeLogParser() },
            Source(harness: "Codex", root: home.appendingPathComponent(".codex/sessions")) { CodexLogParser() },
            Source(harness: "OMP", root: home.appendingPathComponent(".omp/agent/sessions")) { OMPLogParser() },
            Source(harness: "Pi", root: home.appendingPathComponent(".pi/agent/sessions")) { OMPLogParser(harness: "Pi") },
            Source(harness: "Gemini CLI", root: home.appendingPathComponent(".gemini/tmp")) { GeminiCLILogParser() },
        ]
    }

    /// Sessions touched within `backfillWindow` are read from the start, so the
    /// app has numbers to show the moment it launches.
    public init(
        sources: [Source] = SessionLogWatcher.defaultSources(),
        backfillWindow: TimeInterval = 7 * 24 * 3600,
        openCode: OpenCodeLogWatcher? = nil, deepSeek: DeepSeekLogWatcher? = nil,
        antigravity: AntigravityLogWatcher? = nil
    ) {
        self.sources = sources
        self.backfillWindow = backfillWindow
        self.openCode = openCode ?? OpenCodeLogWatcher(backfillWindow: backfillWindow)
        self.deepSeek = deepSeek ?? DeepSeekLogWatcher(backfillWindow: backfillWindow)
        self.antigravity = antigravity ?? AntigravityLogWatcher(backfillWindow: backfillWindow)
    }

    /// A harness has sent a request, by its own log, and the response is not complete.
    public func isAwaiting(_ harness: String, now: Date = Date()) -> Bool {
        if harness == "opencode" { return openCode.isAwaiting(now: now) }
        if harness == "DeepSeek CLI" { return deepSeek.isAwaiting(now: now) }
        if harness == "Antigravity" { return antigravity.isAwaiting(now: now) }
        return tails.values.contains {
            $0.parser.harness == harness && $0.parser.awaitingResponse && now.timeIntervalSince($0.lastGrowth) < 600
        }
    }

    /// When a harness last sent a request, by its own log.
    public func lastRequest(_ harness: String) -> Date? {
        if harness == "opencode" { return openCode.lastRequestAt }
        if harness == "DeepSeek CLI" { return deepSeek.lastRequestAt }
        if harness == "Antigravity" { return antigravity.lastRequestAt }
        return tails.values.filter { $0.parser.harness == harness }.compactMap(\.parser.lastRequestAt).max()
    }

    /// Reads whatever was appended since the last call.
    public func poll(now: Date = Date()) -> [LogRecord] {
        if now.timeIntervalSince(lastScan) >= Self.scanInterval {
            scan(now: now)
            lastScan = now
        }
        var records: [LogRecord] = []
        for tail in tails.values {
            records += read(tail, now: now)
        }
        records += openCode.poll(now: now)
        records += deepSeek.poll(now: now)
        records += antigravity.poll(now: now)
        latestModel["opencode"] = openCode.latestModel
        latestModel["DeepSeek CLI"] = deepSeek.latestModel
        latestModel["Antigravity"] = antigravity.latestModel
        lastActivity["opencode"] = openCode.lastActivity
        lastActivity["DeepSeek CLI"] = deepSeek.lastActivity
        lastActivity["Antigravity"] = antigravity.lastActivity
        return records
    }

    private func scan(now: Date) {
        let cutoff = firstScanDone ? lastScan.addingTimeInterval(-1) : now.addingTimeInterval(-backfillWindow)
        let keys: [URLResourceKey] = [.contentModificationDateKey, .isRegularFileKey]
        for source in sources {
            guard let enumerator = FileManager.default.enumerator(
                at: source.root, includingPropertiesForKeys: keys, options: [.skipsHiddenFiles]
            ) else { continue }
            for case let url as URL in enumerator where url.pathExtension == "jsonl" {
                guard tails[url.path] == nil,
                      let values = try? url.resourceValues(forKeys: Set(keys)),
                      values.isRegularFile == true,
                      let modified = values.contentModificationDate, modified >= cutoff
                else { continue }
                tails[url.path] = Tail(url: url, parser: source.makeParser(), now: now)
            }
        }
        firstScanDone = true
        // Forget files that have been quiet for a long time.
        tails = tails.filter { now.timeIntervalSince($0.value.lastGrowth) < 6 * 3600 }
    }

    private func read(_ tail: Tail, now: Date) -> [LogRecord] {
        var records: [LogRecord] = []
        let size = ((try? FileManager.default.attributesOfItem(atPath: tail.url.path))?[.size] as? NSNumber)?.uint64Value ?? 0
        if size < tail.offset {
            // Truncated or replaced: start over.
            tail.offset = 0
            tail.carry.removeAll()
            tail.oversizedHead = nil
        }
        if size > tail.offset, let handle = try? FileHandle(forReadingFrom: tail.url) {
            defer { try? handle.close() }
            try? handle.seek(toOffset: tail.offset)
            var reachedEnd = false
            while !reachedEnd {
                // Each chunk is released before the next is read; a week of sessions is hundreds of megabytes.
                autoreleasepool {
                    guard let chunk = try? handle.read(upToCount: Self.chunkSize), !chunk.isEmpty else {
                        reachedEnd = true
                        return
                    }
                    tail.offset += UInt64(chunk.count)
                    tail.carry.append(chunk)
                    records += drain(tail)
                    if tail.carry.count > Self.maxLineBytes {
                        // Keep only the two ends of an enormous line: its type and time are there.
                        if tail.oversizedHead == nil { tail.oversizedHead = Data(tail.carry.prefix(Self.edgeBytes)) }
                        tail.carry = Data(tail.carry.suffix(Self.edgeBytes))
                    }
                }
            }
            tail.lastGrowth = now
            tail.flushed = false
            lastActivity[tail.parser.harness] = now
            if let model = tail.parser.currentModel { latestModel[tail.parser.harness] = model }
        } else if !tail.flushed {
            let idle = now.timeIntervalSince(tail.lastGrowth)
            let flushed = tail.parser.flush(idleFor: idle)
            records += flushed
            if idle >= 3 { tail.flushed = true }
        }
        return records
    }

    private func drain(_ tail: Tail) -> [LogRecord] {
        var records: [LogRecord] = []
        var start = tail.carry.startIndex
        while let newline = tail.carry[start...].firstIndex(of: 0x0A) {
            let line = tail.carry[start..<newline]
            start = tail.carry.index(after: newline)
            if let head = tail.oversizedHead {
                tail.oversizedHead = nil
                if let entry = Self.outline(head: head, tail: Data(line.suffix(Self.edgeBytes))) {
                    records += tail.parser.ingest(entry)
                }
                continue
            }
            guard line.count > 2 else { continue }
            // Parsed lines are released one at a time, or a long session would pile up in memory.
            autoreleasepool {
                if let entry = try? JSONSerialization.jsonObject(with: Data(line)) as? [String: Any] {
                    records += tail.parser.ingest(entry)
                }
            }
        }
        tail.carry = Data(tail.carry[start...])
        return records
    }

    /// Stands in for a line too large to parse. Such a line is input to the model,
    /// never its reply, so the parsers only need what kind of entry it was and when.
    static func outline(head: Data, tail: Data) -> [String: Any]? {
        let start = String(decoding: head, as: UTF8.self)
        let end = String(decoding: tail, as: UTF8.self)
        func matches(_ pattern: String, in text: String) -> [String] {
            guard let regex = try? NSRegularExpression(pattern: pattern) else { return [] }
            let range = NSRange(text.startIndex..., in: text)
            return regex.matches(in: text, range: range).compactMap { match in
                Range(match.range(at: 1), in: text).map { String(text[$0]) }
            }
        }
        let types = matches(#""type":"([A-Za-z_]+)""#, in: start)
        guard let type = types.first, type != "assistant" else { return nil }
        let stampPattern = #""timestamp":"([0-9T:.\-]+Z?)""#
        guard let stamp = matches(stampPattern, in: end).last ?? matches(stampPattern, in: start).first else { return nil }

        var entry: [String: Any] = [
            "type": type,
            "timestamp": stamp,
            "isSidechain": start.contains(#""isSidechain":true"#),
        ]
        if types.count > 1 { entry["payload"] = ["type": types[1]] }
        if let role = matches(#""role":"([A-Za-z]+)""#, in: start).first, role != "assistant" {
            entry["message"] = ["role": role]
        }
        return entry
    }
}
