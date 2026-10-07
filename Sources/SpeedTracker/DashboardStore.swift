import Combine
import Foundation
import SpeedTrackerCore

enum DashboardRange: String, CaseIterable, Identifiable {
    case today, week, month, all
    var id: String { rawValue }
    var title: String {
        switch self {
        case .today: return "Today"
        case .week: return "7 days"
        case .month: return "30 days"
        case .all: return "All time"
        }
    }

    func bounds(now: Date = Date(), calendar: Calendar = .current) -> (Date?, Date?) {
        let start: Date?
        switch self {
        case .today: start = calendar.startOfDay(for: now)
        case .week: start = now.addingTimeInterval(-7 * 24 * 3600)
        case .month: start = now.addingTimeInterval(-30 * 24 * 3600)
        case .all: start = nil
        }
        return (start, now.addingTimeInterval(0.001))
    }
}

enum DashboardSection: String, CaseIterable, Identifiable {
    case overview, trends, calls
    var id: String { rawValue }
    var title: String { rawValue.capitalized }
}

enum DashboardCallSort: String, CaseIterable, Identifiable {
    case newest, oldest, fastest, slowest, latency, tokens
    var id: String { rawValue }
    var title: String { self == .latency ? "Highest TTFT" : rawValue.capitalized }
}

/// Owns historical filters, never the menu-bar target. Only an open dashboard does query work.
final class DashboardStore: ObservableObject {
    @Published var range: DashboardRange = .week { didSet { page = 0; refresh() } }
    @Published var harnessFilter = "" { didSet { filtersChanged() } }
    @Published var providerFilter = "" { didSet { filtersChanged() } }
    @Published var modelFilter = "" { didSet { filtersChanged() } }
    @Published var metric: DashboardMetric = .speed
    @Published var section: DashboardSection = .overview
    @Published var callSort: DashboardCallSort = .newest { didSet { page = 0; sortRecords() } }
    @Published var page = 0
    @Published var selectedCallID: UUID?
    @Published private(set) var report = DashboardReport(records: [])
    @Published private(set) var isLoading = false
    @Published private(set) var loadError: String?
    @Published private(set) var skippedLines = 0
    @Published private(set) var lastRefreshed: Date?

    private let history: HistoryStore?
    private let worker = DispatchQueue(label: "speedtracker.dashboard", qos: .userInitiated)
    private var subscription: AnyCancellable?
    private var records: [RequestRecord]
    private var sortedRecords: [RequestRecord] = []
    private var receivedDuringLoad: [RequestRecord] = []
    private var pendingRefresh: DispatchWorkItem?
    private var timer: Timer?
    private var isOpen = false
    private var loadingHistory = false
    private var historyGeneration = 0
    private var reportGeneration = 0
    private var changingFilters = false
    private static let pageSize = 25

    init(history: HistoryStore?, metrics: MetricsStore? = nil, records: [RequestRecord] = []) {
        self.history = history
        self.records = records
        if let metrics {
            subscription = metrics.recorded.sink { [weak self] record in self?.receive(record) }
        }
    }

    var pageRecords: [RequestRecord] {
        Array(sortedRecords.dropFirst(max(0, page) * Self.pageSize).prefix(Self.pageSize))
    }
    var pageCount: Int { max(1, (sortedRecords.count + Self.pageSize - 1) / Self.pageSize) }
    var selectedCall: RequestRecord? {
        guard let selectedCallID else { return nil }
        return report.records.first { $0.id == selectedCallID }
    }

    func open() {
        guard !isOpen else { return }
        isOpen = true
        refresh()
        // Advance rolling ranges at midnight and while the app is otherwise quiet.
        let timer = Timer(timeInterval: 60, repeats: true) { [weak self] _ in self?.refresh() }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    func close() {
        isOpen = false
        historyGeneration += 1
        reportGeneration += 1
        pendingRefresh?.cancel()
        timer?.invalidate()
        timer = nil
        loadingHistory = false
        isLoading = false
        if history != nil { records.removeAll(); sortedRecords.removeAll(); report = DashboardReport(records: []) }
        receivedDuringLoad.removeAll()
    }

    func refresh() {
        historyGeneration += 1
        reportGeneration += 1
        let generation = historyGeneration
        pendingRefresh?.cancel()
        loadError = nil
        guard let history else { rebuild(); return }
        loadingHistory = true
        isLoading = true
        receivedDuringLoad.removeAll()
        let (from, through) = range.bounds()
        worker.async { [weak self] in
            let result = Result { try history.read(matching: DashboardQuery(from: from, through: through)) }
            DispatchQueue.main.async { [weak self] in
                guard let self, self.historyGeneration == generation else { return }
                self.loadingHistory = false
                switch result {
                case .success(let loaded):
                    self.records = loaded.records + self.receivedDuringLoad
                    self.receivedDuringLoad.removeAll()
                    self.skippedLines = loaded.skippedLines
                    self.rebuild()
                case .failure(let error):
                    self.isLoading = false
                    self.loadError = error.localizedDescription
                }
            }
        }
    }

    func clearFilters() {
        changingFilters = true
        harnessFilter = ""
        providerFilter = ""
        modelFilter = ""
        changingFilters = false
        filtersChanged()
    }

    func select(_ group: DashboardGroup) {
        changingFilters = true
        harnessFilter = group.harness
        providerFilter = group.provider.id
        changingFilters = false
        section = .trends
        filtersChanged()
    }

    private func receive(_ record: RequestRecord) {
        guard isOpen else { return }
        if loadingHistory { receivedDuringLoad.append(record); return }
        records.append(record)
        pendingRefresh?.cancel()
        let work = DispatchWorkItem { [weak self] in self?.rebuild() }
        pendingRefresh = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.2, execute: work)
    }

    private func filtersChanged() {
        guard !changingFilters else { return }
        page = 0
        selectedCallID = nil
        if !loadingHistory { rebuild() }
    }

    private func rebuild() {
        reportGeneration += 1
        let generation = reportGeneration
        let (from, through) = range.bounds()
        let query = DashboardQuery(
            from: from, through: through,
            harness: harnessFilter.isEmpty ? nil : harnessFilter,
            providerID: providerFilter.isEmpty ? nil : providerFilter,
            model: modelFilter.isEmpty ? nil : modelFilter
        )
        let records = self.records
        isLoading = true
        worker.async { [weak self] in
            let report = DashboardReport(records: records, query: query)
            DispatchQueue.main.async { [weak self] in
                guard let self, self.reportGeneration == generation else { return }
                self.report = report
                self.sortRecords()
                self.page = min(self.page, self.pageCount - 1)
                if let id = self.selectedCallID, !report.records.contains(where: { $0.id == id }) {
                    self.selectedCallID = nil
                }
                self.lastRefreshed = Date()
                self.isLoading = false
            }
        }
    }

    private func sortRecords() {
        if callSort == .newest { sortedRecords = report.records; return }
        let sort = callSort
        sortedRecords = report.records.sorted { lhs, rhs in
            if sort == .oldest { return lhs.startedAt < rhs.startedAt }
            let l: Double?
            let r: Double?
            switch sort {
            case .fastest, .slowest: l = lhs.tps; r = rhs.tps
            case .latency: l = lhs.ttft; r = rhs.ttft
            case .tokens: l = Double(lhs.outputTokens); r = Double(rhs.outputTokens)
            case .newest, .oldest: return false
            }
            let left = l.flatMap { $0.isFinite && $0 >= 0 ? $0 : nil }
            let right = r.flatMap { $0.isFinite && $0 >= 0 ? $0 : nil }
            if let left, let right, left != right { return sort == .slowest ? left < right : left > right }
            if (left == nil) != (right == nil) { return left != nil }
            if lhs.startedAt != rhs.startedAt { return lhs.startedAt > rhs.startedAt }
            return lhs.id.uuidString < rhs.id.uuidString
        }
    }
}
