import AppKit
import SpeedTrackerCore

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var store: MetricsStore!
    private var server: ProxyServer!
    private var detector: AutoDetector!
    private var statusItem: StatusItemController!
    private var dashboardWindow: DashboardWindowController?
    private let history = HistoryStore()

    func applicationDidFinishLaunching(_ notification: Notification) {
        let config = ConfigFile.load()
        let store = MetricsStore(history: history)
        store.port = config.port
        store.routes = config.routes
        self.store = store

        let server = ProxyServer(config: config)
        server.onEvent = { [weak store] event in
            DispatchQueue.main.async { store?.apply(event) }
        }
        server.onStateChange = { [weak store] state in
            DispatchQueue.main.async { store?.setProxyState(state) }
        }
        server.start()
        self.server = server

        // Finds calls on its own from harness logs and network traffic; the proxy above is optional.
        let detector = AutoDetector(knownKeys: store.knownSourceKeys)
        let trace = ProcessInfo.processInfo.environment["SPEEDTRACKER_TRACE"] != nil
        detector.onEvent = { [weak store] event in
            if trace { Self.log(event) }
            DispatchQueue.main.async { store?.apply(event) }
        }
        detector.onStatus = { [weak store] statuses in
            DispatchQueue.main.async { store?.setHarnesses(statuses) }
        }
        detector.setTarget(store.target)
        store.onTargetChange = { [weak detector] target in detector?.setTarget(target) }
        detector.start()
        self.detector = detector

        installMainMenu()
        statusItem = StatusItemController(store: store) { [weak self] in self?.openDashboard() }
        if CommandLine.arguments.contains("--dashboard") {
            DispatchQueue.main.async { [weak self] in self?.openDashboard() }
        }
    }

    @objc private func openDashboard() {
        if dashboardWindow == nil { dashboardWindow = DashboardWindowController(history: history, metrics: store) }
        dashboardWindow?.open()
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        openDashboard()
        return true
    }

    private func installMainMenu() {
        let menu = NSMenu()
        let appItem = NSMenuItem()
        let appMenu = NSMenu(title: "Speed Tracker")
        let dashboard = NSMenuItem(title: "Open Dashboard", action: #selector(openDashboard), keyEquivalent: "d")
        dashboard.target = self
        appMenu.addItem(dashboard)
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Quit Speed Tracker", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = appMenu
        menu.addItem(appItem)

        let windowItem = NSMenuItem()
        let windowMenu = NSMenu(title: "Window")
        windowMenu.addItem(withTitle: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        windowMenu.addItem(withTitle: "Minimize", action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        windowItem.submenu = windowMenu
        menu.addItem(windowItem)
        NSApp.mainMenu = menu
        NSApp.windowsMenu = windowMenu
    }

    /// `SPEEDTRACKER_TRACE=1` prints what the detector sees, for checking it against a real session.
    private static func log(_ event: TrackerEvent) {
        let stamp = String(format: "%.2f", Date().timeIntervalSince1970.truncatingRemainder(dividingBy: 1000))
        let line: String
        switch event {
        case .started(let live):
            line = "started  \(live.harness) \(live.model) \(live.phase)"
        case .updated(let live):
            line = "updated  \(live.harness) \(live.phase) ttft=\(live.ttft.map { String(format: "%.2f", $0) } ?? "-") tokens~\(Int(live.estimatedTokens))"
        case .finished(_, let record?):
            line = "record   \(record.harness) \(record.model) ttft=\(record.ttft.map { String(format: "%.2f", $0) } ?? "-") "
                + "tps=\(record.tps.map { String(format: "%.1f", $0) } ?? "-") out=\(record.outputTokens) source=\(record.source ?? "?")"
        case .finished:
            line = "ended"
        }
        FileHandle.standardError.write(Data("\(stamp) \(line)\n".utf8))
    }

    func applicationWillTerminate(_ notification: Notification) {
        server?.stop()
        detector?.stop()
    }
}
