import Foundation
import Network

/// Loopback reverse proxy. Harnesses point their base URL at it, it forwards
/// every request untouched to the real provider, and it times the response
/// stream on the way back. Nothing is stored except the timing record.
public final class ProxyServer: NSObject, URLSessionDataDelegate, @unchecked Sendable {
    public typealias Event = TrackerEvent

    public enum State: Equatable, Sendable {
        case stopped
        case running(port: UInt16)
        case failed(String)
    }

    /// Both callbacks run on the proxy's own queue.
    public var onEvent: ((Event) -> Void)?
    public var onStateChange: ((State) -> Void)?

    public let config: ProxyConfig
    let queue = DispatchQueue(label: "speedtracker.proxy", qos: .userInitiated)

    private var listeners: [NWListener] = []
    private var session: URLSession!
    private var connections: [ObjectIdentifier: ProxyConnection] = [:]
    private var exchanges: [Int: Exchange] = [:]
    private var charsPerToken: [String: Double] = [:]
    private var recent: [RequestRecord] = []

    private static let minimumUpdateInterval: TimeInterval = 0.1
    private static let maxInspectedBody = 32 * 1024 * 1024

    public init(config: ProxyConfig) {
        self.config = config
        super.init()
        let configuration = URLSessionConfiguration.ephemeral
        configuration.urlCache = nil
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.httpCookieStorage = nil
        configuration.httpShouldSetCookies = false
        configuration.urlCredentialStorage = nil
        // Models can go quiet for minutes while they think.
        configuration.timeoutIntervalForRequest = 900
        configuration.httpMaximumConnectionsPerHost = 32
        let delegateQueue = OperationQueue()
        delegateQueue.underlyingQueue = queue
        delegateQueue.maxConcurrentOperationCount = 1
        session = URLSession(configuration: configuration, delegate: self, delegateQueue: delegateQueue)
    }

    // MARK: Listening

    public func start() {
        queue.async { [self] in
            guard listeners.isEmpty else { return }
            guard let port = NWEndpoint.Port(rawValue: config.port) else {
                onStateChange?(.failed("Invalid port \(config.port)"))
                return
            }
            do {
                listeners.append(try makeListener(host: .ipv4(.loopback), port: port, primary: true))
            } catch {
                onStateChange?(.failed(error.localizedDescription))
                return
            }
            // `localhost` may resolve to ::1 first; listening there too avoids a refused connection.
            if let v6 = try? makeListener(host: .ipv6(.loopback), port: port, primary: false) {
                listeners.append(v6)
            }
        }
    }

    public func stop() {
        queue.async { [self] in
            listeners.forEach { $0.cancel() }
            listeners.removeAll()
            connections.values.forEach { $0.close() }
            onStateChange?(.stopped)
        }
    }

    private func makeListener(host: NWEndpoint.Host, port: NWEndpoint.Port, primary: Bool) throws -> NWListener {
        let parameters = NWParameters.tcp
        parameters.allowLocalEndpointReuse = true
        parameters.requiredLocalEndpoint = .hostPort(host: host, port: port)
        let listener = try NWListener(using: parameters)
        listener.newConnectionHandler = { [weak self] connection in
            guard let self else { return }
            let wrapped = ProxyConnection(connection: connection, server: self)
            self.connections[ObjectIdentifier(wrapped)] = wrapped
            wrapped.start()
        }
        if primary {
            listener.stateUpdateHandler = { [weak self] state in
                guard let self else { return }
                switch state {
                case .ready:
                    self.onStateChange?(.running(port: port.rawValue))
                case .failed(let error):
                    self.onStateChange?(.failed(Self.describe(error, port: port.rawValue)))
                case .waiting(let error):
                    self.onStateChange?(.failed(Self.describe(error, port: port.rawValue)))
                default:
                    break
                }
            }
        }
        listener.start(queue: queue)
        return listener
    }

    private static func describe(_ error: NWError, port: UInt16) -> String {
        if case .posix(let code) = error, code == .EADDRINUSE {
            return "Port \(port) is already in use"
        }
        return error.localizedDescription
    }

    func connectionClosed(_ connection: ProxyConnection) {
        connections[ObjectIdentifier(connection)] = nil
    }

    // MARK: Dispatch

    private static let droppedRequestHeaders: Set<String> = [
        "host", "content-length", "connection", "keep-alive", "proxy-connection",
        "transfer-encoding", "accept-encoding", "upgrade", "te", "expect",
    ]

    private static let droppedResponseHeaders: Set<String> = [
        "content-length", "content-encoding", "transfer-encoding", "connection", "keep-alive", "alt-svc",
    ]

    func dispatch(_ head: HTTPRequestHead, body: Data, on connection: ProxyConnection) {
        // Only local tools may use the proxy. A web page that reaches 127.0.0.1
        // announces itself with an Origin header or a rebound Host.
        if let host = head.value("Host"), !Self.isLoopback(hostHeader: host) {
            connection.sendJSON(status: 403, ["error": ["type": "forbidden", "message": "Speed Tracker only serves localhost."]])
            return
        }
        if let origin = head.value("Origin"), !Self.isLoopback(origin: origin) {
            connection.sendJSON(status: 403, ["error": ["type": "forbidden", "message": "Browser origins are not allowed."]])
            return
        }

        let path = head.path
        if path == "/" || path.hasPrefix("/__speedtracker") {
            connection.sendJSON(status: 200, statusDocument())
            return
        }
        if head.value("Upgrade")?.lowercased().contains("websocket") == true {
            connection.sendJSON(status: 501, ["error": [
                "type": "not_supported",
                "message": "Speed Tracker does not proxy WebSockets. Use the HTTP streaming transport.",
            ]])
            return
        }
        guard let resolved = config.resolve(target: head.target) else {
            connection.sendJSON(status: 404, ["error": [
                "type": "unknown_route",
                "message": "No route for \(path). Use /<route>/... or /_/<host>/...",
                "routes": config.routes.keys.sorted(),
            ] as [String: Any]])
            return
        }

        var request = URLRequest(url: resolved.url)
        request.httpMethod = head.method
        for (name, value) in head.headers where !Self.droppedRequestHeaders.contains(name.lowercased()) {
            request.addValue(value, forHTTPHeaderField: name)
        }
        // Uncompressed streams keep event timing honest.
        request.setValue("identity", forHTTPHeaderField: "Accept-Encoding")
        if !body.isEmpty { request.httpBody = body }

        let exchange = Exchange(connection: connection, task: session.dataTask(with: request))
        exchange.route = resolved.route
        exchange.upstreamHost = resolved.url.host ?? resolved.route
        exchange.method = head.method
        exchange.harness = HarnessDetector.detect(tag: resolved.harnessTag, header: head.value)
        inspect(head, body: body, url: resolved.url, into: exchange)

        connection.exchange = exchange
        exchanges[exchange.task.taskIdentifier] = exchange
        exchange.startUptime = Clock.now
        exchange.task.resume()

        if exchange.tracked {
            onEvent?(.started(snapshot(of: exchange)))
        }
    }

    /// Decides whether this call is a generation worth timing, and reads the requested model.
    private func inspect(_ head: HTTPRequestHead, body: Data, url: URL, into exchange: Exchange) {
        guard head.method == "POST" else { return }
        let path = url.path.lowercased()
        if ["count_tokens", "embeddings", "moderations", "/files", "/batches"].contains(where: { path.contains($0) }) { return }

        var bodyLooksGenerative = false
        if head.value("Content-Encoding") == nil, !body.isEmpty, body.count <= Self.maxInspectedBody,
           let object = try? JSONSerialization.jsonObject(with: body) as? [String: Any] {
            exchange.requestModel = object["model"] as? String
            bodyLooksGenerative = ["messages", "input", "prompt", "contents"].contains { object[$0] != nil }
        }
        if exchange.requestModel == nil,
           let range = url.path.range(of: #"models/[^/:?]+"#, options: [.regularExpression, .caseInsensitive]) {
            // Gemini puts the model in the path.
            exchange.requestModel = String(url.path[range].dropFirst("models/".count))
        }
        let pathLooksGenerative = [
            "/messages", "/chat/completions", "/responses", "/completions",
            "generatecontent", "/api/chat", "/api/generate",
        ].contains(where: { path.contains($0) })
        exchange.tracked = pathLooksGenerative || bodyLooksGenerative
    }

    private func statusDocument() -> [String: Any] {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        let records = recent.compactMap { record -> Any? in
            guard let data = try? encoder.encode(record) else { return nil }
            return try? JSONSerialization.jsonObject(with: data)
        }
        return [
            "app": "Speed Tracker",
            "port": Int(config.port),
            "usage": "Set your harness base URL to http://127.0.0.1:\(config.port)/<route> or /_/<host>",
            "routes": config.routes,
            "active": exchanges.values.filter(\.tracked).count,
            "recent": records,
        ]
    }

    private static func isLoopback(hostHeader: String) -> Bool {
        var host = hostHeader.lowercased()
        if host.hasPrefix("[") {
            host = String(host.dropFirst().prefix { $0 != "]" })
        } else if let colon = host.lastIndex(of: ":") {
            host = String(host[..<colon])
        }
        return host == "127.0.0.1" || host == "localhost" || host == "::1"
    }

    private static func isLoopback(origin: String) -> Bool {
        guard let host = URL(string: origin)?.host?.lowercased() else { return false }
        return host == "127.0.0.1" || host == "localhost" || host == "::1"
    }

    // MARK: Upstream response

    public func urlSession(
        _ session: URLSession, dataTask: URLSessionDataTask, didReceive response: URLResponse,
        completionHandler: @escaping (URLSession.ResponseDisposition) -> Void
    ) {
        defer { completionHandler(.allow) }
        guard let exchange = exchanges[dataTask.taskIdentifier], let http = response as? HTTPURLResponse else { return }
        exchange.status = http.statusCode
        exchange.ttfb = Clock.now - exchange.startUptime

        var headers: [(String, String)] = []
        for (key, value) in http.allHeaderFields {
            guard let name = key as? String, !Self.droppedResponseHeaders.contains(name.lowercased()) else { continue }
            headers.append((name, "\(value)"))
        }
        let hasBody = exchange.method != "HEAD" && http.statusCode != 204 && http.statusCode != 304
        exchange.connection?.sendHead(status: http.statusCode, headers: headers, hasBody: hasBody)

        if exchange.tracked, (200..<300).contains(http.statusCode) {
            let contentType = (http.value(forHTTPHeaderField: "Content-Type") ?? "").lowercased()
            let kind: StreamMeter.BodyKind
            if contentType.contains("event-stream") {
                kind = .eventStream
            } else if contentType.contains("ndjson") || contentType.contains("jsonl") {
                kind = .ndjson
            } else if contentType.contains("json") {
                kind = .json
            } else {
                kind = .other
            }
            let ratio = exchange.requestModel.flatMap { charsPerToken[$0] } ?? 4.0
            exchange.meter = StreamMeter(kind: kind, charsPerToken: ratio)
        }
    }

    public func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        guard let exchange = exchanges[dataTask.taskIdentifier] else { return }
        exchange.connection?.sendChunk(data)
        guard exchange.meter != nil else { return }

        let now = Clock.now
        let hadFirstToken = exchange.meter?.firstTokenAt != nil
        exchange.meter?.ingest(data, at: now)
        let gotFirstToken = !hadFirstToken && exchange.meter?.firstTokenAt != nil
        if gotFirstToken || now - exchange.lastUpdate >= Self.minimumUpdateInterval {
            exchange.lastUpdate = now
            onEvent?(.updated(snapshot(of: exchange)))
        }
    }

    public func urlSession(
        _ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void
    ) {
        // Hand redirects back to the client instead of following them.
        completionHandler(nil)
    }

    public func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        guard let exchange = exchanges.removeValue(forKey: task.taskIdentifier) else { return }
        let connection = exchange.connection
        connection?.exchange = nil

        if let error {
            let cancelled = (error as NSError).code == NSURLErrorCancelled
            finalize(exchange, aborted: true)
            if exchange.status == 0, !cancelled {
                connection?.sendJSON(status: 502, ["error": [
                    "type": "upstream_unreachable",
                    "message": "Speed Tracker could not reach \(exchange.upstreamHost): \(error.localizedDescription)",
                ]], close: true)
            } else {
                // Mid-stream failure: cut the connection so the client sees a broken stream, not a clean end.
                connection?.close()
            }
            return
        }
        finalize(exchange, aborted: false)
        connection?.finishResponse()
    }

    // MARK: Records

    private func snapshot(of exchange: Exchange) -> LiveSnapshot {
        LiveSnapshot(
            id: exchange.id, startedAt: exchange.startedAt, startUptime: exchange.startUptime,
            harness: exchange.harness, route: exchange.route,
            model: exchange.meter?.model ?? exchange.requestModel ?? "unknown",
            firstTokenUptime: exchange.meter?.firstTokenAt,
            firstCharUptime: exchange.meter?.firstCharAt,
            firstVisibleUptime: exchange.meter?.firstVisibleAt,
            lastTokenUptime: exchange.meter?.lastTokenAt,
            estimatedTokens: exchange.meter?.estimatedTokens ?? 0
        )
    }

    private func finalize(_ exchange: Exchange, aborted: Bool) {
        guard exchange.tracked else { return }
        let now = Clock.now
        guard var meter = exchange.meter else {
            onEvent?(.finished(exchange.id, nil))
            return
        }
        meter.finish(at: now)
        let result = meter.result(startedAt: exchange.startUptime, endedAt: now)
        guard result.outputTokens > 0 else {
            onEvent?(.finished(exchange.id, nil))
            return
        }
        let model = meter.model ?? exchange.requestModel ?? "unknown"
        calibrate(model: model, requested: exchange.requestModel, meter: meter)

        let record = RequestRecord(
            id: exchange.id, startedAt: exchange.startedAt, harness: exchange.harness,
            route: exchange.route, upstreamHost: exchange.upstreamHost, format: meter.format,
            model: model, streamed: meter.streamed, status: exchange.status,
            ttft: result.ttft, firstVisible: result.firstVisible, ttfb: exchange.ttfb,
            generation: result.generation, total: now - exchange.startUptime,
            inputTokens: meter.inputTokens, cachedInputTokens: meter.cachedInputTokens,
            outputTokens: result.outputTokens, reasoningTokens: meter.reportedReasoningTokens,
            tokensEstimated: result.tokensEstimated, tps: result.tps, aborted: aborted, source: "proxy"
        )
        recent.append(record)
        if recent.count > 50 { recent.removeFirst(recent.count - 50) }
        onEvent?(.finished(exchange.id, record))
    }

    /// Learns how many characters this model spends per token, so the next
    /// live estimate tracks the provider's own count. Calls with reasoning are
    /// skipped: their token count covers text that was never streamed.
    private func calibrate(model: String, requested: String?, meter: StreamMeter) {
        guard let reported = meter.reportedOutputTokens, reported >= 20, meter.totalChars >= 80,
              !meter.sawReasoningSignal, (meter.reportedReasoningTokens ?? 0) == 0 else { return }
        let observed = min(max(Double(meter.totalChars) / Double(reported), 1.5), 6.0)
        for key in Set([model, requested].compactMap { $0 }) {
            let previous = charsPerToken[key] ?? observed
            charsPerToken[key] = previous * 0.7 + observed * 0.3
        }
    }
}

// MARK: - Exchange

/// One request/response pair in flight.
final class Exchange {
    let id = UUID()
    let startedAt = Date()
    let task: URLSessionDataTask
    weak var connection: ProxyConnection?
    var startUptime: TimeInterval = 0
    var route = ""
    var upstreamHost = ""
    var method = "GET"
    var harness = "Unknown"
    var requestModel: String?
    var tracked = false
    var status = 0
    var ttfb: Double?
    var meter: StreamMeter?
    var lastUpdate: TimeInterval = 0

    init(connection: ProxyConnection, task: URLSessionDataTask) {
        self.connection = connection
        self.task = task
    }
}

// MARK: - Client connection

/// A client socket speaking HTTP/1.1 with keep-alive. Only ever touched on the server queue.
final class ProxyConnection {
    private enum State {
        case head
        case body(HTTPRequestHead)
        case busy
        case closed
    }

    private let connection: NWConnection
    private unowned let server: ProxyServer
    private var buffer = Data()
    private var state: State = .head
    private var keepAlive = true
    private var responseIsChunked = false
    var exchange: Exchange?

    private static let maxHeadBytes = 256 * 1024
    private static let maxBodyBytes = 512 * 1024 * 1024

    init(connection: NWConnection, server: ProxyServer) {
        self.connection = connection
        self.server = server
    }

    func start() {
        connection.stateUpdateHandler = { [weak self] state in
            switch state {
            case .failed, .cancelled: self?.didClose()
            default: break
            }
        }
        connection.start(queue: server.queue)
        receive()
    }

    private func receive() {
        connection.receive(minimumIncompleteLength: 1, maximumLength: 1 << 16) { [weak self] data, _, isComplete, error in
            guard let self else { return }
            if case .closed = self.state { return }
            if let data, !data.isEmpty {
                self.buffer.append(data)
                self.process()
            }
            if isComplete || error != nil {
                self.close()
                return
            }
            self.receive()
        }
    }

    private func process() {
        while true {
            switch state {
            case .head:
                guard let terminator = buffer.range(of: HTTPParsing.headTerminator) else {
                    if buffer.count > Self.maxHeadBytes { reject(status: 431, "Request headers too large.") }
                    return
                }
                let headData = buffer.subdata(in: 0..<terminator.lowerBound)
                buffer.removeSubrange(0..<terminator.upperBound)
                guard let head = HTTPParsing.head(from: headData) else {
                    reject(status: 400, "Malformed HTTP request.")
                    return
                }
                if let length = head.value("Content-Length").flatMap({ Int($0) }), length > Self.maxBodyBytes {
                    reject(status: 413, "Request body too large.")
                    return
                }
                if head.value("Expect")?.lowercased().contains("100-continue") == true {
                    send(Data("HTTP/1.1 100 Continue\r\n\r\n".utf8))
                }
                state = .body(head)

            case .body(let head):
                let body: Data
                if head.value("Transfer-Encoding")?.lowercased().contains("chunked") == true {
                    do {
                        guard let decoded = try HTTPParsing.dechunk(buffer) else { return }
                        body = decoded.body
                        buffer.removeSubrange(0..<decoded.consumed)
                    } catch {
                        reject(status: 400, "Malformed chunked body.")
                        return
                    }
                } else if let length = head.value("Content-Length").flatMap({ Int($0) }), length > 0 {
                    guard buffer.count >= length else { return }
                    body = buffer.subdata(in: 0..<length)
                    buffer.removeSubrange(0..<length)
                } else {
                    body = Data()
                }
                state = .busy
                keepAlive = head.value("Connection")?.lowercased().contains("close") != true
                server.dispatch(head, body: body, on: self)
                return

            case .busy, .closed:
                return
            }
        }
    }

    // MARK: Writing

    private func send(_ data: Data, then completion: (() -> Void)? = nil) {
        if case .closed = state { return }
        connection.send(content: data, completion: .contentProcessed { [weak self] error in
            if error != nil {
                self?.close()
            } else {
                completion?()
            }
        })
    }

    func sendHead(status: Int, headers: [(String, String)], hasBody: Bool) {
        var text = "HTTP/1.1 \(status) \(HTTPParsing.reason(for: status))\r\n"
        for (name, value) in headers {
            text += "\(name): \(value.replacingOccurrences(of: "\r", with: "").replacingOccurrences(of: "\n", with: " "))\r\n"
        }
        responseIsChunked = hasBody
        if hasBody { text += "Transfer-Encoding: chunked\r\n" }
        text += "Connection: \(keepAlive ? "keep-alive" : "close")\r\n\r\n"
        send(Data(text.utf8))
    }

    func sendChunk(_ data: Data) {
        guard responseIsChunked, !data.isEmpty else { return }
        var framed = Data(String(data.count, radix: 16).utf8)
        framed.append(contentsOf: [13, 10])
        framed.append(data)
        framed.append(contentsOf: [13, 10])
        send(framed)
    }

    func finishResponse() {
        if responseIsChunked {
            send(Data("0\r\n\r\n".utf8)) { [weak self] in self?.advance() }
        } else {
            send(Data()) { [weak self] in self?.advance() }
        }
    }

    func sendJSON(status: Int, _ object: [String: Any], close closeAfter: Bool = false) {
        let body = (try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys, .prettyPrinted])) ?? Data()
        if closeAfter { keepAlive = false }
        var text = "HTTP/1.1 \(status) \(HTTPParsing.reason(for: status))\r\n"
        text += "Content-Type: application/json\r\nContent-Length: \(body.count)\r\n"
        text += "Connection: \(keepAlive ? "keep-alive" : "close")\r\n\r\n"
        var data = Data(text.utf8)
        data.append(body)
        send(data) { [weak self] in self?.advance() }
    }

    private func reject(status: Int, _ message: String) {
        state = .busy
        sendJSON(status: status, ["error": ["type": "bad_request", "message": message]], close: true)
    }

    /// The response is fully written: take the next request on this socket, or hang up.
    private func advance() {
        guard case .busy = state else { return }
        responseIsChunked = false
        guard keepAlive else {
            close()
            return
        }
        state = .head
        process()
    }

    func close() {
        if case .closed = state { return }
        connection.cancel()
        didClose()
    }

    private func didClose() {
        if case .closed = state { return }
        state = .closed
        // The client left; stop paying for tokens nobody will read.
        exchange?.task.cancel()
        server.connectionClosed(self)
    }
}
