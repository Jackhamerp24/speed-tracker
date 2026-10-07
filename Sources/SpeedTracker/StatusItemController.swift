import AppKit
import Combine
import SpeedTrackerCore
import SwiftUI

/// What the menu bar item shows right now.
struct StatusDisplay: Equatable {
    var symbol: String
    var title: String
    var dimmed: Bool

    /// One steady number: the speed. It follows the stream while text flows, stays
    /// where it was while the model waits, thinks or runs a tool, and rests on the
    /// logged figure afterwards. Only the bolt changes: filled while a call is in flight.
    static func make(store: MetricsStore, showTTFT: Bool) -> StatusDisplay {
        let primary = store.primary
        var title = ""
        if let rate = store.displayRate {
            title = "\(primary == nil ? Format.rate(rate) : Format.wholeRate(rate)) t/s"
            if showTTFT, let ttft = store.displayTTFT { title += " · \(Format.durationText(ttft))" }
        } else if let primary {
            // Nothing has ever been measured: the wait is the only thing to show.
            title = String(format: "%.0fs", max(0, store.now - primary.snapshot.startUptime))
        }
        return StatusDisplay(symbol: primary == nil ? "bolt" : "bolt.fill", title: title, dimmed: primary == nil)
    }
}

final class StatusItemController: NSObject {
    private let store: MetricsStore
    private let statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
    private let popover = NSPopover()
    private var subscriptions: Set<AnyCancellable> = []
    private var shown: StatusDisplay?
    private var lastTitleChange: TimeInterval = 0
    private var refreshPending = false

    /// The number may change at most this often, so it reads as a figure and not a flicker.
    private static let minimumTitleInterval: TimeInterval = 0.5
    private static let trace = ProcessInfo.processInfo.environment["SPEEDTRACKER_TRACE"] != nil

    init(store: MetricsStore, openDashboard: @escaping () -> Void) {
        self.store = store
        super.init()

        let host = NSHostingController(rootView: PopoverView(openDashboard: { [weak self] in
            self?.popover.performClose(nil)
            openDashboard()
        }).environmentObject(store))
        host.sizingOptions = [.preferredContentSize]
        popover.contentViewController = host
        popover.behavior = .transient
        popover.animates = false

        if let button = statusItem.button {
            button.target = self
            button.action = #selector(togglePopover)
            button.imagePosition = .imageLeading
            button.setAccessibilityLabel("Speed Tracker")
        }

        // objectWillChange fires before the values land, so read them on the next turn.
        store.objectWillChange
            .receive(on: RunLoop.main)
            .sink { [weak self] in self?.refresh() }
            .store(in: &subscriptions)
        NotificationCenter.default.publisher(for: UserDefaults.didChangeNotification)
            .receive(on: RunLoop.main)
            .sink { [weak self] _ in self?.refresh() }
            .store(in: &subscriptions)
        refresh()
        if CommandLine.arguments.contains("--probe-popover") { probePopover() }
    }

    /// `--probe-popover`: opens the popover, opens the Watching list, and logs where the
    /// window and its content sit at each step. For chasing layout jumps nobody can screenshot.
    private func probePopover() {
        func log(_ label: String) {
            guard let view = popover.contentViewController?.view, let window = view.window else {
                FileHandle.standardError.write(Data("probe \(label): popover has no window\n".utf8))
                return
            }
            let button = statusItem.button?.window?.frame ?? .zero
            let screen = (window.screen ?? NSScreen.main)?.visibleFrame ?? .zero
            let line = String(
                format: "probe %@: window x=%.0f y=%.0f w=%.0f h=%.0f midX=%.0f | button midX=%.0f | screen visible y=%.0f h=%.0f w=%.0f",
                label, window.frame.minX, window.frame.minY, window.frame.width, window.frame.height, window.frame.midX, button.midX,
                screen.minY, screen.height, screen.width
            )
            FileHandle.standardError.write(Data((line + "\n").utf8))
        }
        // Each step waits for the layout to settle, then logs where the window ended up.
        let steps: [(String, () -> Void)] = [
            ("popover shown", { [self] in togglePopover() }),
            ("harness list open", { NotificationCenter.default.post(name: .probeToggleTargetList, object: nil) }),
            ("harness list closed", { NotificationCenter.default.post(name: .probeToggleTargetList, object: nil) }),
            ("Models tab", { NotificationCenter.default.post(name: .probeSwitchTab, object: "Models") }),
            ("Settings tab", { NotificationCenter.default.post(name: .probeSwitchTab, object: "Settings") }),
            ("Live tab", { NotificationCenter.default.post(name: .probeSwitchTab, object: "Live") }),
        ]
        for (index, step) in steps.enumerated() {
            DispatchQueue.main.asyncAfter(deadline: .now() + 2 + Double(index) * 1.0) {
                step.1()
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.7) { log(step.0) }
            }
        }
    }

    private func refresh() {
        let display = StatusDisplay.make(
            store: store, showTTFT: UserDefaults.standard.bool(forKey: Preferences.showTTFTInMenuBar)
        )
        guard display != shown, let button = statusItem.button else { return }

        let now = Clock.now
        let onlyNumberChanged = display.symbol == shown?.symbol && shown?.title.isEmpty == false && !display.title.isEmpty
        if onlyNumberChanged, now - lastTitleChange < Self.minimumTitleInterval {
            // Too soon after the last change. Come back, in case nothing else triggers a refresh.
            if !refreshPending {
                refreshPending = true
                DispatchQueue.main.asyncAfter(deadline: .now() + Self.minimumTitleInterval) { [weak self] in
                    self?.refreshPending = false
                    self?.refresh()
                }
            }
            return
        }
        shown = display
        lastTitleChange = now
        if Self.trace {
            let stamp = String(format: "%.2f", Date().timeIntervalSince1970.truncatingRemainder(dividingBy: 1000))
            FileHandle.standardError.write(Data("\(stamp) menubar  [\(display.symbol)] \(display.title)\n".utf8))
        }

        let image = NSImage(systemSymbolName: display.symbol, accessibilityDescription: "Speed Tracker")
        image?.isTemplate = true
        button.image = image
        // Fixed-width digits keep the item from jittering as the rate changes.
        let font = NSFont.monospacedDigitSystemFont(ofSize: NSFont.systemFontSize(for: .regular) - 1, weight: .medium)
        button.attributedTitle = NSAttributedString(
            string: display.title.isEmpty ? "" : " " + display.title,
            attributes: [.font: font, .baselineOffset: 0.5]
        )
        button.alphaValue = display.dimmed ? 0.75 : 1
    }

    @objc private func togglePopover() {
        guard let button = statusItem.button else { return }
        if popover.isShown {
            popover.performClose(nil)
        } else {
            NSApp.activate(ignoringOtherApps: true)
            popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
            popover.contentViewController?.view.window?.makeKey()
        }
    }
}

extension Notification.Name {
    static let probeToggleTargetList = Notification.Name("SpeedTracker.probeToggleTargetList")
    static let probeSwitchTab = Notification.Name("SpeedTracker.probeSwitchTab")
}
