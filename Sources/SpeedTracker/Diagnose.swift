import Foundation
import SpeedTrackerCore

/// `SpeedTracker --diagnose [seconds]`: shows what automatic detection sees.
enum Diagnose {
    static func run(seconds: Double) {
        let installed = HarnessPresence.installed()
        print("Installed harnesses: \(installed.isEmpty ? "none found" : installed.sorted().joined(separator: ", "))")
        print("Session logs:")
        for source in SessionLogWatcher.defaultSources() {
            print("  \(source.exists ? "found  " : "missing") \(source.harness)  \(source.root.path)")
        }
        let openCode = OpenCodeLogWatcher()
        let deepSeek = DeepSeekLogWatcher()
        let antigravity = AntigravityLogWatcher()
        let started = Date()
        var openCodeRecords: [LogRecord] = []
        var deepSeekRecords: [LogRecord] = []
        var antigravityRecords: [LogRecord] = []
        // Supplemental formats can be paged; drain bounded backfill without changing their files.
        for _ in 0..<20 {
            openCodeRecords += openCode.poll()
            deepSeekRecords += deepSeek.poll()
            antigravityRecords += antigravity.poll()
        }
        print("  opencode: \(openCode.hasLogs ? "readable" : "unavailable"), \(openCodeRecords.count) recent calls, awaiting=\(openCode.isAwaiting(now: Date()))")
        print("  DeepSeek CLI: \(deepSeek.hasLogs ? "readable" : "unavailable"), \(deepSeekRecords.count) recent calls, awaiting=\(deepSeek.isAwaiting(now: Date()))")
        for limitation in deepSeek.limitations { print("    \(limitation)") }
        print("  Antigravity: \(antigravity.hasLogs ? "readable" : "unavailable"), \(antigravityRecords.count) recent calls, awaiting=\(antigravity.isAwaiting(now: Date()))")
        // Numbers only: what the reader measured, never what was said.
        for record in antigravityRecords.suffix(5) {
            let stored = record.makeRecord()
            print(String(
                format: "    %@  ttft %.2f s  %.1f tok/s  %d out (%d thinking)", stored.model, stored.ttft ?? -1,
                stored.tps ?? -1, stored.outputTokens, stored.reasoningTokens ?? 0
            ))
        }
        print(String(format: "  Supplemental log scan: %.2f s", Date().timeIntervalSince(started)))

        let samples = Nettop.sample()
        print("\nConnections: \(samples.count) established")
        var harnessPids: [Int32: String] = [:]
        for sample in samples where harnessPids[sample.pid] == nil {
            guard let path = ProcessInspector.path(of: sample.pid),
                  let harness = HarnessCatalog.classify(path: path, arguments: ProcessInspector.arguments(of: sample.pid))
            else { continue }
            harnessPids[sample.pid] = harness
        }
        print("Harness processes with connections:")
        for (pid, harness) in harnessPids.sorted(by: { $0.key < $1.key }) {
            let remotes = Set(samples.filter { $0.pid == pid && !Nettop.isLoopback($0.remoteAddress) }.map(\.remoteAddress))
            print("  \(harness) (pid \(pid)) -> \(remotes.sorted().joined(separator: ", "))")
        }
        if harnessPids.isEmpty { print("  none") }

        guard !harnessPids.isEmpty else { return }
        print("\nWatching traffic for \(Int(seconds)) s ...")
        let tracker = FlowTracker()
        let start = Clock.now
        while Clock.now - start < seconds {
            let now = Clock.now
            let relevant = Nettop.sample(pids: harnessPids.keys.sorted())
                .filter { harnessPids[$0.pid] != nil && !Nettop.isLoopback($0.remoteAddress) }
            for event in tracker.ingest(relevant, uptime: now, wall: Date()) {
                let stamp = String(format: "%6.2f", now - start)
                switch event {
                case .began(let call):
                    print("\(stamp)  request   \(harnessPids[call.pid] ?? "?") -> \(call.remoteAddress)")
                case .progressed(let call):
                    if call.rxSamples == 1 {
                        print("\(stamp)  first byte after \(call.ttft.map { String(format: "%.2f s", $0) } ?? "?")")
                    } else if call.dormant {
                        print("\(stamp)  quiet     \(call.rxBytes) bytes so far")
                    }
                case .ended(let call):
                    let length = call.firstByteUptime.map { String(format: "%.2f s", call.lastRxUptime - $0) } ?? "-"
                    print("\(stamp)  ended     \(call.rxBytes) bytes over \(length), stream: \(call.isStream)")
                }
            }
            fflush(stdout)
            Thread.sleep(forTimeInterval: 0.1)
        }
    }
}
