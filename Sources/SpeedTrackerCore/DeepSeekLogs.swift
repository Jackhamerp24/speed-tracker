import Foundation
import CoreFoundation
import libzstd

/// Telemetry-only reader for the released official deepseek-ai/deepseek-harness formats.
/// Message/argument strings are examined transiently for empty deltas, never retained or returned.
/// V0/V1 packed rows: session-format-v0-to-v1/src/codec.ts; V2–V4 embedded streams:
/// llm/llm/src/assistant-stream.ts and core/session/src/types.ts in the upstream repository.
public final class DeepSeekLogParser: SessionLogParser {
    public let harness = "DeepSeek CLI"
    public private(set) var currentModel: String?
    public private(set) var lastRequestAt: Date?
    public private(set) var lastActivity: Date?
    public private(set) var version: Int?
    public private(set) var hasSupportedHeader = false
    public private(set) var hasTelemetry = false
    public private(set) var limitation: String?
    public private(set) var inheritedCut: Int?
    public var awaitingResponse: Bool { pending != nil }

    private struct Step: Equatable {
        let turn: Int
        let step: Int
        init?(_ data: [String: Any]) {
            guard let turn = deepSeekCount(data["turn"]), let step = deepSeekCount(data["step"]) else { return nil }
            self.turn = turn
            self.step = step
        }
    }
    private struct Timing {
        var first: Date?
        var visible: Date?
        var blockFirst: Date?
        var blockVisible: Date?
        var finish: Date?
        var usage: [String: Any]?
        var interrupted = false
        var sawDeltas = false
        var sawTextDeltas = false

        mutating func chunk(_ chunk: [String: Any], at date: Date) {
            switch chunk["type"] as? String {
            case "text-delta":
                sawDeltas = true
                sawTextDeltas = true
                if let text = chunk["text"] as? String {
                    if !text.isEmpty { first = earlier(first, date) }
                    if text.unicodeScalars.contains(where: { !CharacterSet.whitespacesAndNewlines.contains($0) }) {
                        visible = earlier(visible, date)
                    }
                }
            case "reasoning-delta":
                sawDeltas = true
                if let text = chunk["text"] as? String, !text.isEmpty { first = earlier(first, date) }
            case "tool-call-delta":
                sawDeltas = true
                if (chunk["argumentsDelta"] as? String)?.isEmpty == false || chunk["name"] is String {
                    first = earlier(first, date)
                }
            case "block-start":
                if let type = chunk["blockType"] as? String, ["reasoning", "text", "tool-call"].contains(type) {
                    blockFirst = earlier(blockFirst, date)
                    if type == "text" { blockVisible = earlier(blockVisible, date) }
                }
            case "usage": usage = chunk["usage"] as? [String: Any]
            case "finish":
                finish = date
                let reason = (chunk["reason"] as? [String: Any])?["kind"] as? String
                interrupted = reason == "aborted" || reason == "error"
            default: break
            }
        }

        mutating func streamRecord(_ record: [String: Any], skipping count: Int = 0) {
            guard let type = record["type"] as? String else { return }
            if type == "chunk", let date = LogDates.parse(record["time"]), let value = record["chunk"] as? [String: Any] {
                chunk(value, at: date)
                return
            }
            guard ["text-chunks", "reasoning-chunks", "tool-call-chunks"].contains(type),
                  let initial = deepSeekMilliseconds(record["time0"]),
                  let members = record[type == "tool-call-chunks" ? "args" : "texts"] as? [String],
                  !members.isEmpty, let gaps = record["dt"] as? [NSNumber], gaps.count == members.count - 1
            else { return }
            sawDeltas = true
            if type == "text-chunks" { sawTextDeltas = true }
            var stamp = initial
            for index in members.indices {
                if index > 0 {
                    let gap = gaps[index - 1].doubleValue
                    guard gap.isFinite, gap.rounded() == gap, abs(gap) <= 9_007_199_254_740_991 else { return }
                    stamp += gap
                }
                guard index >= count, let date = LogDates.milliseconds(stamp) else { continue }
                let member = members[index]
                if !member.isEmpty || (type == "tool-call-chunks" && record["name"] is String) {
                    first = earlier(first, date)
                }
                if type == "text-chunks", member.unicodeScalars.contains(where: { !CharacterSet.whitespacesAndNewlines.contains($0) }) {
                    visible = earlier(visible, date)
                }
            }
        }
    }
    private var sessionID = ""
    private var seeded = false
    private let knownInheritedCut: Int?
    private let expectedVersion: Int?
    private var lastSequence = -1
    private var pending: (step: Step, date: Date)?
    private var historicalTiming = Timing()
    private var historicalStep: Step?
    private var provider = ""

    /// Seeded V2–V4 files need the *last* tagged inherited boundary discovered before replay.
    /// The watcher performs that bounded metadata pass automatically.
    public init(inheritedEventCount: Int? = nil, expectedVersion: Int? = nil) {
        knownInheritedCut = inheritedEventCount
        self.expectedVersion = expectedVersion
    }

    var needsInheritedScan: Bool { hasSupportedHeader && seeded && (version ?? 0) >= 2 && knownInheritedCut == nil }

    public func ingest(_ entry: [String: Any]) -> [LogRecord] {
        guard let type = entry["type"] as? String else { return [] }
        if type == "session" {
            guard version == nil else { return [] }
            guard let number = deepSeekCount(entry["version"]), let id = entry["id"] as? String, !id.isEmpty,
                  LogDates.parse(entry["createdAt"]) != nil else {
                limitation = "Invalid DeepSeek session header"
                return []
            }
            version = number
            guard (0...4).contains(number), expectedVersion == nil || expectedVersion == number else {
                limitation = "Unsupported DeepSeek session generation v\(number)"
                return []
            }
            if number >= 2 {
                guard let isSeeded = entry["isSeeded"] as? Bool else {
                    limitation = "DeepSeek session header lacks isSeeded"
                    return []
                }
                seeded = isSeeded
                inheritedCut = knownInheritedCut
            } else {
                seeded = entry["seedLength"] != nil
                if seeded, deepSeekCount(entry["seedLength"]) == nil {
                    limitation = "Invalid DeepSeek inherited seed length"
                    return []
                }
                inheritedCut = deepSeekCount(entry["seedLength"]) ?? 0
            }
            sessionID = id
            hasSupportedHeader = true
            return []
        }
        guard hasSupportedHeader, let data = entry["data"] as? [String: Any] else { return [] }
        let packed = ["text-chunks", "reasoning-chunks", "tool-call-chunks"].contains(type)
        guard let sequence = deepSeekCount(entry[packed ? "seq0" : "seq"]), sequence > lastSequence else { return [] }
        let time = LogDates.parse(entry[packed ? "time0" : "time"])
        guard let time else { return [] }
        var packedCount = 1
        if packed {
            guard (version ?? 2) < 2, let members = data[type == "tool-call-chunks" ? "args" : "texts"] as? [String], !members.isEmpty,
                  sequence <= Int.max - members.count else { return [] }
            packedCount = members.count
        }
        lastSequence = sequence + packedCount - 1
        if type == "session/end-seed", data["inherited"] as? Bool == true {
            inheritedCut = max(inheritedCut ?? 0, sequence)
        }
        // Do not mistake ancestor end-seed markers for this child's final inherited cut.
        if needsInheritedScan { return [] }
        if let cut = inheritedCut, sequence + packedCount <= cut { return [] }
        lastActivity = max(lastActivity ?? time, time)

        switch type {
        case "step/start":
            guard let step = Step(data) else { return [] }
            pending = (step, time)
            lastRequestAt = time
            historicalTiming = Timing()
            historicalStep = step
            hasTelemetry = true
        case "request/header":
            if let header = data["header"] as? [String: Any], let config = header["config"] as? [String: Any] {
                if let model = config["model"] as? String, !model.isEmpty { currentModel = model }
                if let route = config["provider"] as? String { provider = route }
            }
        case "request/context":
            if let model = data["model"] as? String, !model.isEmpty { currentModel = model }
            if let route = data["provider"] as? String { provider = route }
        case "assistant/chunk":
            guard (version ?? 2) < 2, let step = Step(data), let chunk = data["chunk"] as? [String: Any] else { return [] }
            if historicalStep != step { historicalTiming = Timing(); historicalStep = step }
            historicalTiming.chunk(chunk, at: time)
        case "text-chunks", "reasoning-chunks", "tool-call-chunks":
            guard let step = Step(data) else { return [] }
            if historicalStep != step { historicalTiming = Timing(); historicalStep = step }
            var record = data
            record["type"] = type
            record["time0"] = entry["time0"]
            historicalTiming.streamRecord(record, skipping: max(0, (inheritedCut ?? 0) - sequence))
        case "assistant/attempt", "llm/retry":
            // A failed/retried attempt is not a completed model response. A subsequent
            // attempt in this step has no durable dispatch timestamp; never reuse the old one.
            if let step = Step(data), pending?.step == step { pending = nil }
            historicalTiming = Timing()
            historicalStep = nil
            hasTelemetry = true
        case "assistant/message":
            guard let step = Step(data) else { return [] }
            let request = pending?.step == step ? pending?.date : nil
            if pending?.step == step { pending = nil }
            var timing = (version ?? 2) < 2 && historicalStep == step ? historicalTiming : Timing()
            historicalTiming = Timing()
            historicalStep = nil
            if let stream = data["stream"] as? [[String: Any]] { for record in stream { timing.streamRecord(record) } }
            let message: [String: Any]
            if let wrapped = data["message"] as? [String: Any] { message = wrapped }
            else if version == 0, let provenance = data["provenance"] as? [String: Any] {
                var source = provenance
                source["kind"] = "model"
                message = ["id": "legacy-message:\(sessionID):\(sequence)", "role": "assistant", "source": source]
            } else { return [] }
            guard message["role"] as? String == "assistant", let id = message["id"] as? String, !id.isEmpty,
                  let source = message["source"] as? [String: Any], source["kind"] as? String == "model",
                  let model = source["model"] as? String, !model.isEmpty,
                  let route = source["provider"] as? String,
                  // Rewrites/recall are surface maintenance, not another provider call.
                  entry["surfaceOp"] == nil || entry["surfaceOp"] as? String == "append"
            else { return [] }
            currentModel = model
            provider = route
            hasTelemetry = true
            guard let usage = (data["usage"] as? [String: Any]) ?? timing.usage,
                  let output = deepSeekCount(usage["outputTokens"]) else { return [] }
            let end = timing.finish.flatMap { $0 <= time ? $0 : nil } ?? time
            let first = validTiming(timing.sawDeltas ? timing.first : timing.blockFirst, start: request, end: end)
            let visible = validTiming(timing.sawTextDeltas ? timing.visible : timing.blockVisible, start: request, end: end)
            let cached = deepSeekCount(usage["cacheReadTokens"])
            let uncached = deepSeekCount(usage["inputTokens"])
            let written = deepSeekCount(usage["cacheWriteTokens"]) ?? 0
            var aggregateInput: Int?
            if let uncached, uncached <= Int.max - (cached ?? 0), uncached + (cached ?? 0) <= Int.max - written {
                aggregateInput = uncached + (cached ?? 0) + written
            }
            return [LogRecord(
                key: "deepseek:\(id)", harness: harness, model: model, provider: provider,
                requestStart: request.flatMap { $0 <= end ? $0 : nil }, firstToken: first, firstVisible: visible,
                end: end, outputTokens: output, reasoningTokens: deepSeekCount(usage["reasoningTokens"]),
                inputTokens: aggregateInput, cachedInputTokens: cached,
                aborted: data["interrupted"] as? Bool == true || timing.interrupted
            )]
        case "step/end":
            if let step = Step(data), pending?.step == step { pending = nil }
            historicalTiming = Timing()
            historicalStep = nil
        case "turn/end", "session/end-seed":
            pending = nil
            historicalTiming = Timing()
            historicalStep = nil
        default: break
        }
        return []
    }

    public func flush(idleFor seconds: TimeInterval) -> [LogRecord] { [] }

    private func validTiming(_ date: Date?, start: Date?, end: Date) -> Date? {
        guard let date, date <= end else { return nil }
        if let start, date < start { return nil }
        return date
    }
}

private func earlier(_ previous: Date?, _ date: Date) -> Date { previous.map { min($0, date) } ?? date }
private func deepSeekCount(_ value: Any?) -> Int? {
    guard let number = value as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID() else { return nil }
    let raw = number.doubleValue
    guard raw.isFinite, raw >= 0, raw.rounded() == raw, raw <= 9_007_199_254_740_991, raw < Double(Int.max) else { return nil }
    return Int(raw)
}
private func deepSeekMilliseconds(_ value: Any?) -> Double? {
    guard let number = value as? NSNumber, CFGetTypeID(number) != CFBooleanGetTypeID() else { return nil }
    let value = number.doubleValue
    return value.isFinite && value > 0 && value.rounded() == value && value <= 9_007_199_254_740_991 ? value : nil
}

/// One bounded line buffer shared by raw and streaming Zstandard input.
private final class DeepSeekLines {
    static let maximum = 4 * 1024 * 1024
    private var carry = Data()
    private var discarding = false
    var oversized = false

    func consume(_ bytes: UnsafeRawBufferPointer, row: ([String: Any]) -> Void) {
        var start = 0
        for index in bytes.indices where bytes[index] == 10 {
            append(UnsafeRawBufferPointer(rebasing: bytes[start..<index]))
            if !discarding, !carry.isEmpty,
               let entry = try? JSONSerialization.jsonObject(with: carry) as? [String: Any] { row(entry) }
            carry.removeAll(keepingCapacity: true)
            discarding = false
            start = index + 1
        }
        append(UnsafeRawBufferPointer(rebasing: bytes[start...]))
    }

    private func append(_ bytes: UnsafeRawBufferPointer) {
        guard !discarding else { return }
        guard bytes.count <= Self.maximum - carry.count else {
            carry.removeAll(keepingCapacity: false)
            discarding = true
            oversized = true
            return
        }
        carry.append(contentsOf: bytes)
    }
}

private enum DeepSeekDecodeError: Error { case unavailable, corrupt }

/// Keeps the decoder alive across arbitrary input splits, including a torn final frame.
private final class DeepSeekZstd {
    private let stream: OpaquePointer
    private var input = Data()
    private var position = 0
    private var draining = false
    private var output = [UInt8](repeating: 0, count: 64 * 1024)
    private(set) var frameComplete = true
    var hasPending: Bool { position < input.count || draining }

    init() throws {
        guard let stream = ZSTD_createDStream() else { throw DeepSeekDecodeError.unavailable }
        guard ZSTD_isError(ZSTD_initDStream(stream)) == 0 else {
            ZSTD_freeDStream(stream)
            throw DeepSeekDecodeError.unavailable
        }
        self.stream = stream
    }
    deinit { ZSTD_freeDStream(stream) }

    func feed(_ bytes: Data) { input = bytes; position = 0 }

    func drain(limit: Int, consume: (UnsafeRawBufferPointer) -> Void) throws -> Int {
        var produced = 0
        while hasPending, produced < limit {
            let capacity = min(output.count, limit - produced)
            var result = 0
            var used = 0
            var consumed = position
            input.withUnsafeBytes { source in
                output.withUnsafeMutableBytes { destination in
                    var incoming = ZSTD_inBuffer(src: source.baseAddress, size: source.count, pos: position)
                    var outgoing = ZSTD_outBuffer(dst: destination.baseAddress, size: capacity, pos: 0)
                    result = ZSTD_decompressStream(stream, &outgoing, &incoming)
                    consumed = incoming.pos
                    used = outgoing.pos
                    if ZSTD_isError(result) == 0, used > 0 {
                        consume(UnsafeRawBufferPointer(rebasing: destination[..<used]))
                    }
                }
            }
            guard ZSTD_isError(result) == 0 else { throw DeepSeekDecodeError.corrupt }
            let advanced = consumed != position
            position = consumed
            produced += used
            frameComplete = result == 0
            draining = used == capacity && !frameComplete
            if !advanced && used == 0 { draining = false; break }
        }
        if position == input.count && !draining { input.removeAll(keepingCapacity: false); position = 0 }
        return produced
    }
}

/// Passive read-only follower of official dsh's ~/.dsh/sessions tree. No external executable,
/// configuration, credential access, decompressed-file copy, or migration writes are needed.
/// Call from one queue, like SessionLogWatcher. Each poll has a shared decoded-byte budget.
public final class DeepSeekLogWatcher {
    public let roots: [URL]
    public private(set) var hasLogs = false
    public private(set) var latestModel: String?
    public private(set) var lastActivity: Date?
    public private(set) var lastRequestAt: Date?
    public private(set) var limitations: [String] = []

    private final class Tail {
        let url: URL
        let version: Int
        let compressed: Bool
        var parser: DeepSeekLogParser
        var lines = DeepSeekLines()
        var decoder: DeepSeekZstd?
        var offset: UInt64 = 0
        var probe = Data()
        var identity: String?
        var failed = false
        var prepared = false
        init(url: URL, version: Int) {
            self.url = url
            self.version = version
            compressed = url.pathExtension == "zstd"
            parser = DeepSeekLogParser(expectedVersion: version)
        }
        func reset(inheritedCut: Int? = nil) {
            parser = DeepSeekLogParser(inheritedEventCount: inheritedCut, expectedVersion: version)
            lines = DeepSeekLines()
            decoder = nil
            offset = 0
            probe.removeAll()
            failed = false
        }
    }
    private let backfillWindow: TimeInterval
    private var tails: [String: Tail] = [:]
    private var lastScan = Date.distantPast
    private var discoveryLimitations: [String] = []
    // Persistent history also deduplicates sourceKey; this bounded recent set avoids repeats
    // during replacement/torn-tail repair without growing with the lifetime of the application.
    private var recentKeys = Set<String>()
    private var keyOrder: [String] = []
    private var keyCursor = 0
    private static let readSize = 64 * 1024
    private static let pollBudget = 16 * 1024 * 1024

    /// Default source root, following the official harness precedence: a nonblank
    /// `DSH_HOME` beats the default `~/.dsh` (packages/util/home-paths). Pass
    /// `roots:` directly for a fully deterministic root.
    public convenience init(
        home: URL = FileManager.default.homeDirectoryForCurrentUser,
        backfillWindow: TimeInterval = 7 * 24 * 3600,
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) {
        let override = environment["DSH_HOME"]?.trimmingCharacters(in: .whitespacesAndNewlines)
        let root: URL
        if let override, !override.isEmpty {
            root = URL(fileURLWithPath: (override as NSString).expandingTildeInPath).appendingPathComponent("sessions")
        } else {
            root = home.appendingPathComponent(".dsh/sessions")
        }
        self.init(roots: [root], backfillWindow: backfillWindow)
    }
    public init(roots: [URL], backfillWindow: TimeInterval = 7 * 24 * 3600) {
        self.roots = roots
        self.backfillWindow = backfillWindow
    }

    public func isAwaiting(now: Date = Date()) -> Bool {
        tails.values.contains {
            guard !$0.failed, $0.parser.awaitingResponse, let date = $0.parser.lastRequestAt else { return false }
            let elapsed = now.timeIntervalSince(date)
            return elapsed >= 0 && elapsed < 600
        }
    }

    public func poll(now: Date = Date()) -> [LogRecord] {
        if now.timeIntervalSince(lastScan) >= 4 {
            scan(now: now)
            lastScan = now
        }
        var records: [LogRecord] = []
        var budget = Self.pollBudget
        for tail in tails.values.sorted(by: { $0.url.path < $1.url.path }) where budget > 0 {
            follow(tail, budget: &budget) { record in
                guard recentKeys.insert(record.key).inserted else { return }
                if keyOrder.count == 8192 {
                    recentKeys.remove(keyOrder[keyCursor])
                    keyOrder[keyCursor] = record.key
                    keyCursor = (keyCursor + 1) % 8192
                } else {
                    keyOrder.append(record.key)
                }
                records.append(record)
            }
        }
        let supported = tails.values.filter { !$0.failed && $0.parser.hasSupportedHeader && !$0.parser.needsInheritedScan }
        hasLogs = supported.contains { $0.parser.hasTelemetry }
        lastActivity = supported.compactMap { $0.parser.lastActivity }.max()
        lastRequestAt = supported.compactMap { $0.parser.lastRequestAt }.max()
        latestModel = supported.filter { $0.parser.currentModel != nil }.max {
            ($0.parser.lastActivity ?? .distantPast) < ($1.parser.lastActivity ?? .distantPast)
        }?.parser.currentModel
        limitations = discoveryLimitations + tails.values.sorted { $0.url.path < $1.url.path }.compactMap {
            if $0.failed { return "Unreadable or corrupt DeepSeek log: \($0.url.path)" }
            if $0.lines.oversized { return "DeepSeek telemetry row exceeded 4 MiB and was skipped: \($0.url.path)" }
            if let limitation = $0.parser.limitation { return "\(limitation): \($0.url.path)" }
            if $0.parser.needsInheritedScan { return "DeepSeek seeded log has no complete inherited boundary: \($0.url.path)" }
            return nil
        }
        return records
    }

    private func scan(now: Date) {
        discoveryLimitations.removeAll()
        let keys: [URLResourceKey] = [.isRegularFileKey, .contentModificationDateKey]
        var selected: [String: (version: Int, urls: [URL])] = [:]
        for root in roots {
            guard let enumerator = FileManager.default.enumerator(at: root, includingPropertiesForKeys: keys, options: [.skipsHiddenFiles]) else { continue }
            for case let url as URL in enumerator {
                guard let version = Self.generation(url.lastPathComponent),
                      let values = try? url.resourceValues(forKeys: Set(keys)), values.isRegularFile == true else { continue }
                let directory = url.deletingLastPathComponent().path
                if let old = selected[directory], old.version > version { continue }
                if selected[directory]?.version == version { selected[directory]?.urls.append(url) }
                else { selected[directory] = (version, [url]) }
            }
        }
        var retained = Set<String>()
        for candidate in selected.values {
            guard candidate.urls.count == 1, let url = candidate.urls.first else {
                discoveryLimitations.append("Ambiguous DeepSeek raw/compressed generation: \(candidate.urls.first?.deletingLastPathComponent().path ?? "")")
                continue
            }
            guard (0...4).contains(candidate.version) else {
                discoveryLimitations.append("Unsupported DeepSeek session generation v\(candidate.version): \(url.path)")
                continue
            }
            guard let modified = try? url.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate,
                  modified >= now.addingTimeInterval(-backfillWindow) else { continue }
            retained.insert(url.path)
            if tails[url.path] == nil { tails[url.path] = Tail(url: url, version: candidate.version) }
        }
        tails = tails.filter { retained.contains($0.key) }
    }

    static func generation(_ name: String) -> Int? {
        let suffix = name.hasSuffix(".jsonl.zstd") ? ".jsonl.zstd" : ".jsonl"
        guard name.hasSuffix(suffix) else { return nil }
        let stem = String(name.dropLast(suffix.count))
        if stem == "session" { return 0 }
        guard stem.hasPrefix("session.v") else { return nil }
        let digits = stem.dropFirst("session.v".count)
        guard !digits.isEmpty, digits.allSatisfy({ $0.isASCII && $0.isNumber }), let version = Int(digits), version > 0,
              String(version) == digits else { return nil }
        return version
    }

    private func follow(_ tail: Tail, budget: inout Int, record: (LogRecord) -> Void) {
        guard let attributes = try? FileManager.default.attributesOfItem(atPath: tail.url.path),
              let size = (attributes[.size] as? NSNumber)?.uint64Value,
              let handle = try? FileHandle(forReadingFrom: tail.url) else { tail.failed = true; return }
        defer { try? handle.close() }
        let identity = "\(attributes[.systemNumber] ?? ""):\(attributes[.systemFileNumber] ?? "")"
        do {
            var changed = tail.identity != nil && tail.identity != identity
            if size >= tail.offset, !tail.probe.isEmpty {
                _ = try handle.seek(toOffset: tail.offset - UInt64(tail.probe.count))
                let currentProbe = try handle.read(upToCount: tail.probe.count)
                changed = changed || currentProbe != tail.probe
            }
            if size < tail.offset || changed {
                tail.reset()
                tail.prepared = false
            }
            tail.identity = identity
            guard !tail.failed else { return }
            while budget > 0 {
                let ingest: (UnsafeRawBufferPointer) -> Void = { bytes in
                    tail.lines.consume(bytes) { entry in
                        for value in tail.parser.ingest(entry) { record(value) }
                    }
                }
                if let decoder = tail.decoder, decoder.hasPending {
                    budget -= try decoder.drain(limit: budget, consume: ingest)
                    if decoder.hasPending { break }
                }
                guard tail.offset < size else {
                    if !tail.prepared, tail.parser.needsInheritedScan, let cut = tail.parser.inheritedCut {
                        tail.reset(inheritedCut: cut)
                        tail.prepared = true
                        continue
                    }
                    if tail.decoder?.frameComplete == true { tail.decoder = nil }
                    break
                }
                _ = try handle.seek(toOffset: tail.offset)
                let readLimit = tail.compressed ? Self.readSize : min(Self.readSize, budget)
                guard let bytes = try handle.read(upToCount: Int(min(size - tail.offset, UInt64(readLimit)))), !bytes.isEmpty else { break }
                tail.offset += UInt64(bytes.count)
                budget -= bytes.count
                if bytes.count >= 64 { tail.probe = Data(bytes.suffix(64)) }
                else {
                    tail.probe.append(bytes)
                    if tail.probe.count > 64 { tail.probe = Data(tail.probe.suffix(64)) }
                }
                if tail.compressed {
                    if tail.decoder == nil { tail.decoder = try DeepSeekZstd() }
                    tail.decoder?.feed(bytes)
                } else {
                    bytes.withUnsafeBytes(ingest)
                }
            }
        } catch { tail.failed = true; tail.decoder = nil }
    }
}
