import Foundation

/// Byte counters of one TCP connection at one moment.
public struct FlowSample: Equatable, Sendable {
    public var pid: Int32
    public var processName: String
    public var localPort: UInt16
    public var remoteAddress: String
    public var remotePort: UInt16
    public var rx: UInt64
    public var tx: UInt64

    public init(pid: Int32, processName: String, localPort: UInt16, remoteAddress: String, remotePort: UInt16, rx: UInt64, tx: UInt64) {
        self.pid = pid
        self.processName = processName
        self.localPort = localPort
        self.remoteAddress = remoteAddress
        self.remotePort = remotePort
        self.rx = rx
        self.tx = tx
    }
}

/// Reads per-connection byte counters by running `nettop` once.
///
/// The kernel interfaces behind these counters need an Apple-only entitlement,
/// so a system tool has to read them. `netstat` is cheaper but returns nothing
/// when its parent is a locally signed app; `nettop` does not have that limit.
public enum Nettop {
    /// One sample of every TCP connection, or only those of `pids`. Takes 15 to 30 ms.
    public static func sample(pids: [Int32]? = nil) -> [FlowSample] {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/nettop")
        var arguments = ["-L", "1", "-x", "-n", "-m", "tcp", "-J", "bytes_in,bytes_out"]
        for pid in pids ?? [] { arguments += ["-p", String(pid)] }
        process.arguments = arguments
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        process.standardInput = FileHandle.nullDevice
        do {
            try process.run()
        } catch {
            return []
        }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return parse(String(decoding: data, as: UTF8.self))
    }

    /// Output is CSV: a row per process (`claude.59479,869201,31938006,`) followed by
    /// its connections (`tcp4 192.168.1.5:49768<->160.79.104.10:443,671694,29485456,`).
    public static func parse(_ output: String) -> [FlowSample] {
        var samples: [FlowSample] = []
        var pid: Int32 = 0
        var name = ""
        for line in output.split(separator: "\n") {
            let fields = line.split(separator: ",", omittingEmptySubsequences: false)
            guard let first = fields.first, !first.isEmpty else { continue }
            if let arrow = first.range(of: "<->") {
                guard fields.count >= 3, let rx = UInt64(fields[1]), let tx = UInt64(fields[2]),
                      let space = first.firstIndex(of: " ")
                else { continue }
                let isV6 = first.hasPrefix("tcp6")
                guard let local = endpoint(first[first.index(after: space)..<arrow.lowerBound], isV6: isV6),
                      let remote = endpoint(first[arrow.upperBound...], isV6: isV6)
                else { continue }
                samples.append(FlowSample(
                    pid: pid, processName: name, localPort: local.port,
                    remoteAddress: remote.address, remotePort: remote.port, rx: rx, tx: tx
                ))
            } else if let dot = first.lastIndex(of: "."), let number = Int32(first[first.index(after: dot)...]) {
                pid = number
                name = String(first[..<dot])
            }
        }
        return samples
    }

    /// `192.168.1.5:443` for IPv4, `2606:4700::6810:202f.443` for IPv6.
    private static func endpoint(_ text: Substring, isV6: Bool) -> (address: String, port: UInt16)? {
        guard let separator = text.lastIndex(of: isV6 ? "." : ":"),
              let port = UInt16(text[text.index(after: separator)...])
        else { return nil }
        return (String(text[..<separator]), port)
    }

    public static func isLoopback(_ address: String) -> Bool {
        address.hasPrefix("127.") || address == "::1" || address.hasPrefix("fe80:")
    }
}

// MARK: - Flow tracking

/// One request and its response, as seen from byte counters alone.
public struct FlowCall: Identifiable, Equatable, Sendable {
    public let id: UUID
    public var pid: Int32
    public var processName: String
    public var remoteAddress: String
    /// Nil when the response resumed after a long silence and its request was not seen.
    public var requestStartUptime: TimeInterval?
    public var requestStartWall: Date?
    public var firstByteUptime: TimeInterval?
    public var firstByteWall: Date?
    public var lastRxUptime: TimeInterval
    public var lastRxWall: Date
    /// Response bytes since the request went out.
    public var rxBytes: UInt64 = 0
    public var rxSamples = 0
    /// True once data has arrived steadily, as generated text does.
    public var sustained = false
    /// Gone quiet without ever streaming steadily: either a small reply that is
    /// already complete, or a model reasoning silently before it answers.
    public var dormant = false

    /// Looks like a streamed generation: data arrived steadily and at a real rate.
    /// A quick API call, or a connection an app keeps open and trickles updates over, does not.
    public var isStream: Bool {
        guard let first = firstByteUptime else { return false }
        return sustained && lastRxUptime - first >= 0.5
    }

    public var ttft: Double? {
        guard let start = requestStartUptime, let first = firstByteUptime, first - start >= 0.02 else { return nil }
        return first - start
    }
}

/// Turns successive counter samples into calls: an upload burst is a request,
/// the first reply bytes after it are the first byte, and silence is the end.
public final class FlowTracker {
    public enum Event: Equatable {
        case began(FlowCall)
        case progressed(FlowCall)
        case ended(FlowCall)
    }

    private struct Key: Hashable {
        var pid: Int32
        var localPort: UInt16
        var remoteAddress: String
        var remotePort: UInt16
    }

    private struct Connection {
        var rx: UInt64
        var tx: UInt64
        var call: FlowCall?
        var lastUpload: TimeInterval?
        /// Recent samples and whether each carried data, to tell a steady stream from occasional pings.
        var recent: [(time: TimeInterval, bytes: UInt64)] = []
        var lastCallEnd: TimeInterval?
    }

    private var connections: [Key: Connection] = [:]

    /// Upload this large in one sample is a request; flow-control chatter is far smaller.
    static let requestBytes: UInt64 = 300
    /// Reply bytes needed before it counts as a response.
    static let responseBytes: UInt64 = 300
    /// While a request uploads, the server answers with flow-control frames,
    /// roughly a byte per kilobyte sent. Replies this soon after an upload are those.
    static let uploadGrace: TimeInterval = 0.2
    /// Slowest flow that still counts as generated text. A slow model at 5 tokens a second
    /// sends more than this; background connections that trickle status updates send less.
    static let streamBytesPerSecond: Double = 400
    static let quietAfterStream: TimeInterval = 2.0
    static let dormantAfter: TimeInterval = 3.0
    static let tinyReplyQuiet: TimeInterval = 1.5
    static let giveUpAfter: TimeInterval = 180

    public init() {}

    public var openCalls: [FlowCall] {
        connections.values.compactMap(\.call)
    }

    /// Something is in flight that deserves frequent sampling.
    public var hasActiveCalls: Bool {
        connections.values.contains { $0.call.map { !$0.dormant } ?? false }
    }

    /// A request is out and its first byte has not arrived: the moment that needs the finest timing.
    public var isAwaitingFirstByte: Bool {
        connections.values.contains { $0.call.map { !$0.dormant && $0.firstByteUptime == nil } ?? false }
    }

    public func ingest(_ samples: [FlowSample], uptime now: TimeInterval, wall: Date) -> [Event] {
        var events: [Event] = []
        var present = Set<Key>()

        for sample in samples {
            let key = Key(pid: sample.pid, localPort: sample.localPort, remoteAddress: sample.remoteAddress, remotePort: sample.remotePort)
            present.insert(key)
            guard var connection = connections[key], sample.rx >= connection.rx, sample.tx >= connection.tx else {
                // First sight, or the counters restarted: take a baseline.
                connections[key] = Connection(rx: sample.rx, tx: sample.tx)
                continue
            }
            let sent = sample.tx - connection.tx
            let received = sample.rx - connection.rx
            connection.rx = sample.rx
            connection.tx = sample.tx

            let uploading = sent >= Self.requestBytes
            if uploading {
                if var call = connection.call {
                    if let first = call.firstByteUptime {
                        if call.rxBytes < 8000, now - first < 1.0 {
                            // What looked like a reply was the TLS handshake. The request is only now going out.
                            call.firstByteUptime = nil
                            call.firstByteWall = nil
                            call.rxBytes = 0
                            call.rxSamples = 0
                            call.sustained = false
                            connection.recent.removeAll()
                            connection.call = call
                            events.append(.progressed(call))
                        } else {
                            // A new request on a connection whose last response is over.
                            events.append(.ended(call))
                            connection.recent.removeAll()
                            connection.call = begin(sample, now: now, wall: wall, events: &events)
                        }
                    } else if call.dormant || (call.rxBytes > 0 && now - call.lastRxUptime > 1.0) {
                        // The earlier request got a small reply and is long done.
                        events.append(.ended(call))
                        connection.recent.removeAll()
                        connection.call = begin(sample, now: now, wall: wall, events: &events)
                    }
                    // Otherwise the same upload is still going.
                } else {
                    connection.recent.removeAll()
                    connection.call = begin(sample, now: now, wall: wall, events: &events)
                }
                connection.lastUpload = now
            }

            let isFlowControl = uploading || connection.lastUpload.map { now - $0 < Self.uploadGrace } ?? false
            if received > 0, !isFlowControl {
                if var call = connection.call {
                    call.rxBytes += received
                    call.lastRxUptime = now
                    call.lastRxWall = wall
                    call.dormant = false
                    if call.firstByteUptime == nil, call.rxBytes >= Self.responseBytes {
                        call.firstByteUptime = now
                        call.firstByteWall = wall
                    }
                    if call.firstByteUptime != nil { call.rxSamples += 1 }
                    connection.call = call
                    events.append(.progressed(call))
                } else if received >= Self.responseBytes, let ended = connection.lastCallEnd, now - ended < 120 {
                    // Data with no request before it: a stream that outlasted our patience.
                    var call = FlowCall(
                        id: UUID(), pid: sample.pid, processName: sample.processName, remoteAddress: sample.remoteAddress,
                        requestStartUptime: nil, requestStartWall: nil, firstByteUptime: now, firstByteWall: wall,
                        lastRxUptime: now, lastRxWall: wall
                    )
                    call.rxBytes = received
                    call.rxSamples = 1
                    connection.recent.removeAll()
                    connection.call = call
                    events.append(.began(call))
                }
            }

            if var call = connection.call, call.firstByteUptime != nil, !call.sustained {
                // Steady means data in nearly every sample for most of a second, whatever the sampling rate.
                connection.recent.append((now, isFlowControl ? 0 : received))
                connection.recent.removeAll { now - $0.time > 1.3 }
                let hits = connection.recent.filter { $0.bytes > 0 }.count
                // The oldest sample's bytes arrived before the window it opens.
                let bytes = connection.recent.dropFirst().reduce(0) { $0 + $1.bytes }
                if let oldest = connection.recent.first, now - oldest.time >= 0.6, hits >= 3,
                   Double(hits) >= 0.75 * Double(connection.recent.count),
                   Double(bytes) / (now - oldest.time) >= Self.streamBytesPerSecond {
                    call.sustained = true
                    connection.call = call
                    events.append(.progressed(call))
                }
            }

            if var call = connection.call {
                let quiet = now - call.lastRxUptime
                if isOver(call, quiet: quiet, now: now) {
                    events.append(.ended(call))
                    connection.call = nil
                    connection.lastCallEnd = now
                } else if !call.dormant, shouldSleep(call, quiet: quiet) {
                    call.dormant = true
                    connection.call = call
                    events.append(.progressed(call))
                }
            }
            connections[key] = connection
        }

        // Closed connections end their calls.
        for (key, connection) in connections where !present.contains(key) {
            if let call = connection.call { events.append(.ended(call)) }
            connections[key] = nil
        }
        return events
    }

    private func begin(_ sample: FlowSample, now: TimeInterval, wall: Date, events: inout [Event]) -> FlowCall {
        let call = FlowCall(
            id: UUID(), pid: sample.pid, processName: sample.processName, remoteAddress: sample.remoteAddress,
            requestStartUptime: now, requestStartWall: wall, firstByteUptime: nil, firstByteWall: nil,
            lastRxUptime: now, lastRxWall: wall
        )
        events.append(.began(call))
        return call
    }

    private func isOver(_ call: FlowCall, quiet: TimeInterval, now: TimeInterval) -> Bool {
        // Once text is flowing, a pause this long means the response has ended.
        if call.sustained { return quiet >= Self.quietAfterStream }
        return now - (call.requestStartUptime ?? call.lastRxUptime) > Self.giveUpAfter && quiet > Self.giveUpAfter / 2
    }

    private func shouldSleep(_ call: FlowCall, quiet: TimeInterval) -> Bool {
        if call.firstByteUptime != nil { return quiet >= Self.dormantAfter }
        // A few bytes and then nothing: a small reply, already complete.
        return call.rxBytes > 0 && quiet >= Self.tinyReplyQuiet
    }
}
