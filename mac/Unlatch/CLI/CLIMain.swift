import AppKit
import FileProvider
import Foundation
import ServiceManagement

/// The app's command-line entry, used by `npx unlatch` (npm/unlatch/lib/mac.js) and scripts:
///
/// ```text
/// <App>.app/Contents/MacOS/<Exe> --cli status            [--json]
/// <App>.app/Contents/MacOS/<Exe> --cli add --name <n> --host <user@host[:port]|alias> --root <dir>
///                                     [--port <p>] [--identity <file>] [--use-shell-agent] [--json]
/// <App>.app/Contents/MacOS/<Exe> --cli remove <id|name>  [--json]   (→ {"id","preserved"})
/// <App>.app/Contents/MacOS/<Exe> --cli open <id|name>    [--no-reveal] [--json]
/// <App>.app/Contents/MacOS/<Exe> --cli register-agent | repair-agent | unregister-agent [--json]
/// ```
///
/// With `--json`, stdout carries exactly one JSON object: `{"ok": true, …}` or
/// `{"ok": false, "error": "…", "code": "usage|not_configured|requires_approval|agent_unavailable|not_found|failed"}`.
/// Exit status: 0 ok, 1 failed, 2 usage, 3 the user has to act (approval, agent unreachable).
/// A domain is `{"id","name","host","port","root","identity","state","detail","entries","path","last_error"}`
/// where `state` is a `ConnState` variant name (`Connecting`, `Syncing`, `Live`, `Offline`,
/// `NeedsUser`, `Paused`) or `Unknown`, and `path` is the Finder location once registered.
///
/// It talks to the engine agent over the same XPC management interface as the menu-bar UI
/// (`AgentClient`), so it is only as privileged as the UI. The spec is in docs/INSTALL.md.
enum CLIMain {
    @MainActor
    static func run() -> Never {
        let all = CommandLine.arguments
        let args = Array(all.drop { $0 != "--cli" }.dropFirst())
        let command = CLICommand(args: args)
        // XPC calls cannot be cancelled: a watchdog bounds the whole invocation instead.
        Task { @MainActor in
            try? await Task.sleep(nanoseconds: UInt64(command.deadline * 1_000_000_000))
            command.fail("timed out after \(Int(command.deadline)) s waiting for the \(CLICommand.product) agent", code: "agent_unavailable", exit: 3)
        }
        Task { @MainActor in
            exit(await command.run())
        }
        dispatchMain()
    }
}

@MainActor
final class CLICommand {
    static var product: String { (Bundle.main.object(forInfoDictionaryKey: "CFBundleName") as? String) ?? "Unlatch" }

    struct Failure: Error {
        let message: String
        let code: String
        let exit: Int32
    }

    private let command: String
    private var positional: [String] = []
    private var options: [String: String] = [:]
    private var flags: Set<String> = []
    private var usageError: String?
    let json: Bool

    private static let valueOptions: Set<String> = ["name", "host", "root", "port", "identity"]
    private static let boolFlags: Set<String> = ["json", "no-reveal", "use-shell-agent"]

    init(args: [String]) {
        command = args.first ?? "help"
        var i = 1
        while i < args.count {
            let a = args[i]
            if a.hasPrefix("--") {
                var key = String(a.dropFirst(2))
                var value: String?
                if let eq = key.firstIndex(of: "=") {
                    value = String(key[key.index(after: eq)...])
                    key = String(key[..<eq])
                }
                if Self.valueOptions.contains(key) {
                    if value == nil, i + 1 < args.count {
                        i += 1
                        value = args[i]
                    }
                    if let value { options[key] = value } else { usageError = "--\(key) needs a value" }
                } else if Self.boolFlags.contains(key) {
                    flags.insert(key)
                } else {
                    usageError = "unknown option \(a)"
                }
            } else {
                positional.append(a)
            }
            i += 1
        }
        json = flags.contains("json")
    }

    /// Longer than the agent call it bounds (`AgentClient`: add 600 s, remove 120 s).
    var deadline: TimeInterval {
        switch command {
        case "add": return 610
        case "remove": return 130
        case "repair-agent": return 60
        default: return 30
        }
    }

    func run() async -> Int32 {
        if let usageError { return fail(usageError, code: "usage", exit: 2) }
        do {
            switch command {
            case "status": try await status()
            case "add": try await add()
            case "remove": try await remove()
            case "open": try await open()
            case "register-agent": try register()
            case "repair-agent": try await repairAgent()
            case "unregister-agent": try await unregisterAgent()
            case "help", "--help", "-h":
                print("usage: \(Self.product) --cli status|add|remove|open|register-agent|repair-agent|unregister-agent [--json] (see docs/INSTALL.md)")
            default:
                return fail("unknown command \(command)", code: "usage", exit: 2)
            }
            return 0
        } catch let f as Failure {
            return fail(f.message, code: f.code, exit: f.exit)
        } catch {
            return fail("\(error)", code: "failed", exit: 1)
        }
    }

    // MARK: output

    @discardableResult
    func fail(_ message: String, code: String, exit status: Int32) -> Int32 {
        if json {
            emit(["ok": false, "error": message, "code": code])
        } else {
            FileHandle.standardError.write(Data("\(Self.product): \(message)\n".utf8))
        }
        fflush(stdout)
        Darwin.exit(status)
    }

    private func emit(_ object: [String: Any?]) {
        let clean = object.mapValues { $0 ?? NSNull() }
        if let data = try? JSONSerialization.data(withJSONObject: clean, options: [.prettyPrinted, .sortedKeys]),
           let text = String(data: data, encoding: .utf8) {
            print(text)
        }
    }

    private func say(_ json: [String: Any?], _ human: @autoclosure () -> String) {
        if self.json {
            var o = json
            o["ok"] = true
            emit(o)
        } else {
            print(human())
        }
    }

    // MARK: plumbing

    private func bundleConfig() throws -> BundleConfig {
        do {
            return try BundleConfig.from(bundle: .main)
        } catch {
            throw Failure(message: "this build has no signing configuration (docs/MACOS.md §3): \(error)", code: "not_configured", exit: 1)
        }
    }

    private func service(_ config: BundleConfig) -> SMAppService {
        SMAppService.agent(plistName: config.launchAgentPlistName)
    }

    private static func name(of status: SMAppService.Status) -> String {
        switch status {
        case .enabled: return "enabled"
        case .requiresApproval: return "requires_approval"
        case .notRegistered: return "not_registered"
        case .notFound: return "not_found"
        @unknown default: return "unknown"
        }
    }

    /// The agent must be registered and allowed before XPC can reach it.
    private func readyClient() throws -> (BundleConfig, AgentClient) {
        let config = try bundleConfig()
        let svc = service(config)
        if svc.status == .notRegistered || svc.status == .notFound {
            try? svc.register()
        }
        switch svc.status {
        case .enabled: return (config, AgentClient(config: config))
        case .requiresApproval:
            throw Failure(message: "allow \(Self.product) in System Settings → General → Login Items & Extensions", code: "requires_approval", exit: 3)
        default:
            throw Failure(message: "the background agent is not registered (\(Self.name(of: svc.status))); open the app once", code: "agent_unavailable", exit: 3)
        }
    }

    private func snapshots(_ client: AgentClient) async throws -> [DomainSnapshot] {
        do {
            return try await client.listDomains()
        } catch {
            throw Failure(message: "\(error)", code: "agent_unavailable", exit: 3)
        }
    }

    private func find(_ key: String?, in snaps: [DomainSnapshot]) throws -> DomainSnapshot {
        guard let key, !key.isEmpty else {
            if snaps.count == 1 { return snaps[0] }
            throw Failure(message: "name a VM (have: \(snaps.map(\.record.displayName).joined(separator: ", ")))", code: "usage", exit: 2)
        }
        if let s = snaps.first(where: { $0.record.id == key || $0.record.displayName == key }) { return s }
        throw Failure(message: "no VM named \(key)", code: "not_found", exit: 1)
    }

    private static func text(_ v: Any??) -> String {
        if case let .some(.some(x)) = v { return "\(x)" }
        return ""
    }

    private static func stateName(_ state: ConnState?) -> (String, String?) {
        guard let state else { return ("Unknown", nil) }
        switch state {
        case .connecting: return ("Connecting", nil)
        case let .syncing(received): return ("Syncing", "\(received) entries received")
        case .live: return ("Live", nil)
        case let .offline(error, retryInMs): return ("Offline", "\(error) (retry in \(retryInMs / 1000) s)")
        case let .needsUser(reason, url): return ("NeedsUser", url.map { "\(reason) \($0)" } ?? reason)
        case let .paused(reason): return ("Paused", reason)
        }
    }

    private func visiblePath(_ record: DomainRecord) async -> String? {
        guard record.registered else { return nil }
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(rawValue: record.id), displayName: record.displayName)
        guard let manager = NSFileProviderManager(for: domain) else { return nil }
        return await withCheckedContinuation { (cont: CheckedContinuation<String?, Never>) in
            manager.getUserVisibleURL(for: .rootContainer) { url, _ in cont.resume(returning: url?.path) }
        }
    }

    private func describe(_ s: DomainSnapshot) async -> [String: Any?] {
        let (state, detail) = Self.stateName(s.status?.state)
        return [
            "id": s.record.id,
            "name": s.record.displayName,
            "host": s.record.destination,
            "port": s.record.port.map { Int($0) },
            "root": s.record.remoteRoot,
            "identity": s.record.identityFile,
            "state": state,
            "detail": detail,
            "entries": s.status.map { Int($0.entries) },
            "path": await visiblePath(s.record),
            "last_error": s.lastError,
        ]
    }

    // MARK: commands

    private func status() async throws {
        let config = try bundleConfig()
        let agent = Self.name(of: service(config).status)
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
        var domains: [[String: Any?]] = []
        if agent == "enabled" {
            for s in try await snapshots(AgentClient(config: config)) { domains.append(await describe(s)) }
        }
        let lines = domains.map { d in "\(Self.text(d["name"]))\t\(Self.text(d["host"])):\(Self.text(d["root"]))\t\(Self.text(d["state"]))" }
        say(["app_version": version, "agent": agent, "domains": domains],
            (["\(Self.product) \(version ?? "") agent=\(agent)"] + lines).joined(separator: "\n"))
    }

    private func add() async throws {
        guard let host = options["host"], let root = options["root"], !root.isEmpty else {
            throw Failure(message: "add needs --host and --root", code: "usage", exit: 2)
        }
        let dest: SSHDestination
        do {
            dest = try SSHDestination.parse(host)
        } catch {
            throw Failure(message: "invalid --host \(host)", code: "usage", exit: 2)
        }
        var port = dest.port
        if let p = options["port"] {
            guard let v = UInt16(p), v > 0 else { throw Failure(message: "invalid --port \(p)", code: "usage", exit: 2) }
            port = v
        }
        let (_, client) = try readyClient()
        let name = options["name"].flatMap { $0.isEmpty ? nil : $0 } ?? dest.destination.split(separator: "@").last.map(String.init) ?? dest.destination
        let record = DomainRecord(
            id: DomainRecord.makeIdentifier(displayName: name),
            displayName: name,
            destination: dest.destination,
            port: port,
            identityFile: options["identity"].map { ($0 as NSString).expandingTildeInPath },
            remoteRoot: root,
            useShellAgent: flags.contains("use-shell-agent"))
        // Same sequence as the Add VM window (AppModel.addAndConnect): a failed first connect
        // removes the VM again so a retry starts clean.
        do {
            try await client.addDomain(record)
        } catch {
            throw Failure(message: "\(error)", code: "failed", exit: 1)
        }
        do {
            try await client.connectInteractive(record.id)
        } catch {
            _ = try? await client.removeDomain(record.id)
            throw Failure(message: "\(error)", code: "failed", exit: 1)
        }
        var d: [String: Any?] = ["id": record.id, "name": name]
        if let snap = try await snapshots(client).first(where: { $0.record.id == record.id }) {
            d = await describe(snap)
        }
        say(["domain": d], "added \(name) (\(record.id))")
    }

    private func remove() async throws {
        let (_, client) = try readyClient()
        let s = try find(positional.first, in: try await snapshots(client))
        let kept: String?
        do {
            kept = try await client.removeDomain(s.record.id)
        } catch {
            throw Failure(message: "\(error)", code: "failed", exit: 1)
        }
        // `preserved`: where files with edits that never reached the VM were moved (or null).
        say(["id": s.record.id, "preserved": kept],
            "removed \(s.record.displayName)" + (kept.map { "\nedits that had not reached the VM were kept in \($0)" } ?? ""))
    }

    private func open() async throws {
        let (_, client) = try readyClient()
        let s = try find(positional.first, in: try await snapshots(client))
        guard let path = await visiblePath(s.record) else {
            throw Failure(message: "\(s.record.displayName) has no Finder location yet (state: \(Self.stateName(s.status?.state).0))", code: "failed", exit: 1)
        }
        if !flags.contains("no-reveal") {
            _ = NSWorkspace.shared.open(URL(fileURLWithPath: path, isDirectory: true))
        }
        say(["id": s.record.id, "path": path], path)
    }

    private func register() throws {
        let config = try bundleConfig()
        let svc = service(config)
        do {
            try svc.register()
        } catch {
            if svc.status != .enabled && svc.status != .requiresApproval {
                throw Failure(message: "register: \(error.localizedDescription)", code: "failed", exit: 1)
            }
        }
        say(["agent": Self.name(of: svc.status)], "agent: \(Self.name(of: svc.status))")
    }

    /// MQ-062/063: a replaced bundle needs unregister → wait for launchd → register.
    private func repairAgent() async throws {
        let config = try bundleConfig()
        let svc = service(config)
        try? await svc.unregister()
        try? await Task.sleep(nanoseconds: 6_000_000_000)
        try register()
    }

    private func unregisterAgent() async throws {
        let config = try bundleConfig()
        let svc = service(config)
        do {
            try await svc.unregister()
        } catch {
            if svc.status != .notRegistered {
                throw Failure(message: "unregister: \(error.localizedDescription)", code: "failed", exit: 1)
            }
        }
        say(["agent": Self.name(of: svc.status)], "agent: \(Self.name(of: svc.status))")
    }
}
