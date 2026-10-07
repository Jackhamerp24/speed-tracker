import Foundation
import Darwin

/// Append-only log of completed calls, one JSON object per line.
public final class HistoryStore: @unchecked Sendable {
    private let url: URL
    private let queue = DispatchQueue(label: "speedtracker.history", qos: .utility)
    private let encoder: JSONEncoder
    private let decoder: JSONDecoder

    public init(url: URL = AppPaths.history) {
        self.url = url
        encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
        decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
    }

    public func append(_ record: RequestRecord) {
        queue.async { [self] in
            guard var line = try? encoder.encode(record) else { return }
            line.append(0x0A)
            let manager = FileManager.default
            try? manager.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            if !manager.fileExists(atPath: url.path) {
                manager.createFile(atPath: url.path, contents: nil)
            }
            guard let handle = try? FileHandle(forWritingTo: url) else { return }
            defer { try? handle.close() }
            _ = try? handle.seekToEnd()
            try? handle.write(contentsOf: line)
        }
    }

    /// Source keys of every stored record, so a call found again in a harness log is not stored twice.
    public func loadSourceKeys() -> Set<String> {
        guard let data = try? Data(contentsOf: url, options: .mappedIfSafe) else { return [] }
        let marker = Data("\"sourceKey\":\"".utf8)
        var keys: Set<String> = []
        var searchStart = data.startIndex
        while let found = data.range(of: marker, in: searchStart..<data.endIndex) {
            guard let quote = data[found.upperBound...].firstIndex(of: UInt8(ascii: "\"")) else { break }
            keys.insert(String(decoding: data[found.upperBound..<quote], as: UTF8.self))
            searchStart = quote
        }
        return keys
    }

    /// The most recent records, oldest first. Reads only the tail of the file.
    public func loadRecent(limit: Int) -> [RequestRecord] {
        guard let handle = try? FileHandle(forReadingFrom: url) else { return [] }
        defer { try? handle.close() }
        let size = (try? handle.seekToEnd()) ?? 0
        let window = UInt64(limit) * 700
        let offset = size > window ? size - window : 0
        try? handle.seek(toOffset: offset)
        guard let data = try? handle.readToEnd() else { return [] }
        var lines = data.split(separator: 0x0A)
        if offset > 0, !lines.isEmpty { lines.removeFirst() }
        return lines.suffix(limit).compactMap { try? decoder.decode(RequestRecord.self, from: Data($0)) }
    }
    /// Reads the complete history in file order, after all previously submitted appends.
    /// Malformed, truncated and oversized lines are counted, but do not hide later valid records.
    /// Line buffering is capped at 1 MiB; only matching, deduplicated records are retained.
    public func read(matching query: DashboardQuery) throws -> DashboardHistory {
        try queue.sync {
            if (try? url.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true {
                throw CocoaError(.fileReadUnknown, userInfo: [NSFilePathErrorKey: url.path])
            }
            let handle: FileHandle
            do {
                handle = try FileHandle(forReadingFrom: url)
            } catch {
                let error = error as NSError
                if (error.domain == NSCocoaErrorDomain && (error.code == NSFileReadNoSuchFileError || error.code == NSFileNoSuchFileError))
                    || (error.domain == NSPOSIXErrorDomain && error.code == Int(ENOENT)) {
                    return DashboardHistory(records: [], skippedLines: 0)
                }
                throw error
            }
            var closed = false
            defer { if !closed { try? handle.close() } }
            let decoder = JSONDecoder()
            decoder.dateDecodingStrategy = .iso8601
            let maximumLineBytes = 1_048_576
            let chunkBytes = 65_536
            var line = Data()
            var oversized = false
            var skipped = 0
            var records: [RequestRecord] = []
            var seen: Set<DashboardRecordKey> = []

            func consumeLine() {
                defer {
                    line.removeAll(keepingCapacity: true)
                    oversized = false
                }
                guard !oversized, let record = try? decoder.decode(RequestRecord.self, from: line) else {
                    skipped += 1
                    return
                }
                if seen.insert(DashboardRecordKey(record)).inserted, query.includes(record) {
                    records.append(record)
                }
            }

            while let chunk = try handle.read(upToCount: chunkBytes), !chunk.isEmpty {
                var start = chunk.startIndex
                while start < chunk.endIndex {
                    let newline = chunk[start...].firstIndex(of: 0x0A)
                    let end = newline ?? chunk.endIndex
                    if !oversized {
                        if end - start > maximumLineBytes - line.count {
                            oversized = true
                            line.removeAll(keepingCapacity: true)
                        } else {
                            line.append(contentsOf: chunk[start..<end])
                        }
                    }
                    if let newline {
                        consumeLine()
                        start = chunk.index(after: newline)
                    } else {
                        break
                    }
                }
            }
            // A complete final JSON object without a newline is valid; a partial object is skipped.
            if oversized || !line.isEmpty { consumeLine() }
            try handle.close()
            closed = true
            return DashboardHistory(records: records, skippedLines: skipped)
        }
    }
}
