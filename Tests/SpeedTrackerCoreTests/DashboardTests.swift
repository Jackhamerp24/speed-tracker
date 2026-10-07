import Foundation
import Testing
@testable import SpeedTrackerCore

private let dashboardEpoch = Date(timeIntervalSince1970: 1_700_000_000)

private func dashboardRecord(
    id: UUID = UUID(), at: Date = dashboardEpoch, harness: String = "OMP", route: String = "openai",
    host: String = "api.openai.com", model: String = "model-a", status: Int = 200,
    ttft: Double? = 2, generation: Double? = 4, total: Double = 6, tokens: Int = 100,
    estimated: Bool = false, tps: Double? = 25, aborted: Bool = false,
    source: String? = "proxy", key: String? = nil
) -> RequestRecord {
    RequestRecord(
        id: id, startedAt: at, harness: harness, route: route, upstreamHost: host,
        format: .openAIResponses, model: model, streamed: true, status: status,
        ttft: ttft, firstVisible: nil, ttfb: nil, generation: generation, total: total,
        inputTokens: nil, cachedInputTokens: nil, outputTokens: tokens, reasoningTokens: nil,
        tokensEstimated: estimated, tps: tps, aborted: aborted, source: source, sourceKey: key
    )
}

private func withDashboardHistory(_ body: (URL) throws -> Void) throws {
    let directory = FileManager.default.temporaryDirectory.appendingPathComponent("speedtracker-dashboard-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: directory) }
    try body(directory.appendingPathComponent("history.jsonl"))
}

private func historyData(_ records: [RequestRecord]) throws -> Data {
    let encoder = JSONEncoder()
    encoder.dateEncodingStrategy = .iso8601
    var data = Data()
    for record in records {
        data.append(try encoder.encode(record))
        data.append(0x0A)
    }
    return data
}

@Suite struct DashboardProviderTests {
    @Test func endpointHostTakesPrecedenceOverRouteAndModel() {
        let direct = ProviderIdentity(record: dashboardRecord(route: "anthropic", host: "HTTPS://API.OPENAI.COM.:443/v1", model: "claude-opus"))
        #expect(direct.id == "openai")
        #expect(direct.host == "api.openai.com")
        #expect(!direct.isUnverified)

        let gateway = ProviderIdentity(record: dashboardRecord(route: "openai", host: "https://user:password@API.gateway.example:443/v1?api_key=secret", model: "gpt-model"))
        #expect(gateway.id == "host:api.gateway.example")
        #expect(gateway.host == "api.gateway.example")
        #expect(!gateway.evidence.contains("password"))
        #expect(!gateway.evidence.contains("secret"))
        #expect(gateway.id != direct.id)

        let routeHost = ProviderIdentity(record: dashboardRecord(route: "https://router.example/v1", host: "openai"))
        #expect(routeHost.id == "host:router.example")
        let unknown = ProviderIdentity(record: dashboardRecord(route: "", host: "", model: "claude-opus"))
        #expect(unknown.id == "unknown")
        #expect(unknown.isUnverified)
        let malformed = ProviderIdentity(record: dashboardRecord(route: "", host: "provider?api_key=secret"))
        #expect(malformed.id == "unknown")
        #expect(!malformed.evidence.contains("secret"))
        #expect(!malformed.displayName.contains("secret"))
    }

    @Test func codexIsDistinctAndAliasesNormalize() {
        let codexLabel = ProviderIdentity(record: dashboardRecord(host: "  OPENAI_CODEX  ", source: "log"))
        let codexHost = ProviderIdentity(record: dashboardRecord(host: "chatgpt.com"))
        let direct = ProviderIdentity(record: dashboardRecord(host: "api.openai.com"))
        #expect(codexLabel.id == "openai-codex")
        #expect(codexHost.id == codexLabel.id)
        #expect(codexLabel.id != direct.id)
        #expect(codexLabel.isUnverified)
        #expect(ProviderIdentity(record: dashboardRecord(host: "x.ai", source: "log")).id == "xai")
        #expect(ProviderIdentity(record: dashboardRecord(host: "api.moonshot.cn")).id == "moonshot")
    }

    @Test func observedAndAttributedEndpointsRetainDifferentConfidence() {
        let proxy = ProviderIdentity(record: dashboardRecord(host: "api.openai.com", source: "proxy"))
        let network = ProviderIdentity(record: dashboardRecord(host: "api.openai.com", source: "network"))
        let legacy = ProviderIdentity(record: dashboardRecord(host: "api.openai.com", source: nil))
        #expect(proxy.id == network.id && proxy.id == legacy.id)
        #expect(!proxy.isUnverified)
        #expect(network.isUnverified)
        #expect(legacy.isUnverified)
        let mixed = DashboardReport(records: [
            dashboardRecord(host: "api.openai.com", source: "proxy"),
            dashboardRecord(host: "api.openai.com", source: "network")
        ])
        #expect(mixed.providers[0].isUnverified)
    }

    @Test func legacyLogDefaultsAreNotDirectEndpointProof() {
        for (harness, label) in [("Claude Code", "anthropic"), ("Codex", "openai")] {
            let identity = ProviderIdentity(record: dashboardRecord(harness: harness, route: label, host: label, source: "log"))
            #expect(identity.host == nil)
            #expect(identity.isUnverified)
        }
        let reportedHost = ProviderIdentity(record: dashboardRecord(host: "api.openai.com", source: "log"))
        #expect(reportedHost.isUnverified)

        let report = DashboardReport(records: [
            dashboardRecord(harness: "Codex", host: "api.openai.com"),
            dashboardRecord(harness: "Codex", host: "openai", source: "log"),
        ])
        #expect(report.groups.count == 1)
        #expect(report.groups[0].provider.isUnverified)
        #expect(report.providers[0].isUnverified)
    }

    @Test func localEndpointsAreExplicitAndSeparateByPort() {
        let ollama = ProviderIdentity(record: dashboardRecord(route: "ollama", host: "http://127.0.0.1:11434"))
        let studio = ProviderIdentity(record: dashboardRecord(route: "lmstudio", host: "http://127.0.0.1:1234"))
        let ipv6 = ProviderIdentity(record: dashboardRecord(host: "[::1]:11434"))
        #expect(ollama.id == "local:127.0.0.1:11434")
        #expect(studio.id != ollama.id)
        #expect(ipv6.id == "local:[::1]:11434")
        #expect(ipv6.host == "::1")
    }
}

@Suite struct DashboardAnalyticsTests {
    @Test func providerHarnessMatrixKeepsGatewaysSeparate() {
        let report = DashboardReport(records: [
            dashboardRecord(harness: "OMP", host: "api.openai.com", model: "shared-model"),
            dashboardRecord(harness: "Codex", host: "api.openai.com", model: "shared-model"),
            dashboardRecord(harness: "OMP", host: "gateway.example", model: "shared-model"),
            dashboardRecord(harness: "OMP", host: "gateway.example", model: "other-model"),
            dashboardRecord(harness: "OMP", route: "", host: "", model: "shared-model"),
        ])
        #expect(report.groups.count == 4)
        #expect(Set(report.groups.map(\.id)).count == 4)
        #expect(report.groups.first { $0.provider.id == "host:gateway.example" }?.summary.count == 2)
        #expect(report.groups.filter { $0.provider.id == "openai" }.count == 2)
        #expect(report.groups.contains { $0.provider.id == "unknown" })
    }

    @Test func eachFilterOffersOnlyWhatTheOtherTwoHaveCallsWith() {
        let records = [
            dashboardRecord(harness: "OMP", host: "api.openai.com", model: "gpt-6-astra"),
            dashboardRecord(harness: "Codex", host: "api.openai.com", model: "gpt-6-astra"),
            dashboardRecord(harness: "Codex", host: "api.openai.com", model: "gpt-6.1-sol"),
            dashboardRecord(harness: "OMP", host: "api.deepseek.com", model: "deepseek-flash"),
            dashboardRecord(harness: "Claude Code", host: "api.anthropic.com", model: "claude-opus-5-5"),
        ]
        let all = DashboardReport(records: records)
        #expect(all.harnessOptions == ["Claude Code", "Codex", "OMP"])
        #expect(all.modelOptions == ["claude-opus-5-5", "deepseek-flash", "gpt-6-astra", "gpt-6.1-sol"])
        #expect(all.providerOptions.map(\.id) == all.providers.map(\.id))

        // Picking a model leaves only the harnesses and providers that have used it.
        let astra = DashboardReport(records: records, query: DashboardQuery(model: "gpt-6-astra"))
        #expect(astra.harnessOptions == ["Codex", "OMP"])
        #expect(astra.providerOptions.map(\.id) == ["openai"])
        // Its own list is not narrowed, so another model can still be chosen.
        #expect(astra.modelOptions.count == 4)

        // Picking a harness leaves only its models and providers.
        let omp = DashboardReport(records: records, query: DashboardQuery(harness: "OMP"))
        #expect(omp.modelOptions == ["deepseek-flash", "gpt-6-astra"])
        #expect(Set(omp.providerOptions.map(\.id)) == ["deepseek", "openai"])
        #expect(omp.harnessOptions.count == 3)

        // Picking a provider does the same.
        let openai = DashboardReport(records: records, query: DashboardQuery(providerID: "openai"))
        #expect(openai.harnessOptions == ["Codex", "OMP"])
        #expect(openai.modelOptions == ["gpt-6-astra", "gpt-6.1-sol"])

        // Two filters together narrow the third by both.
        let both = DashboardReport(records: records, query: DashboardQuery(harness: "OMP", providerID: "openai"))
        #expect(both.modelOptions == ["gpt-6-astra"])
        #expect(both.harnessOptions == ["Codex", "OMP"])
        #expect(Set(both.providerOptions.map(\.id)) == ["deepseek", "openai"])
        // The unfiltered lists still describe the whole date range.
        #expect(both.harnesses.count == 3)
        #expect(both.models.count == 4)
    }

    @Test func facetsUseDateWindowBeforeDimensionFilters() {
        let early = dashboardEpoch.addingTimeInterval(-1)
        let end = dashboardEpoch.addingTimeInterval(10)
        let selected = dashboardRecord(at: dashboardEpoch, harness: "OMP", host: "gateway.example", model: "selected")
        let report = DashboardReport(records: [
            dashboardRecord(at: early, harness: "Before", model: "before"), selected,
            dashboardRecord(at: dashboardEpoch.addingTimeInterval(5), harness: "Codex", model: "other"),
            dashboardRecord(at: end, harness: "After", model: "after"),
        ], query: DashboardQuery(from: dashboardEpoch, through: end, harness: "OMP", providerID: "host:gateway.example", model: "selected"))
        #expect(report.records.map(\.id) == [selected.id])
        #expect(report.harnesses == ["Codex", "OMP"])
        #expect(report.models == ["other", "selected"])
        #expect(Set(report.providers.map(\.id)) == ["openai", "host:gateway.example"])
        #expect(report.summary.count == 1)
        #expect(report.groups.count == 1)
        #expect(!DashboardQuery(from: end, through: dashboardEpoch).includes(selected))
    }

    @Test func r7PercentilesAreDeterministicAndInterpolate() {
        let records = (1...20).map { dashboardRecord(ttft: Double($0), tps: Double($0) * 10) }
        let summary = DashboardSummary(records: records)
        #expect(summary.medianTTFT == 10.5)
        #expect(abs(summary.p95TTFT! - 19.05) < 0.000_001)
        #expect(summary.medianTPS == 105)
        #expect(abs(summary.p95TPS! - 190.5) < 0.000_001)
        #expect(summary.latencyCount == 20)
        #expect(summary.speedCount == 20)
        let reversed = DashboardSummary(records: Array(records.reversed()))
        #expect(reversed.p95TTFT == summary.p95TTFT)
        let single = DashboardSummary(records: [records[0]])
        #expect(single.medianTTFT == single.p95TTFT)
        #expect(DashboardMetric.speed.value(in: summary) == 105)
        #expect(DashboardMetric.calls.value(in: summary) == 20)
    }

    @Test func missingTimingAndWholeRequestFallbackDoNotBecomeGenerationSpeed() {
        let log = LogRecord(
            key: "legacy:call", harness: "Codex", model: "model", provider: "openai",
            requestStart: dashboardEpoch, firstToken: nil, end: dashboardEpoch.addingTimeInterval(10), outputTokens: 100
        ).makeRecord()
        #expect(log.tps == 10)
        #expect(log.generation == nil)
        let summary = DashboardSummary(records: [log, dashboardRecord(ttft: nil, generation: 0, tps: 50)])
        #expect(summary.count == 2)
        #expect(summary.medianTTFT == nil)
        #expect(summary.p95TTFT == nil)
        #expect(summary.medianTPS == nil)
        #expect(summary.p95TPS == nil)
        #expect(summary.roundTripCount == 2)
        #expect(summary.outputTokens == 200)
        let report = DashboardReport(records: [log])
        #expect(report.histogram(for: .speed).isEmpty)
        #expect(report.histogram(for: .latency).isEmpty)
        #expect(report.trends[0].summary.medianTTFT == nil)
    }

    @Test func interruptedAndKnownErrorsStayInVolumeButNotPerformance() {
        let summary = DashboardSummary(records: [
            dashboardRecord(ttft: 1, tps: 10),
            dashboardRecord(ttft: 100, estimated: true, tps: 1000, aborted: true),
            dashboardRecord(status: 503, ttft: 200, tps: 2000),
            dashboardRecord(status: 429, ttft: 300, tps: 3000),
        ])
        #expect(summary.count == 4)
        #expect(summary.interruptedCount == 3)
        #expect(summary.estimatedCount == 1)
        #expect(summary.outputTokens == 400)
        #expect(summary.medianTTFT == 1)
        #expect(summary.p95TPS == 10)
        #expect(summary.latencyCount == 1)
        #expect(summary.speedCount == 1)
    }

    @Test func invalidMeasurementsAreExcludedButZeroLatencyIsReal() {
        let summary = DashboardSummary(records: [
            dashboardRecord(ttft: .nan, tps: .infinity),
            dashboardRecord(ttft: -1, generation: -1, tokens: -100, tps: -5),
            dashboardRecord(ttft: .infinity, generation: .infinity, tps: 999),
            dashboardRecord(ttft: 0, tps: 0),
            dashboardRecord(ttft: nil, generation: .nan, tps: 50),
        ])
        #expect(summary.count == 5)
        #expect(summary.outputTokens == 400)
        #expect(summary.latencyCount == 1)
        #expect(summary.speedCount == 1)
        #expect(summary.medianTTFT == 0)
        #expect(summary.medianTPS == 0)
        #expect(summary.roundTripCount == 0)
    }

    @Test func reportDeduplicatesBySourceKeyOrUUIDWithFirstOccurrenceWinning() {
        let first = dashboardRecord(key: "stable-source")
        let duplicateSource = dashboardRecord(model: "must-not-replace", key: "stable-source")
        let noSource = dashboardRecord(source: nil)
        var duplicateUUID = noSource
        duplicateUUID.model = "must-not-replace"
        let namespacedKey = dashboardRecord(key: noSource.id.uuidString)
        let report = DashboardReport(records: [first, duplicateSource, noSource, duplicateUUID, namespacedKey])
        #expect(report.records.count == 3)
        #expect(!report.records.contains { $0.model == "must-not-replace" })
        #expect(report.summary.outputTokens == 300)
        #expect(report.records.contains { $0.id == namespacedKey.id })

        var outsideWindow = first
        outsideWindow.startedAt = dashboardEpoch.addingTimeInterval(-10)
        #expect(DashboardReport(records: [outsideWindow, first], query: DashboardQuery(from: dashboardEpoch)).records.isEmpty)
    }

    @Test func trendsAreChronologicalSparseAndBounded() {
        let records = [
            dashboardRecord(at: dashboardEpoch.addingTimeInterval(7200), ttft: nil, tps: nil),
            dashboardRecord(at: dashboardEpoch, ttft: 1, tps: 10),
            dashboardRecord(at: dashboardEpoch.addingTimeInterval(1), ttft: 3, tps: 30),
        ]
        let report = DashboardReport(records: records)
        #expect(report.trends.count == 2)
        #expect(report.trends[0].date < report.trends[1].date)
        #expect(report.trends[0].summary.count == 2)
        #expect(report.trends[0].summary.medianTTFT == 2)
        #expect(report.trends[1].summary.medianTPS == nil)
        #expect(report.records[0].startedAt > report.records[1].startedAt)

        let longHistory = (0..<1000).map { dashboardRecord(at: dashboardEpoch.addingTimeInterval(Double($0) * 86400)) }
        let longReport = DashboardReport(records: longHistory)
        #expect(longReport.trends.count <= 120)
        #expect(longReport.trends.reduce(0) { $0 + $1.summary.count } == 1000)
        #expect(longReport.trends.map(\.date) == longReport.trends.map(\.date).sorted())
    }

    @Test func histogramsUseValidSamplesAndIncludeMaximumBoundary() {
        let report = DashboardReport(records: [
            dashboardRecord(ttft: 0, tps: 0), dashboardRecord(ttft: 1, tps: 10),
            dashboardRecord(ttft: 2, tps: 20), dashboardRecord(ttft: 3, tps: 30),
            dashboardRecord(ttft: nil, generation: nil, tps: 500),
            dashboardRecord(ttft: 50, tps: 500, aborted: true),
        ])
        let latency = report.histogram(for: .latency)
        let speed = report.histogram(for: .speed)
        #expect(latency.reduce(0) { $0 + $1.count } == 4)
        #expect(speed.reduce(0) { $0 + $1.count } == 4)
        #expect(latency.last?.upper == 3)
        #expect(speed.last?.upper == 30)
        #expect(speed.last!.count > 0)
        #expect(report.histogram(for: .calls).isEmpty)
        #expect(report.histogram(for: .tokens).isEmpty)
        let constant = DashboardReport(records: [dashboardRecord(ttft: 0, tps: 0)])
        #expect(constant.histogram(for: .speed).first?.count == 1)
        #expect(constant.histogram(for: .speed).first!.upper > 0)
    }

    @Test func sourcesAndEmptyStatesAreHonest() {
        let report = DashboardReport(records: [
            dashboardRecord(source: "log"), dashboardRecord(source: "network"),
            dashboardRecord(source: "proxy"), dashboardRecord(source: nil), dashboardRecord(source: "unrecognized"),
        ])
        #expect(report.sources.map(\.id) == ["log", "network", "proxy", "unknown"])
        #expect(report.sources.last?.count == 2)
        let empty = DashboardReport(records: [])
        #expect(empty.summary.count == 0)
        #expect(empty.summary.medianTTFT == nil)
        #expect(empty.summary.p95TPS == nil)
        #expect(empty.groups.isEmpty)
        #expect(empty.providers.isEmpty)
        #expect(empty.trends.isEmpty)
        #expect(empty.histogram(for: .latency).isEmpty)
    }
}

@Suite struct DashboardHistoryTests {
    @Test func filtersReadBeyondThousandRecordTailAndRespectBoundaries() throws {
        try withDashboardHistory { url in
            // Spelled out step by step: as one expression this is too much for some compilers to type-check.
            let records: [RequestRecord] = (0..<1505).map { (index: Int) -> RequestRecord in
                let early = index < 10
                let start: Date = dashboardEpoch.addingTimeInterval(Double(index))
                let harness: String = early ? "Earlier Harness" : "OMP"
                let host: String = early ? "gateway.example" : "api.openai.com"
                let model: String = early ? "old-model" : "new-model"
                return dashboardRecord(at: start, harness: harness, host: host, model: model, key: "call:\(index)")
            }
            try historyData(records).write(to: url)
            let store = HistoryStore(url: url)
            let all = try store.read(matching: DashboardQuery())
            #expect(all.records.count == 1505)
            #expect(all.records.first?.sourceKey == "call:0")
            #expect(all.records.last?.sourceKey == "call:1504")
            let selected = try store.read(matching: DashboardQuery(
                from: dashboardEpoch.addingTimeInterval(2), through: dashboardEpoch.addingTimeInterval(5),
                harness: "Earlier Harness", providerID: "host:gateway.example", model: "old-model"
            ))
            #expect(selected.records.map(\.sourceKey) == ["call:2", "call:3", "call:4"])
            #expect(selected.skippedLines == 0)
            #expect(try store.read(matching: DashboardQuery(from: dashboardEpoch.addingTimeInterval(5), through: dashboardEpoch.addingTimeInterval(2))).records.isEmpty)
        }
    }

    @Test func queuedAppendsAreVisibleToImmediateReadInOrder() throws {
        try withDashboardHistory { url in
            let store = HistoryStore(url: url)
            for index in 0..<50 { store.append(dashboardRecord(key: "queued:\(index)")) }
            let history = try store.read(matching: DashboardQuery())
            #expect(history.records.map(\.sourceKey) == (0..<50).map { Optional("queued:\($0)") })
            #expect(history.skippedLines == 0)
            #expect(store.loadSourceKeys().count == 50)
            #expect(store.loadRecent(limit: 5).map(\.sourceKey) == (45..<50).map { Optional("queued:\($0)") })
        }
    }

    @Test func malformedAndTruncatedLinesAreCountedWithoutLosingGoodRecords() throws {
        try withDashboardHistory { url in
            let first = dashboardRecord(key: "first")
            let second = dashboardRecord(key: "second")
            var data = try historyData([first])
            data.append(Data("not-json\n{\"partial\":\n".utf8))
            data.append(try historyData([second]))
            data.append(Data("{\"id\":\"unfinished".utf8))
            try data.write(to: url)
            let history = try HistoryStore(url: url).read(matching: DashboardQuery())
            #expect(history.records.map(\.sourceKey) == ["first", "second"])
            #expect(history.skippedLines == 3)
        }
    }

    @Test func completeFinalLineWithoutNewlineIsAccepted() throws {
        try withDashboardHistory { url in
            var data = try historyData([dashboardRecord(key: "final")])
            data.removeLast()
            try data.write(to: url)
            let history = try HistoryStore(url: url).read(matching: DashboardQuery())
            #expect(history.records.count == 1)
            #expect(history.skippedLines == 0)
        }
    }

    @Test func boundedCarrySkipsOversizedLineAndRecoversAcrossChunks() throws {
        try withDashboardHistory { url in
            var largeButValid = dashboardRecord(key: "large-valid")
            largeButValid.model = String(repeating: "m", count: 70_000)
            var data = try historyData([largeButValid])
            data.append(Data(repeating: 0x78, count: 1_048_577))
            data.append(0x0A)
            data.append(try historyData([dashboardRecord(key: "after-oversized")]))
            try data.write(to: url)
            let history = try HistoryStore(url: url).read(matching: DashboardQuery())
            #expect(history.records.map(\.sourceKey) == ["large-valid", "after-oversized"])
            #expect(history.records[0].model.count == 70_000)
            #expect(history.skippedLines == 1)
        }
    }

    @Test func streamingDedupIsStableAndDoesNotCountDuplicatesAsMalformed() throws {
        try withDashboardHistory { url in
            let keyed = dashboardRecord(model: "first", key: "stable")
            let duplicateKey = dashboardRecord(model: "second", key: "stable")
            let unkeyed = dashboardRecord(source: nil)
            var duplicateID = unkeyed
            duplicateID.model = "second"
            try historyData([keyed, duplicateKey, unkeyed, duplicateID]).write(to: url)
            let history = try HistoryStore(url: url).read(matching: DashboardQuery())
            #expect(history.records.map(\.id) == [keyed.id, unkeyed.id])
            #expect(history.records[0].model == "first")
            #expect(history.skippedLines == 0)
            #expect(try HistoryStore(url: url).read(matching: DashboardQuery(model: "second")).records.isEmpty)
        }
    }

    @Test func legacyHistoryWithoutProvenanceFieldsStillDecodesHonestly() throws {
        try withDashboardHistory { url in
            let encoder = JSONEncoder()
            encoder.dateEncodingStrategy = .iso8601
            let legacy = dashboardRecord(harness: "Codex", host: "openai", ttft: nil, generation: nil, tps: 10)
            var object = try #require(try JSONSerialization.jsonObject(with: encoder.encode(legacy)) as? [String: Any])
            object.removeValue(forKey: "source")
            object.removeValue(forKey: "sourceKey")
            // Unknown additive fields from a newer writer must not break old history inspection.
            object["futureMeasurement"] = 123
            try JSONSerialization.data(withJSONObject: object).write(to: url)
            let history = try HistoryStore(url: url).read(matching: DashboardQuery())
            let record = try #require(history.records.first)
            #expect(record.source == nil)
            #expect(record.sourceKey == nil)
            let identity = ProviderIdentity(record: record)
            #expect(identity.id == "openai")
            #expect(identity.isUnverified)
            #expect(DashboardReport(records: history.records).summary.medianTPS == nil)
            #expect(history.skippedLines == 0)
        }
    }

    @Test func nonexistentHistoryIsEmptyButOtherIOErrorsThrow() throws {
        try withDashboardHistory { url in
            let missing = try HistoryStore(url: url).read(matching: DashboardQuery())
            #expect(missing.records.isEmpty)
            #expect(missing.skippedLines == 0)
            try FileManager.default.createDirectory(at: url, withIntermediateDirectories: false)
            #expect(throws: (any Error).self) {
                try HistoryStore(url: url).read(matching: DashboardQuery())
            }
        }
    }
}
