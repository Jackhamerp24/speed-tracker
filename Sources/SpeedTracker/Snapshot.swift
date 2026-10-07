import AppKit
import SpeedTrackerCore
import SwiftUI

/// Offscreen renders use demo records only; --dashboard adds --section overview|trends|calls,
/// and --target-list opens the Live tab's harness list.
enum Snapshot {
    static func render(to path: String, arguments: [String]) {
        let store = MetricsStore(history: nil, defaults: nil)
        if !arguments.contains("--empty") {
            store.loadDemo(streaming: !arguments.contains("--idle"))
        } else {
            store.setProxyState(.running(port: store.port))
        }
        var tab = PopoverTab.live
        if let index = arguments.firstIndex(of: "--target"), arguments.count > index + 1 {
            store.setTarget(arguments[index + 1])
        }
        if let index = arguments.firstIndex(of: "--tab"), arguments.count > index + 1 {
            tab = PopoverTab(rawValue: arguments[index + 1].capitalized) ?? .live
        }

        let appearance = NSAppearance(named: arguments.contains("--dark") ? .darkAqua : .aqua)
        let root: AnyView
        let dashboardMode = arguments.contains("--dashboard")
        if dashboardMode {
            let dashboard = DashboardStore(history: nil, records: arguments.contains("--empty") ? [] : dashboardDemo())
            if let index = arguments.firstIndex(of: "--section"), arguments.count > index + 1 {
                dashboard.section = DashboardSection(rawValue: arguments[index + 1]) ?? .overview
            }
            if let index = arguments.firstIndex(of: "--harness"), arguments.count > index + 1 {
                dashboard.harnessFilter = arguments[index + 1]
            }
            if let index = arguments.firstIndex(of: "--model"), arguments.count > index + 1 {
                dashboard.modelFilter = arguments[index + 1]
            }
            dashboard.refresh()
            let deadline = Date().addingTimeInterval(10)
            while dashboard.isLoading, Date() < deadline {
                RunLoop.main.run(until: Date().addingTimeInterval(0.01))
            }
            guard !dashboard.isLoading else {
                FileHandle.standardError.write(Data("dashboard query timed out\n".utf8))
                exit(1)
            }
            if arguments.contains("--select-call") { dashboard.selectedCallID = dashboard.report.records.first?.id }
            root = AnyView(DashboardView(dashboard: dashboard).environmentObject(store))
            print("dashboard: \(dashboard.section.rawValue), \(dashboard.report.summary.count) calls, \(dashboard.report.groups.count) provider/harness groups")
        } else {
            root = AnyView(PopoverView(tab: tab, targetListOpen: arguments.contains("--target-list")).environmentObject(store))
        }
        let host = NSHostingView(rootView: root.background(Color(nsColor: .windowBackgroundColor)))
        host.appearance = appearance
        let size = dashboardMode ? NSSize(width: 1180, height: 820) : host.fittingSize
        host.frame = NSRect(origin: .zero, size: size)

        let window = NSWindow(
            contentRect: NSRect(x: -10_000, y: -10_000, width: size.width, height: size.height),
            styleMask: [.borderless], backing: .buffered, defer: false
        )
        window.appearance = appearance
        window.contentView = host
        host.layoutSubtreeIfNeeded()
        // Give SwiftUI a turn of the run loop to finish layout.
        RunLoop.main.run(until: Date().addingTimeInterval(0.3))

        guard let bitmap = host.bitmapImageRepForCachingDisplay(in: host.bounds) else {
            FileHandle.standardError.write(Data("could not create bitmap\n".utf8))
            exit(1)
        }
        host.cacheDisplay(in: host.bounds, to: bitmap)
        guard let png = bitmap.representation(using: .png, properties: [:]) else { exit(1) }
        do {
            try png.write(to: URL(fileURLWithPath: path))
        } catch {
            FileHandle.standardError.write(Data("\(error)\n".utf8))
            exit(1)
        }
        let display = StatusDisplay.make(store: store, showTTFT: arguments.contains("--ttft"))
        print("menu bar: [\(display.symbol)] \(display.title)")
        print("wrote \(path) (\(Int(size.width))x\(Int(size.height)))")
    }

    /// Diverse deterministic measurements for visual inspection, never written to history.
    private static func dashboardDemo() -> [RequestRecord] {
        let now = Date()
        let endpoints = [
            ("Claude Code", "anthropic", "anthropic", "claude-opus-5-5", "log"),
            ("Codex", "openai", "openai", "gpt-6.1-sol", "log"),
            ("OMP", "openrouter", "openrouter.ai", "deepseek-reasoner", "proxy"),
            ("OMP", "anthropic", "api.anthropic.com", "claude-opus-5-5", "proxy"),
            ("Pi", "ollama", "localhost", "qwen-local", "proxy"),
            ("DeepSeek CLI", "deepseek", "api.deepseek.com", "unknown", "network"),
            ("OMP", "gateway", "models.example.net", "gpt-6-astra", "proxy")
        ]
        return (0..<140).map { index in
            let endpoint = endpoints[index % endpoints.count]
            let duration = 5 + Double(index % 13)
            let tokens = 250 + index % 17 * 65
            let ttft = 0.4 + Double(index % 11) * 0.18
            let noTiming = index % 19 == 0
            return RequestRecord(
                id: UUID(), startedAt: now.addingTimeInterval(-Double(index) * 3600),
                harness: endpoint.0, route: endpoint.1, upstreamHost: endpoint.2,
                format: .unknown, model: endpoint.3, streamed: true,
                status: index % 37 == 0 ? 429 : 200, ttft: noTiming ? nil : ttft,
                firstVisible: noTiming ? nil : ttft + 0.4, ttfb: noTiming ? nil : ttft * 0.7,
                generation: noTiming ? nil : duration, total: duration + ttft,
                inputTokens: 12_000 + index * 20, cachedInputTokens: 9_000,
                outputTokens: tokens, reasoningTokens: index % 3 == 0 ? tokens / 3 : nil,
                tokensEstimated: endpoint.4 == "network", tps: Double(tokens) / (duration + (noTiming ? ttft : 0)),
                aborted: index % 23 == 0, source: endpoint.4, sourceKey: "dashboard-demo:\(index)"
            )
        }
    }
}
