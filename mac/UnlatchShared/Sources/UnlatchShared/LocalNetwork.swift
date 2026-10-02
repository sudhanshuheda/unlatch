import Foundation

/// macOS asks for Local Network permission in the app's name only when *our* process touches the
/// LAN; a spawned /usr/bin/ssh is not reliably attributed (MQ-069, review D21). Before spawning
/// ssh to a LAN host the agent opens and cancels an NWConnection itself. This decides "LAN host".
public enum LocalNetworkClassifier {
    /// RFC 1918, link-local (v4 and v6), IPv6 ULA, and mDNS `.local` names. Loopback, CGNAT
    /// (Tailscale's 100.64/10, routed over a utun) and public addresses are not.
    public static func isLocalNetwork(host: String) -> Bool {
        let h = host.lowercased().trimmingCharacters(in: CharacterSet(charactersIn: "[]"))
        if h.hasSuffix(".local") || h.hasSuffix(".local.") { return true }
        if let v4 = ipv4(h) { return isPrivate(v4: v4) }
        if let v6 = ipv6(h) { return isPrivate(v6: v6) }
        return false
    }

    public static func isPrivate(v4 a: [UInt8]) -> Bool {
        switch (a[0], a[1]) {
        case (10, _): return true
        case (172, 16...31): return true
        case (192, 168): return true
        case (169, 254): return true
        default: return false
        }
    }

    public static func isPrivate(v6 a: [UInt8]) -> Bool {
        if a[0] == 0xfe && (a[1] & 0xc0) == 0x80 { return true } // fe80::/10
        if (a[0] & 0xfe) == 0xfc { return true } // fc00::/7
        // IPv4-mapped ::ffff:a.b.c.d
        if a[0..<10].allSatisfy({ $0 == 0 }) && a[10] == 0xff && a[11] == 0xff {
            return isPrivate(v4: Array(a[12..<16]))
        }
        return false
    }

    public static func ipv4(_ s: String) -> [UInt8]? {
        var addr = in_addr()
        guard inet_pton(AF_INET, s, &addr) == 1 else { return nil }
        return withUnsafeBytes(of: addr) { Array($0) }
    }

    public static func ipv6(_ s: String) -> [UInt8]? {
        let noZone = s.split(separator: "%", maxSplits: 1).first.map(String.init) ?? s
        var addr = in6_addr()
        guard inet_pton(AF_INET6, noZone, &addr) == 1 else { return nil }
        return withUnsafeBytes(of: addr) { Array($0) }
    }

    /// Resolve a DNS name and report whether any address is on the LAN.
    public static func resolvesToLocalNetwork(host: String) -> Bool {
        if isLocalNetwork(host: host) { return true }
        var hints = addrinfo()
        #if canImport(Glibc)
        hints.ai_socktype = Int32(SOCK_STREAM.rawValue) // Linux (only used to test this file)
        #else
        hints.ai_socktype = SOCK_STREAM
        #endif
        var result: UnsafeMutablePointer<addrinfo>?
        guard getaddrinfo(host, nil, &hints, &result) == 0, let first = result else { return false }
        defer { freeaddrinfo(first) }
        var cursor: UnsafeMutablePointer<addrinfo>? = first
        while let ai = cursor {
            if let sa = ai.pointee.ai_addr {
                switch Int32(sa.pointee.sa_family) {
                case AF_INET:
                    let bytes = sa.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { p in
                        withUnsafeBytes(of: p.pointee.sin_addr) { Array($0) }
                    }
                    if isPrivate(v4: bytes) { return true }
                case AF_INET6:
                    let bytes = sa.withMemoryRebound(to: sockaddr_in6.self, capacity: 1) { p in
                        withUnsafeBytes(of: p.pointee.sin6_addr) { Array($0) }
                    }
                    if isPrivate(v6: bytes) { return true }
                default:
                    break
                }
            }
            cursor = ai.pointee.ai_next
        }
        return false
    }
}
