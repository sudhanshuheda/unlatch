import Foundation

/// A configured VM, persisted by the agent (`domains.json` in the app-group container) and shown
/// by the UI. `id` is both the `NSFileProviderDomainIdentifier` and the engine name.
public struct DomainRecord: Codable, Equatable, Identifiable {
    public var id: String
    public var displayName: String
    /// ssh-config alias or `user@host`.
    public var destination: String
    public var port: UInt16?
    public var identityFile: String?
    public var remoteRoot: String
    /// Pass the login shell's SSH_AUTH_SOCK (1Password, Secretive…) instead of launchd's.
    public var useShellAgent: Bool
    public var exposeExec: Bool
    /// Set once the first interactive connect succeeded and the Finder domain was added.
    public var registered: Bool

    public init(
        id: String, displayName: String, destination: String, port: UInt16? = nil,
        identityFile: String? = nil, remoteRoot: String, useShellAgent: Bool = false,
        exposeExec: Bool = false, registered: Bool = false
    ) {
        self.id = id
        self.displayName = displayName
        self.destination = destination
        self.port = port
        self.identityFile = identityFile
        self.remoteRoot = remoteRoot
        self.useShellAgent = useShellAgent
        self.exposeExec = exposeExec
        self.registered = registered
    }

    private enum CodingKeys: String, CodingKey {
        case id, displayName, destination, port, identityFile, remoteRoot, useShellAgent, exposeExec, registered
    }

    /// The identity and connection fields are required; the flags default to `false` when
    /// absent, so a record written by an older or newer version (one without a flag) still
    /// loads instead of failing the whole `domains.json`.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        displayName = try c.decode(String.self, forKey: .displayName)
        destination = try c.decode(String.self, forKey: .destination)
        port = try c.decodeIfPresent(UInt16.self, forKey: .port)
        identityFile = try c.decodeIfPresent(String.self, forKey: .identityFile)
        remoteRoot = try c.decode(String.self, forKey: .remoteRoot)
        useShellAgent = try c.decodeIfPresent(Bool.self, forKey: .useShellAgent) ?? false
        exposeExec = try c.decodeIfPresent(Bool.self, forKey: .exposeExec) ?? false
        registered = try c.decodeIfPresent(Bool.self, forKey: .registered) ?? false
    }

    /// A stable, unique identifier: a slug of the display name plus random hex.
    public static func makeIdentifier(displayName: String) -> String {
        let allowed = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-"))
        let slug = displayName.lowercased()
            .unicodeScalars.map { allowed.contains($0) && $0.isASCII ? Character($0) : "-" }
            .reduce(into: "") { acc, c in
                if c == "-" && (acc.isEmpty || acc.hasSuffix("-")) { return }
                acc.append(c)
            }
        let trimmed = String(slug.prefix(32)).trimmingCharacters(in: CharacterSet(charactersIn: "-"))
        let suffix = String(format: "%06x", UInt32.random(in: 0..<0x100_0000))
        return "\(trimmed.isEmpty ? "vm" : trimmed)-\(suffix)"
    }
}

/// Decoding the agent's `domains.json` (`[DomainRecord]`) without losing VMs to one bad entry.
public enum DomainRecordList {
    public struct Decoded: Equatable {
        /// Every readable record, in file order; the first record with a given id wins.
        public var records: [DomainRecord]
        /// The file was not a clean list of records (unparseable, not an array, or some entries
        /// unreadable): keep a copy of it before anything saves over it.
        public var damaged: Bool
    }

    private struct Lossy: Decodable {
        let record: DomainRecord?
        init(from decoder: Decoder) throws { record = try? DomainRecord(from: decoder) }
    }

    public static func decode(_ data: Data) -> Decoded {
        guard let entries = try? JSONDecoder().decode([Lossy].self, from: data) else {
            return Decoded(records: [], damaged: true)
        }
        var seen = Set<String>()
        var records: [DomainRecord] = []
        var damaged = false
        for e in entries {
            guard let r = e.record, seen.insert(r.id).inserted else {
                damaged = true
                continue
            }
            records.append(r)
        }
        return Decoded(records: records, damaged: damaged)
    }
}

/// What the agent reports per domain.
public struct DomainSnapshot: Codable, Equatable, Identifiable {
    public var record: DomainRecord
    public var status: EngineStatus?
    /// Last error from starting or connecting the engine.
    public var lastError: String?

    public var id: String { record.id }

    public init(record: DomainRecord, status: EngineStatus?, lastError: String?) {
        self.record = record
        self.status = status
        self.lastError = lastError
    }
}

/// `user@host[:port]`, `host`, `[v6addr]:port` as typed in the Add VM sheet.
public struct SSHDestination: Equatable {
    public var destination: String
    public var port: UInt16?

    public enum ParseError: Error, Equatable {
        case empty, badPort, optionLike, whitespace
    }

    public static func parse(_ raw: String) throws -> SSHDestination {
        let s = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if s.isEmpty { throw ParseError.empty }
        if s.contains(where: \.isWhitespace) { throw ParseError.whitespace }
        // ssh would read a leading '-' as an option.
        if s.hasPrefix("-") { throw ParseError.optionLike }
        let (userPart, hostPart): (String?, String) = {
            if let at = s.lastIndex(of: "@") { return (String(s[..<at]), String(s[s.index(after: at)...])) }
            return (nil, s)
        }()
        var host = hostPart
        var port: UInt16?
        if host.hasPrefix("[") {
            guard let close = host.firstIndex(of: "]") else { throw ParseError.badPort }
            let inner = String(host[host.index(after: host.startIndex)..<close])
            let rest = host[host.index(after: close)...]
            if rest.hasPrefix(":") {
                guard let p = UInt16(rest.dropFirst()), p > 0 else { throw ParseError.badPort }
                port = p
            } else if !rest.isEmpty {
                throw ParseError.badPort
            }
            host = inner
        } else if host.filter({ $0 == ":" }).count == 1, let colon = host.firstIndex(of: ":") {
            guard let p = UInt16(host[host.index(after: colon)...]), p > 0 else { throw ParseError.badPort }
            port = p
            host = String(host[..<colon])
        }
        if host.isEmpty { throw ParseError.empty }
        let dest = userPart.map { "\($0)@\(host)" } ?? host
        return SSHDestination(destination: dest, port: port)
    }
}
