import AppKit
import SpeedTrackerCore
import SwiftUI

/// Keeps a single reusable dashboard window. Closing it never stops collection.
final class DashboardWindowController: NSWindowController, NSWindowDelegate {
    let dashboard: DashboardStore

    init(history: HistoryStore, metrics: MetricsStore) {
        dashboard = DashboardStore(history: history, metrics: metrics)
        let root = DashboardView(dashboard: dashboard).environmentObject(metrics)
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1180, height: 820),
            styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false
        )
        window.title = "Speed Tracker — Dashboard"
        window.contentMinSize = NSSize(width: 1000, height: 680)
        window.contentViewController = NSHostingController(rootView: root)
        window.isReleasedWhenClosed = false
        window.setFrameAutosaveName("SpeedTrackerDashboard")
        super.init(window: window)
        window.delegate = self
        window.center()
    }

    required init?(coder: NSCoder) { nil }

    func open() {
        dashboard.open()
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
        showWindow(nil)
        window?.deminiaturize(nil)
        window?.makeKeyAndOrderFront(nil)
    }

    func windowWillClose(_ notification: Notification) {
        dashboard.close()
        NSApp.setActivationPolicy(.accessory)
    }
}
