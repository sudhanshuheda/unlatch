import Foundation

/// Agent-owned persistence in the app-group container:
/// `Library/Application Support/Unlatch/domains.json` plus per-domain engine directories.
struct DomainStore {
    struct Directories {
        let state: URL
        let cache: URL
        let temp: URL
    }

    let root: URL

    init(appGroup: String) throws {
        guard let container = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup) else {
            throw UnlatchFFIError(code: "CannotSync", msg: "no app-group container for \(appGroup) (signing/entitlements mismatch?)")
        }
        root = container.appendingPathComponent("Library/Application Support/Unlatch", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
    }

    private var file: URL { root.appendingPathComponent("domains.json") }

    struct Loaded {
        var records: [DomainRecord]
        /// Set when `domains.json` could not be read cleanly: a copy of it as it was, kept before
        /// the next `save` overwrites it.
        var keptCopy: URL?
    }

    /// Every readable record. One bad entry (an older/newer schema, a hand edit) no longer drops
    /// every VM, and a damaged file is copied aside first, so the VMs it named are recoverable.
    func load() -> Loaded {
        guard FileManager.default.fileExists(atPath: file.path) else { return Loaded(records: [], keptCopy: nil) }
        let decoded = (try? Data(contentsOf: file)).map(DomainRecordList.decode)
            ?? DomainRecordList.Decoded(records: [], damaged: true)
        guard decoded.damaged else { return Loaded(records: decoded.records, keptCopy: nil) }
        let stamp = ISO8601DateFormatter().string(from: Date()).replacingOccurrences(of: ":", with: "-")
        let copy = root.appendingPathComponent("domains.json.unreadable-\(stamp)")
        do {
            try FileManager.default.copyItem(at: file, to: copy)
            return Loaded(records: decoded.records, keptCopy: copy)
        } catch {
            return Loaded(records: decoded.records, keptCopy: nil)
        }
    }

    func save(_ records: [DomainRecord]) throws {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        try encoder.encode(records).write(to: file, options: [.atomic])
    }

    /// Created on demand. The cache lives in the group container on purpose: it must be on the
    /// same volume as the extension's temporary directory so fetches are clonefile(2) copies.
    func directories(for id: String) throws -> Directories {
        let base = root.appendingPathComponent("domains", isDirectory: true).appendingPathComponent(id, isDirectory: true)
        let dirs = Directories(
            state: base.appendingPathComponent("state", isDirectory: true),
            cache: base.appendingPathComponent("cache", isDirectory: true),
            temp: base.appendingPathComponent("tmp", isDirectory: true))
        for d in [dirs.state, dirs.cache, dirs.temp] {
            try FileManager.default.createDirectory(at: d, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        }
        return dirs
    }

    func removeDirectories(for id: String) {
        let base = root.appendingPathComponent("domains", isDirectory: true).appendingPathComponent(id, isDirectory: true)
        try? FileManager.default.removeItem(at: base)
    }
}
