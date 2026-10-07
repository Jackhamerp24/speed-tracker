import Foundation

struct HTTPRequestHead {
    var method: String
    var target: String
    var headers: [(name: String, value: String)]

    func value(_ name: String) -> String? {
        headers.first { $0.name.caseInsensitiveCompare(name) == .orderedSame }?.value
    }

    /// Path without the query string.
    var path: String {
        String(target.prefix { $0 != "?" })
    }
}

enum HTTPParsing {
    struct Malformed: Error {}

    static let headTerminator = Data([13, 10, 13, 10])

    static func head(from data: Data) -> HTTPRequestHead? {
        guard let text = String(data: data, encoding: .utf8) ?? String(data: data, encoding: .isoLatin1) else {
            return nil
        }
        var lines = text.components(separatedBy: "\r\n")
        guard !lines.isEmpty else { return nil }
        let requestLine = lines.removeFirst().split(separator: " ", omittingEmptySubsequences: true)
        guard requestLine.count == 3, requestLine[2].hasPrefix("HTTP/1.") else { return nil }

        var headers: [(name: String, value: String)] = []
        for line in lines where !line.isEmpty {
            guard let colon = line.firstIndex(of: ":") else { return nil }
            let name = line[..<colon].trimmingCharacters(in: .whitespaces)
            let value = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
            guard !name.isEmpty else { return nil }
            headers.append((name, value))
        }
        return HTTPRequestHead(method: String(requestLine[0]), target: String(requestLine[1]), headers: headers)
    }

    /// Decodes a chunked body sitting at the start of `data`. Returns nil while
    /// the body is still incomplete, otherwise the body and the bytes it used.
    static func dechunk(_ data: Data) throws -> (body: Data, consumed: Int)? {
        let bytes = [UInt8](data)
        var position = 0
        var body = Data()
        while true {
            guard let lineEnd = crlf(in: bytes, from: position) else { return nil }
            let sizeText = String(decoding: bytes[position..<lineEnd], as: UTF8.self)
                .prefix { $0 != ";" }
                .trimmingCharacters(in: .whitespaces)
            guard let size = Int(sizeText, radix: 16), size >= 0 else { throw Malformed() }
            position = lineEnd + 2
            if size == 0 {
                // Skip trailers up to the blank line that ends the body.
                while true {
                    guard let end = crlf(in: bytes, from: position) else { return nil }
                    let isBlank = end == position
                    position = end + 2
                    if isBlank { return (body, position) }
                }
            }
            guard bytes.count >= position + size + 2 else { return nil }
            body.append(contentsOf: bytes[position..<position + size])
            position += size
            guard bytes[position] == 13, bytes[position + 1] == 10 else { throw Malformed() }
            position += 2
        }
    }

    private static func crlf(in bytes: [UInt8], from start: Int) -> Int? {
        guard start < bytes.count else { return nil }
        var index = start
        while index + 1 < bytes.count {
            if bytes[index] == 13, bytes[index + 1] == 10 { return index }
            index += 1
        }
        return nil
    }

    static func reason(for status: Int) -> String {
        switch status {
        case 200: return "OK"
        case 201: return "Created"
        case 202: return "Accepted"
        case 204: return "No Content"
        case 301: return "Moved Permanently"
        case 302: return "Found"
        case 304: return "Not Modified"
        case 307: return "Temporary Redirect"
        case 308: return "Permanent Redirect"
        case 400: return "Bad Request"
        case 401: return "Unauthorized"
        case 403: return "Forbidden"
        case 404: return "Not Found"
        case 405: return "Method Not Allowed"
        case 408: return "Request Timeout"
        case 409: return "Conflict"
        case 413: return "Payload Too Large"
        case 422: return "Unprocessable Entity"
        case 426: return "Upgrade Required"
        case 429: return "Too Many Requests"
        case 431: return "Request Header Fields Too Large"
        case 500: return "Internal Server Error"
        case 501: return "Not Implemented"
        case 502: return "Bad Gateway"
        case 503: return "Service Unavailable"
        case 504: return "Gateway Timeout"
        case 529: return "Overloaded"
        default: return status < 400 ? "OK" : "Error"
        }
    }
}
