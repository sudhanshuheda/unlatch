import Foundation

/// Host aliases from `~/.ssh/config` for the "Add VM" picker. A best-effort reader: concrete
/// `Host` names (no wildcards or negations), following `Include` (relative to `~/.ssh`, with
/// globs), ignoring `Match` blocks. ssh itself resolves the alias at connect time, so an alias
/// we mis-describe still connects correctly.
public struct SSHConfigHost: Equatable, Identifiable {
    public var alias: String
    public var hostName: String?
    public var user: String?
    public var port: UInt16?
    public var identityFile: String?

    public var id: String { alias }

    public init(alias: String, hostName: String? = nil, user: String? = nil, port: UInt16? = nil, identityFile: String? = nil) {
        self.alias = alias
        self.hostName = hostName
        self.user = user
        self.port = port
        self.identityFile = identityFile
    }
}

public enum SSHConfigParser {
    public static func load(home: URL = FileManager.default.homeDirectoryForCurrentUser) -> [SSHConfigHost] {
        let sshDir = home.appendingPathComponent(".ssh", isDirectory: true)
        var hosts: [SSHConfigHost] = []
        var visited = Set<String>()
        parseFile(sshDir.appendingPathComponent("config"), sshDir: sshDir, home: home, depth: 0, visited: &visited, into: &hosts)
        return dedupe(hosts)
    }

    /// Parse config text. `include` resolves an `Include` argument to file contents.
    public static func parse(_ text: String, include: (String) -> [String] = { _ in [] }, depth: Int = 0) -> [SSHConfigHost] {
        var hosts: [SSHConfigHost] = []
        var current: [Int] = [] // indices into `hosts` of the block being read
        for rawLine in text.components(separatedBy: .newlines) {
            guard let (key, args) = split(rawLine) else { continue }
            switch key {
            case "host":
                let names = args.filter { !$0.contains("*") && !$0.contains("?") && !$0.hasPrefix("!") }
                current = names.map { name in
                    hosts.append(SSHConfigHost(alias: name))
                    return hosts.count - 1
                }
            case "match":
                current = []
            case "include":
                guard depth < 8 else { continue }
                for arg in args {
                    for contents in include(arg) {
                        hosts.append(contentsOf: parse(contents, include: include, depth: depth + 1))
                    }
                }
                // Hosts from an included file do not continue the enclosing block.
            case "hostname":
                for i in current where hosts[i].hostName == nil { hosts[i].hostName = args.first }
            case "user":
                for i in current where hosts[i].user == nil { hosts[i].user = args.first }
            case "port":
                for i in current where hosts[i].port == nil { hosts[i].port = args.first.flatMap { UInt16($0) } }
            case "identityfile":
                for i in current where hosts[i].identityFile == nil { hosts[i].identityFile = args.first }
            default:
                break
            }
        }
        return hosts
    }

    /// `Key value…`, `Key=value`, comments and quotes, keyword lower-cased.
    static func split(_ line: String) -> (String, [String])? {
        var s = line.trimmingCharacters(in: .whitespaces)
        if s.isEmpty || s.hasPrefix("#") { return nil }
        if let eq = s.firstIndex(of: "="), !s[..<eq].contains(where: \.isWhitespace) {
            s = String(s[..<eq]) + " " + String(s[s.index(after: eq)...])
        }
        var words: [String] = []
        var word = ""
        var quoted = false
        var hasWord = false
        for ch in s {
            if ch == "\"" {
                quoted.toggle()
                hasWord = true
            } else if ch.isWhitespace && !quoted {
                if hasWord { words.append(word) }
                word = ""
                hasWord = false
            } else if ch == "#" && !quoted && !hasWord {
                break
            } else {
                word.append(ch)
                hasWord = true
            }
        }
        if hasWord { words.append(word) }
        guard let key = words.first else { return nil }
        return (key.lowercased(), Array(words.dropFirst()))
    }

    private static func parseFile(_ url: URL, sshDir: URL, home: URL, depth: Int, visited: inout Set<String>, into hosts: inout [SSHConfigHost]) {
        let path = url.standardizedFileURL.path
        guard depth < 8, !visited.contains(path), let text = try? String(contentsOf: url, encoding: .utf8) else { return }
        visited.insert(path)
        var files: [URL] = []
        let parsed = parse(text, include: { arg in
            files += expand(arg, sshDir: sshDir, home: home)
            return []
        })
        // Include order matters little for a picker; read includes after the file itself.
        hosts.append(contentsOf: parsed)
        for f in files {
            parseFile(f, sshDir: sshDir, home: home, depth: depth + 1, visited: &visited, into: &hosts)
        }
    }

    static func expand(_ pattern: String, sshDir: URL, home: URL) -> [URL] {
        var p = pattern
        if p.hasPrefix("~/") { p = home.path + String(p.dropFirst(1)) }
        if !p.hasPrefix("/") { p = sshDir.path + "/" + p }
        guard p.contains("*") || p.contains("?") || p.contains("[") else { return [URL(fileURLWithPath: p)] }
        let dir = (p as NSString).deletingLastPathComponent
        let glob = (p as NSString).lastPathComponent
        let names = (try? FileManager.default.contentsOfDirectory(atPath: dir)) ?? []
        return names.filter { fnmatch(glob, $0, 0) == 0 }.sorted().map { URL(fileURLWithPath: dir).appendingPathComponent($0) }
    }

    static func dedupe(_ hosts: [SSHConfigHost]) -> [SSHConfigHost] {
        var seen = Set<String>()
        return hosts.filter { seen.insert($0.alias).inserted }
    }
}
