import Foundation
import Testing
import libzstd
@testable import SpeedTrackerCore

private let dshEpoch = 1_790_000_000_000
private func dshDate(_ offset: Int = 0) -> Date { Date(timeIntervalSince1970: Double(dshEpoch + offset) / 1000) }
private func dshHeader(version: Int = 3, seeded: Bool = false, seedLength: Int? = nil) -> [String: Any] {
    var result: [String: Any] = ["type": "session", "version": version, "id": "session-fixture", "createdAt": dshEpoch, "delegationDepth": 0]
    if version >= 2 { result["isSeeded"] = seeded }
    if let seedLength { result["seedLength"] = seedLength }
    return result
}
private func dshEvent(_ type: String, seq: Int, at: Int, _ data: [String: Any]) -> [String: Any] {
    ["type": type, "seq": seq, "time": dshEpoch + at, "data": data]
}
private func dshStart(seq: Int = 0, at: Int = 0, step: Int = 1) -> [String: Any] {
    dshEvent("step/start", seq: seq, at: at, ["turn": 1, "step": step])
}
private func dshChunk(_ type: String, at: Int, fields: [String: Any] = [:]) -> [String: Any] {
    var chunk = fields
    chunk["type"] = type
    return ["type": "chunk", "time": dshEpoch + at, "chunk": chunk]
}
private func dshMessage(seq: Int = 1, at: Int = 6000, id: String = "message-fixture", step: Int = 1,
                        sourceKind: String = "model", usage: [String: Any]? = ["inputTokens": 14, "outputTokens": 100],
                        stream: [[String: Any]]? = nil, interrupted: Bool = false) -> [String: Any] {
    var data: [String: Any] = [
        "turn": 1, "step": step,
        "message": ["role": "assistant", "id": id, "content": [["type": "text", "text": "synthetic fixture only"]],
                    "source": ["kind": sourceKind, "provider": "opencode-go", "model": "deepseek-v4.1-flash",
                               // Replay state is adapter metadata, not evidence that this is a replayed call.
                               "replayState": ["response": ["kind": "pi-ai", "stopReason": "toolUse"]]]],
        "stream": stream ?? [
            dshChunk("block-start", at: 1999, fields: ["index": 0, "blockType": "reasoning"]),
            ["type": "reasoning-chunks", "time0": dshEpoch + 2000, "index": 0, "dt": [10, 20], "texts": ["", "thinking", " fragment"]],
            dshChunk("block-start", at: 4000, fields: ["index": 1, "blockType": "text"]),
            ["type": "text-chunks", "time0": dshEpoch + 4000, "index": 1, "dt": [25, 25], "texts": [" ", "", "answer"]],
            dshChunk("finish", at: 5998, fields: ["reason": ["kind": "tool-calls"]]),
        ],
    ]
    if let usage { data["usage"] = usage }
    if interrupted { data["interrupted"] = true }
    return dshEvent("assistant/message", seq: seq, at: at, data)
}
private func dshLines(_ rows: [[String: Any]]) throws -> Data {
    var data = Data()
    for row in rows {
        data.append(try JSONSerialization.data(withJSONObject: row, options: [.sortedKeys]))
        data.append(10)
    }
    return data
}
private enum DshFixtureError: Error { case compression }
private func dshFrame(_ data: Data) throws -> Data {
    var result = Data(count: ZSTD_compressBound(data.count))
    let count = result.withUnsafeMutableBytes { target in
        data.withUnsafeBytes { source in
            ZSTD_compress(target.baseAddress, target.count, source.baseAddress, source.count, 3)
        }
    }
    guard ZSTD_isError(count) == 0 else { throw DshFixtureError.compression }
    result.count = count
    return result
}
private final class DshDirectory {
    let root: URL
    let session: URL
    init() throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent("speedtracker-dsh-\(UUID().uuidString)")
        session = root.appendingPathComponent("--project--/session-fixture")
        try FileManager.default.createDirectory(at: session, withIntermediateDirectories: true)
    }
    deinit { try? FileManager.default.removeItem(at: root) }
    func file(_ name: String = "session.v3.jsonl") -> URL { session.appendingPathComponent(name) }
    func put(_ data: Data, name: String = "session.v3.jsonl", modified: Date = dshDate()) throws {
        let url = file(name)
        try data.write(to: url, options: [.atomic])
        try FileManager.default.setAttributes([.modificationDate: modified], ofItemAtPath: url.path)
    }
    func append(_ data: Data, name: String = "session.v3.jsonl") throws {
        let url = file(name)
        let handle = try FileHandle(forWritingTo: url)
        defer { try? handle.close() }
        _ = try handle.seekToEnd()
        try handle.write(contentsOf: data)
        try FileManager.default.setAttributes([.modificationDate: dshDate()], ofItemAtPath: url.path)
    }
}

@Suite struct DeepSeekParserTests {
    @Test func exactStreamTimingStopsBeforeToolsAndAggregatesDisjointUsage() throws {
        let parser = DeepSeekLogParser()
        #expect(parser.ingest(dshHeader()).isEmpty)
        _ = parser.ingest(dshStart())
        #expect(parser.awaitingResponse)
        let record = try #require(parser.ingest(dshMessage(usage: [
            "inputTokens": 14, "cacheReadTokens": 8960, "cacheWriteTokens": 20,
            "outputTokens": 100, "reasoningTokens": 33, "totalTokens": 9094,
        ])).first)
        #expect(!parser.awaitingResponse)
        #expect(record.harness == "DeepSeek CLI")
        #expect(record.provider == "opencode-go")
        #expect(record.model == "deepseek-v4.1-flash")
        #expect(record.key == "deepseek:message-fixture")
        #expect(record.requestStart == dshDate())
        #expect(record.firstToken == dshDate(2010))
        #expect(record.firstVisible == dshDate(4050))
        #expect(record.end == dshDate(5998))
        #expect(record.inputTokens == 8994)
        #expect(record.cachedInputTokens == 8960)
        #expect(record.outputTokens == 100)
        #expect(record.reasoningTokens == 33)
        _ = parser.ingest(dshEvent("tool/call", seq: 2, at: 6001, ["turn": 1, "step": 1]))
        _ = parser.ingest(dshEvent("tool/result", seq: 3, at: 30000, ["turn": 1, "step": 1]))
        #expect(!parser.awaitingResponse)
        #expect(parser.ingest(dshEvent("step/end", seq: 4, at: 30001, ["turn": 1, "step": 1])).isEmpty)
        #expect(parser.flush(idleFor: 3600).isEmpty)
        let generation = try #require(record.makeRecord().generation)
        #expect(abs(generation - 3.988) < 0.0001)
    }

    @Test func whitespaceIsGeneratedButNotVisibleAndToolArgumentsCountAsGeneration() throws {
        let parser = DeepSeekLogParser()
        _ = parser.ingest(dshHeader())
        _ = parser.ingest(dshStart())
        let stream: [[String: Any]] = [
            dshChunk("block-start", at: 100, fields: ["index": 0, "blockType": "text"]),
            ["type": "text-chunks", "time0": dshEpoch + 200, "index": 0, "dt": [100], "texts": ["", " \n"]],
            ["type": "tool-call-chunks", "time0": dshEpoch + 500, "index": 1, "id": "tool-1", "name": "read", "dt": [], "args": [""]],
            dshChunk("finish", at: 600, fields: ["reason": ["kind": "tool-calls"]]),
        ]
        let record = try #require(parser.ingest(dshMessage(at: 601, stream: stream)).first)
        #expect(record.firstToken == dshDate(300))
        #expect(record.firstVisible == nil)
        #expect(record.end == dshDate(600))
    }

    @Test func signedPackedTimeDeltasDoNotInventATime0Token() throws {
        let parser = DeepSeekLogParser()
        _ = parser.ingest(dshHeader(version: 4))
        _ = parser.ingest(dshStart())
        let stream: [[String: Any]] = [
            ["type": "text-chunks", "time0": dshEpoch + 1000, "index": 0, "dt": [100, -10], "texts": ["", " ", "visible"]],
            dshChunk("finish", at: 2000, fields: ["reason": ["kind": "stop"]]),
        ]
        let record = try #require(parser.ingest(dshMessage(at: 2001, stream: stream)).first)
        #expect(record.firstToken == dshDate(1090))
        #expect(record.firstVisible == dshDate(1090))
    }

    @Test func metadataOnlyBlockStartsRemainAvailableWithoutDeltaPayloads() throws {
        let parser = DeepSeekLogParser()
        _ = parser.ingest(dshHeader())
        _ = parser.ingest(dshStart())
        let record = try #require(parser.ingest(dshMessage(stream: [
            dshChunk("block-start", at: 1000, fields: ["index": 0, "blockType": "reasoning"]),
            dshChunk("block-start", at: 2000, fields: ["index": 1, "blockType": "text"]),
        ])).first)
        #expect(record.firstToken == dshDate(1000))
        #expect(record.firstVisible == dshDate(2000))
        #expect(record.end == dshDate(6000))
    }

    @Test func nonmodelMissingUsageAndInvalidTokenCountersNeverBecomeFakeMeasurements() {
        for fixture in [dshMessage(sourceKind: "plugin"), dshMessage(usage: nil),
                        dshMessage(usage: ["outputTokens": -1]), dshMessage(usage: ["outputTokens": 1.5]),
                        dshMessage(usage: ["outputTokens": true])] {
            let parser = DeepSeekLogParser()
            _ = parser.ingest(dshHeader())
            _ = parser.ingest(dshStart())
            #expect(parser.ingest(fixture).isEmpty)
            #expect(!parser.awaitingResponse)
        }
    }

    @Test func zeroOutputIsExactNotEstimatedAndTotalIsNotOutput() throws {
        let parser = DeepSeekLogParser()
        _ = parser.ingest(dshHeader())
        _ = parser.ingest(dshStart())
        let record = try #require(parser.ingest(dshMessage(usage: ["inputTokens": 5, "outputTokens": 0, "totalTokens": 999], stream: [])).first)
        #expect(record.outputTokens == 0)
        #expect(record.firstToken == nil)
        #expect(record.inputTokens == 5)
        #expect(!record.makeRecord().tokensEstimated)
    }

    @Test(arguments: ["aborted", "error"])
    func failedFinishAndInterruptedPrefixesAreMarked(kind: String) throws {
        let parser = DeepSeekLogParser()
        _ = parser.ingest(dshHeader())
        _ = parser.ingest(dshStart())
        let record = try #require(parser.ingest(dshMessage(stream: [
            dshChunk("reasoning-delta", at: 1000, fields: ["index": 0, "text": "prefix"]),
            dshChunk("usage", at: 1999, fields: ["usage": ["inputTokens": 5, "outputTokens": 2]]),
            dshChunk("finish", at: 2000, fields: ["reason": ["kind": kind, "failure": ["code": "FIXTURE", "message": "fixture"]]]),
        ])).first)
        #expect(record.aborted)
        #expect(record.end == dshDate(2000))
        #expect(!parser.awaitingResponse)
        let second = DeepSeekLogParser()
        _ = second.ingest(dshHeader())
        _ = second.ingest(dshStart())
        #expect(second.ingest(dshMessage(stream: [], interrupted: true)).first?.aborted == true)
    }

    @Test func failedAttemptRetryDoesNotReuseFirstDispatchForLatency() throws {
        let parser = DeepSeekLogParser()
        _ = parser.ingest(dshHeader())
        _ = parser.ingest(dshStart())
        #expect(parser.ingest(dshEvent("assistant/attempt", seq: 1, at: 1000, ["turn": 1, "step": 1, "stream": []])).isEmpty)
        #expect(!parser.awaitingResponse)
        let record = try #require(parser.ingest(dshMessage(seq: 2)).first)
        #expect(record.requestStart == nil)
        #expect(record.makeRecord().ttft == nil)
        #expect(parser.ingest(dshMessage(seq: 2)).isEmpty)
    }

    @Test func staleBracketClosersNeverManufactureRecords() {
        for type in ["step/end", "turn/end", "session/end-seed"] {
            let parser = DeepSeekLogParser()
            _ = parser.ingest(dshHeader())
            _ = parser.ingest(dshStart())
            #expect(parser.flush(idleFor: 10000).isEmpty)
            #expect(parser.ingest(dshEvent(type, seq: 1, at: 500, ["turn": 1, "step": 1, "reason": ["kind": "interrupted"]])).isEmpty)
            #expect(!parser.awaitingResponse)
        }
    }

    @Test func unsupportedOrMismatchedHeaderIsClearlyRefused() {
        let parser = DeepSeekLogParser()
        _ = parser.ingest(dshHeader(version: 5))
        #expect(!parser.hasSupportedHeader)
        #expect(parser.limitation?.contains("v5") == true)
        #expect(parser.ingest(dshStart()).isEmpty)
        #expect(!parser.awaitingResponse)
        let mismatch = DeepSeekLogParser(expectedVersion: 3)
        _ = mismatch.ingest(dshHeader(version: 2))
        #expect(!mismatch.hasSupportedHeader)
    }

    @Test(arguments: [0, 1])
    func historicalTopLevelPackedChunksAndUsageArePreserved(version: Int) throws {
        let parser = DeepSeekLogParser()
        _ = parser.ingest(dshHeader(version: version))
        _ = parser.ingest(dshStart())
        _ = parser.ingest(["type": "reasoning-chunks", "seq0": 1, "time0": dshEpoch + 1000,
                           "data": ["turn": 1, "step": 1, "index": 0, "dt": [20], "texts": ["", "thought"]]])
        _ = parser.ingest(dshEvent("assistant/chunk", seq: 3, at: 2000, ["turn": 1, "step": 1,
                          "chunk": ["type": "text-delta", "index": 1, "text": "answer"]]))
        _ = parser.ingest(dshEvent("assistant/chunk", seq: 4, at: 3000, ["turn": 1, "step": 1,
                          "chunk": ["type": "finish", "reason": ["kind": "stop"]]]))
        var event = dshMessage(seq: 5, at: 3001, stream: [])
        if version == 0 {
            var data = try #require(event["data"] as? [String: Any])
            data.removeValue(forKey: "message")
            data["content"] = []
            data["provenance"] = ["provider": "opencode-go", "model": "deepseek-v4.1-flash"]
            event["data"] = data
        }
        let record = try #require(parser.ingest(event).first)
        #expect(record.firstToken == dshDate(1020))
        #expect(record.firstVisible == dshDate(2000))
        #expect(record.end == dshDate(3000))
        #expect(record.outputTokens == 100)
    }
}

@Suite struct DeepSeekWatcherTests {
    @Test func rawIncrementalTornLineIdleAndExpiration() throws {
        let directory = try DshDirectory()
        let start = try dshLines([dshHeader(), dshStart()])
        let response = try dshLines([dshMessage()])
        try directory.put(start + response.prefix(response.count / 2))
        let watcher = DeepSeekLogWatcher(roots: [directory.root])
        #expect(watcher.poll(now: dshDate(100)).isEmpty)
        #expect(watcher.hasLogs)
        #expect(watcher.isAwaiting(now: dshDate(100)))
        #expect(!watcher.isAwaiting(now: dshDate(600_000)))
        #expect(watcher.poll(now: dshDate(200)).isEmpty)
        try directory.append(Data(response.dropFirst(response.count / 2)))
        #expect(watcher.poll(now: dshDate(6001)).count == 1)
        #expect(!watcher.isAwaiting(now: dshDate(6001)))
        #expect(watcher.poll(now: dshDate(9000)).isEmpty)
        #expect(watcher.latestModel == "deepseek-v4.1-flash")
        #expect(watcher.lastRequestAt == dshDate())
    }

    @Test func compressedConcatenatedFramesAcrossArbitraryAppendBoundaries() throws {
        let directory = try DshDirectory()
        let name = "session.v3.jsonl.zstd"
        try directory.put(try dshFrame(dshLines([dshHeader()])), name: name)
        let watcher = DeepSeekLogWatcher(roots: [directory.root])
        #expect(watcher.poll(now: dshDate()).isEmpty)
        #expect(!watcher.hasLogs)
        try directory.append(try dshFrame(dshLines([dshStart()])), name: name)
        #expect(watcher.poll(now: dshDate(1)).isEmpty)
        #expect(watcher.isAwaiting(now: dshDate(1)))
        let response = try dshFrame(dshLines([dshMessage()]))
        var records: [LogRecord] = []
        for index in stride(from: 0, to: response.count, by: 7) {
            try directory.append(Data(response[index..<min(index + 7, response.count)]), name: name)
            records += watcher.poll(now: dshDate(6001))
        }
        #expect(records.count == 1)
        #expect(records.first?.firstToken == dshDate(2010))
        #expect(records.first?.end == dshDate(5998))
        #expect(watcher.hasLogs)
        #expect(watcher.limitations.isEmpty)
        #expect(watcher.poll(now: dshDate(9000)).isEmpty)
    }

    @Test func tornChecksumTailRetainsCompleteRowsAndContinuesAfterGrowth() throws {
        let directory = try DshDirectory()
        let name = "session.v3.jsonl.zstd"
        let prefix = try dshFrame(dshLines([dshHeader()]))
        let batch = try dshFrame(dshLines([dshStart(), dshMessage()]))
        try directory.put(prefix + batch.dropLast(2), name: name)
        let watcher = DeepSeekLogWatcher(roots: [directory.root])
        var records = watcher.poll(now: dshDate(6001))
        try directory.append(Data(batch.suffix(2)), name: name)
        records += watcher.poll(now: dshDate(6002))
        #expect(records.count == 1)
        #expect(watcher.limitations.isEmpty)
        try directory.append(try dshFrame(dshLines([dshStart(seq: 2, at: 10000, step: 2),
                        dshMessage(seq: 3, at: 16000, id: "message-second", step: 2, stream: [])])), name: name)
        #expect(watcher.poll(now: dshDate(16001)).first?.key == "deepseek:message-second")
    }

    @Test func tornFrameRewrittenInPlaceRestartsDecoderWithoutDuplicatingRecoveredRows() throws {
        let directory = try DshDirectory()
        let name = "session.v3.jsonl.zstd"
        let prefix = try dshFrame(dshLines([dshHeader()]))
        let batch = try dshFrame(dshLines([dshStart(), dshMessage()]))
        try directory.put(prefix + batch.dropLast(2), name: name)
        let watcher = DeepSeekLogWatcher(roots: [directory.root])
        var records = watcher.poll(now: dshDate(6001))
        let handle = try FileHandle(forWritingTo: directory.file(name))
        try handle.truncate(atOffset: UInt64(prefix.count))
        _ = try handle.seek(toOffset: UInt64(prefix.count))
        // Official crash repair republishes the recovered committed rows in new frames.
        try handle.write(contentsOf: dshFrame(dshLines([dshStart()])))
        try handle.write(contentsOf: dshFrame(dshLines([dshMessage()])))
        try handle.close()
        records += watcher.poll(now: dshDate(6002))
        #expect(records.count == 1)
        #expect(watcher.limitations.isEmpty)
        #expect(!watcher.isAwaiting(now: dshDate(6002)))
    }

    @Test func rawUTF8RowsSplitAtEveryByteAreOnlyAcceptedAfterNewline() throws {
        let directory = try DshDirectory()
        try directory.put(try dshLines([dshHeader(), dshStart()]))
        let watcher = DeepSeekLogWatcher(roots: [directory.root])
        _ = watcher.poll(now: dshDate(1))
        let response = try dshLines([dshMessage(stream: [
            dshChunk("text-delta", at: 1000, fields: ["index": 0, "text": "synthetic λ"]),
            dshChunk("finish", at: 2000, fields: ["reason": ["kind": "stop"]]),
        ])])
        for byte in response.dropLast() {
            try directory.append(Data([byte]))
            #expect(watcher.poll(now: dshDate(2001)).isEmpty)
        }
        try directory.append(Data([10]))
        let record = try #require(watcher.poll(now: dshDate(2001)).first)
        #expect(record.firstToken == dshDate(1000))
        #expect(record.firstVisible == dshDate(1000))
        #expect(!watcher.isAwaiting(now: dshDate(2001)))
    }

    @Test func historicalSeedLengthSkipsInheritedResponses() throws {
        let directory = try DshDirectory()
        try directory.put(try dshLines([
            dshHeader(version: 1, seedLength: 2), dshStart(), dshMessage(id: "inherited", stream: []),
            dshStart(seq: 2, at: 10000, step: 2), dshMessage(seq: 3, at: 16000, id: "child", step: 2, stream: []),
        ]), name: "session.v1.jsonl")
        let watcher = DeepSeekLogWatcher(roots: [directory.root])
        #expect(watcher.poll(now: dshDate(16001)).map(\.key) == ["deepseek:child"])
    }

    @Test func highestGenerationWinsAndFutureVersionNeverFallsBack() throws {
        let directory = try DshDirectory()
        try directory.put(try dshLines([dshHeader(version: 1), dshStart(), dshMessage(id: "old")]), name: "session.v1.jsonl")
        try directory.put(try dshFrame(dshLines([dshHeader(version: 4), dshStart(), dshMessage(id: "current")])), name: "session.v4.jsonl.zstd")
        let watcher = DeepSeekLogWatcher(roots: [directory.root])
        #expect(watcher.poll(now: dshDate(6001)).map(\.key) == ["deepseek:current"])
        try directory.put(try dshLines([dshHeader(version: 5)]), name: "session.v5.jsonl")
        #expect(watcher.poll(now: dshDate(10002)).isEmpty)
        #expect(!watcher.hasLogs)
        #expect(watcher.limitations.contains(where: { $0.contains("v5") }))
    }

    @Test func seededNestedAncestorMarkersUseTheLastInheritedCut() throws {
        let directory = try DshDirectory()
        let rows = [dshHeader(seeded: true), dshStart(), dshMessage(id: "inherited-one"),
                    dshEvent("session/end-seed", seq: 2, at: 6001, ["inherited": true]),
                    dshStart(seq: 3, at: 10000, step: 2), dshMessage(seq: 4, at: 16000, id: "inherited-two", step: 2),
                    dshEvent("session/end-seed", seq: 5, at: 16001, ["inherited": true]),
                    dshEvent("turn/end", seq: 6, at: 16002, ["turn": 1, "reason": ["kind": "forked"]]),
                    dshStart(seq: 7, at: 20000, step: 3), dshMessage(seq: 8, at: 26000, id: "owned", step: 3, stream: [])]
        try directory.put(try dshFrame(dshLines(rows)), name: "session.v3.jsonl.zstd")
        let watcher = DeepSeekLogWatcher(roots: [directory.root])
        #expect(watcher.poll(now: dshDate(26001)).map(\.key) == ["deepseek:owned"])
        #expect(watcher.hasLogs)
        #expect(!watcher.isAwaiting(now: dshDate(26001)))
        #expect(watcher.poll(now: dshDate(26002)).isEmpty)
    }

    @Test func inheritedBoundaryMissingIsNotUsableTelemetry() throws {
        let directory = try DshDirectory()
        try directory.put(try dshLines([dshHeader(seeded: true), dshStart(), dshMessage()]))
        let watcher = DeepSeekLogWatcher(roots: [directory.root])
        #expect(watcher.poll(now: dshDate(6001)).isEmpty)
        #expect(!watcher.hasLogs)
        #expect(!watcher.isAwaiting(now: dshDate(6001)))
        #expect(watcher.limitations.contains(where: { $0.contains("inherited boundary") }))
    }

    @Test func replacementAndReplaySameMessageIdentityDoNotDuplicate() throws {
        let directory = try DshDirectory()
        let initial = try dshLines([dshHeader(), dshStart(), dshMessage()])
        try directory.put(initial)
        let watcher = DeepSeekLogWatcher(roots: [directory.root])
        #expect(watcher.poll(now: dshDate(6001)).count == 1)
        // Atomic replacement changes inode; the decoder/parser restart, but response keys stay stable.
        try directory.put(initial + dshLines([dshStart(seq: 2, at: 7000), dshMessage(seq: 3, at: 9000, stream: [])]))
        #expect(watcher.poll(now: dshDate(9001)).isEmpty)
        #expect(!watcher.isAwaiting(now: dshDate(9001)))
    }

    @Test func rootPresenceHeaderOnlyBackfillAndMixedEncodingAreNotFalseAvailability() throws {
        let directory = try DshDirectory()
        let empty = DeepSeekLogWatcher(roots: [directory.root])
        #expect(empty.poll(now: dshDate()).isEmpty)
        #expect(!empty.hasLogs)
        try directory.put(try dshLines([dshHeader()]))
        #expect(empty.poll(now: dshDate(4001)).isEmpty)
        #expect(!empty.hasLogs)
        try directory.put(try dshLines([dshHeader(), dshStart(), dshMessage()]), modified: dshDate(-800_000_000))
        let old = DeepSeekLogWatcher(roots: [directory.root])
        #expect(old.poll(now: dshDate()).isEmpty)
        #expect(!old.hasLogs)
        try directory.put(try dshLines([dshHeader(), dshStart()]))
        try directory.put(try dshFrame(dshLines([dshHeader()])), name: "session.v3.jsonl.zstd")
        let mixed = DeepSeekLogWatcher(roots: [directory.root])
        #expect(mixed.poll(now: dshDate()).isEmpty)
        #expect(!mixed.hasLogs)
        #expect(mixed.limitations.contains(where: { $0.contains("Ambiguous") }))
    }

    @Test func oversizedToolRowIsBoundedAndFollowingTelemetrySurvives() throws {
        let directory = try DshDirectory()
        var data = try dshLines([dshHeader(), dshStart()])
        data.append(try dshLines([dshEvent("tool/result", seq: 1, at: 1000, ["turn": 1, "step": 1, "fixture": String(repeating: "x", count: 4 * 1024 * 1024 + 1)])]))
        data.append(try dshLines([dshMessage(seq: 2)]))
        try directory.put(data)
        let watcher = DeepSeekLogWatcher(roots: [directory.root])
        #expect(watcher.poll(now: dshDate(6001)).count == 1)
        #expect(watcher.limitations.contains(where: { $0.contains("4 MiB") }))
        #expect(!watcher.isAwaiting(now: dshDate(6001)))
    }

    @Test func completeFrameCorruptionIsReportedAndNeverShowsActive() throws {
        let directory = try DshDirectory()
        var corrupted = try dshFrame(dshLines([dshHeader(), dshStart()]))
        // An unrecognizable frame prefix is structural corruption of a committed frame.
        corrupted[corrupted.startIndex] ^= 0xFF
        try directory.put(corrupted, name: "session.v3.jsonl.zstd")
        let watcher = DeepSeekLogWatcher(roots: [directory.root])
        _ = watcher.poll(now: dshDate(1))
        #expect(!watcher.hasLogs)
        #expect(!watcher.isAwaiting(now: dshDate(1)))
        #expect(watcher.limitations.contains(where: { $0.contains("corrupt") }))
    }

    @Test func canonicalNamesAndDefaultSourceAreExact() {
        #expect(DeepSeekLogWatcher.generation("session.jsonl") == 0)
        #expect(DeepSeekLogWatcher.generation("session.v4.jsonl.zstd") == 4)
        #expect(DeepSeekLogWatcher.generation("session.v10.jsonl") == 10)
        #expect(DeepSeekLogWatcher.generation("session.v04.jsonl") == nil)
        #expect(DeepSeekLogWatcher.generation("copy-session.v3.jsonl") == nil)
        let home = URL(fileURLWithPath: "/fixture-home")
        #expect(DeepSeekLogWatcher(home: home, environment: [:]).roots == [home.appendingPathComponent(".dsh/sessions")])
        #expect(DeepSeekLogWatcher(home: home, environment: ["DSH_HOME": "   "]).roots == [home.appendingPathComponent(".dsh/sessions")])
        #expect(DeepSeekLogWatcher(home: home, environment: ["DSH_HOME": "/elsewhere/dsh"]).roots
            == [URL(fileURLWithPath: "/elsewhere/dsh/sessions")])
    }
}
