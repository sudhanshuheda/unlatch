import Foundation

/// Identity values baked into each bundle's Info.plist from `mac/Signing.xcconfig`
/// (never hard-coded: forks change `UNLATCH_BUNDLE_PREFIX`, and the app group must be the
/// team-prefixed `$(TeamIdentifierPrefix)$(UNLATCH_BUNDLE_PREFIX).unlatch`, review D20).
public struct BundleConfig: Equatable {
    public var appGroup: String
    public var teamID: String
    public var bundlePrefix: String

    public init(appGroup: String, teamID: String, bundlePrefix: String) {
        self.appGroup = appGroup
        self.teamID = teamID
        self.bundlePrefix = bundlePrefix
    }

    public enum Key {
        public static let appGroup = "UnlatchAppGroup"
        public static let teamID = "UnlatchTeamID"
        public static let bundlePrefix = "UnlatchBundlePrefix"
        public static let agentPlist = "UnlatchAgentPlist"
    }

    public struct Missing: Error, CustomStringConvertible {
        public let key: String
        public var description: String { "Info.plist key \(key) is missing or empty" }
    }

    public static func from(bundle: Bundle) throws -> BundleConfig {
        try from(infoDictionary: bundle.infoDictionary ?? [:])
    }

    public static func from(infoDictionary info: [String: Any]) throws -> BundleConfig {
        func value(_ key: String) throws -> String {
            guard let v = info[key] as? String, !v.isEmpty, !v.contains("$(") else { throw Missing(key: key) }
            return v
        }
        return BundleConfig(
            appGroup: try value(Key.appGroup),
            teamID: try value(Key.teamID),
            bundlePrefix: try value(Key.bundlePrefix))
    }

    /// The engine agent's MachService. Sandboxed clients may look up Mach services whose names
    /// start with one of their app groups, which is how the extension reaches it.
    public var machServiceName: String { "\(appGroup).engine" }

    public var appBundleID: String { "\(bundlePrefix).unlatch" }
    public var extensionBundleID: String { "\(bundlePrefix).unlatch.fileprovider" }
    public var launchAgentLabel: String { "\(bundlePrefix).unlatch.agent" }
    public var launchAgentPlistName: String { "\(launchAgentLabel).plist" }

    /// Code-signing requirement for XPC peers: signed by Apple-issued certificates of our team,
    /// with one of the allow-listed identifiers (review D11: the agent is a confused deputy
    /// otherwise — any same-user process could drive the VM through its ssh session).
    public func requirement(identifiers: [String]) -> String {
        let ids = identifiers.map { "identifier \"\(Self.quoteSafe($0))\"" }.joined(separator: " or ")
        return "anchor apple generic and certificate leaf[subject.OU] = \"\(Self.quoteSafe(teamID))\" and (\(ids))"
    }

    /// What the agent accepts: the app itself (menu-bar UI) and the File Provider extension.
    public var agentClientRequirement: String { requirement(identifiers: [appBundleID, extensionBundleID]) }

    /// What clients require of the agent (same executable as the app).
    public var agentRequirement: String { requirement(identifiers: [appBundleID]) }

    private static func quoteSafe(_ s: String) -> String {
        String(s.unicodeScalars.filter { CharacterSet.alphanumerics.contains($0) || "._-".unicodeScalars.contains($0) })
    }
}
