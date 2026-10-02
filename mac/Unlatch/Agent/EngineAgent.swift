import AppKit
import FileProvider
import Foundation
import Network
import os

/// The engine agent: `Unlatch.app/Contents/MacOS/Unlatch --agent`, started on demand by launchd via
/// the MachService and kept alive (review §2(a)2). Owns one `EngineHost` per configured VM and
/// the XPC sessions that talk to them.
final class EngineAgent {
    let config: BundleConfig
    private let store: DomainStore
    /// Serializes records/hosts/errors.
    private let queue = DispatchQueue(label: "unlatch.agent.state")
    private var records: [DomainRecord] = []
    private var hosts: [String: EngineHost] = [:]
    private var errors: [String: String] = [:]
    /// Finder domains the system has registered for us with no record in `domains.json`
    /// (a damaged file, or a removal interrupted by an older version), keyed by identifier.
    /// Shown so they can be removed; no engine runs for them.
    private var orphans: [String: DomainRecord] = [:]
    /// Identifiers whose `NSFileProviderManager.remove` is in flight.
    private var removing: Set<String> = []
    /// Set when `domains.json` was unreadable at start; shown on the orphaned locations.
    private var storeWarning: String?
    private let sessions = NSHashTable<ClientSession>.weakObjects()
    private let sessionsLock = NSLock()
    private var shellEnv: [String: String]?
    private let pathMonitor = NWPathMonitor()
    private var wakeObserver: NSObjectProtocol?
    private let log = Logger(subsystem: "unlatch", category: "agent")

    init(config: BundleConfig) throws {
        self.config = config
        store = try DomainStore(appGroup: config.appGroup)
    }

    func start() {
        // Once per launch (review §2(e)10): PATH and agent sockets from the login shell.
        shellEnv = ShellEnvironment.resolve()
        if shellEnv == nil { log.error("login shell environment unavailable; using launchd's") }
        let loaded = store.load()
        if let copy = loaded.keptCopy {
            storeWarning = "The saved VM list was damaged; a copy is at \(copy.path)."
            log.error("domains.json unreadable; copy kept at \(copy.path, privacy: .public)")
        }
        queue.sync {
            records = loaded.records
            for r in records { startHostLocked(r) }
        }
        findOrphanedDomains()
        // At agent start every domain may be sitting in the system's failure backoff from while
        // we were not running (MQ-005): clear it, and refresh the materialized set.
        for host in queue.sync(execute: { Array(hosts.values) }) {
            host.signalResolved()
            host.syncMaterializedSet()
        }
        pathMonitor.pathUpdateHandler = { [weak self] path in
            if path.status == .satisfied { self?.networkChanged() }
        }
        pathMonitor.start(queue: DispatchQueue(label: "unlatch.agent.path"))
        wakeObserver = NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.didWakeNotification, object: nil, queue: nil
        ) { [weak self] _ in self?.networkChanged() }
    }

    func host(for domain: String) -> EngineHost? {
        queue.sync { hosts[domain] }
    }

    func register(_ session: ClientSession, domain: String) {
        sessionsLock.lock()
        sessions.add(session)
        sessionsLock.unlock()
    }

    func snapshots() -> [DomainSnapshot] {
        let (recs, hs, errs, orphaned, warning) = queue.sync { (records, hosts, errors, orphans, storeWarning) }
        let known = recs.map { r in
            let host = hs[r.id]
            return DomainSnapshot(record: r, status: host?.status(), lastError: errs[r.id] ?? host?.needsUserReason)
        }
        let unknown = orphaned.values.sorted { $0.id < $1.id }.map { r in
            DomainSnapshot(record: r, status: nil, lastError: [
                "This Finder location has no saved VM settings, so nothing serves it. Remove it, then add the VM again.",
                warning,
            ].compactMap { $0 }.joined(separator: " "))
        }
        return known + unknown
    }

    // MARK: management

    func add(_ record: DomainRecord) throws {
        _ = try SSHDestination.parse(record.destination)
        guard !record.id.isEmpty, !record.remoteRoot.isEmpty else {
            throw UnlatchFFIError(code: "InvalidArgument", msg: "id and remote root are required")
        }
        try queue.sync {
            guard !records.contains(where: { $0.id == record.id }), orphans[record.id] == nil else {
                throw UnlatchFFIError(code: "Exists", msg: "a VM with id \(record.id) already exists")
            }
            var r = record
            r.registered = false
            records.append(r)
            try store.save(records)
            startHostLocked(r)
        }
    }

    /// Interactive first connect (askpass), then add the Finder domain.
    func connectInteractive(_ id: String, reply: @escaping (String?) -> Void) {
        DispatchQueue.global(qos: .userInitiated).async { [self] in
            guard case let (record, host)? = queue.sync(execute: { () -> (DomainRecord, EngineHost)? in
                guard let r = records.first(where: { $0.id == id }), let h = hosts[id] else { return nil }
                return (r, h)
            }) else {
                return reply(errorText(for: id) ?? "no VM with id \(id)")
            }
            LocalNetworkNudge.nudgeIfNeeded(destination: record.destination, port: record.port, environment: sshEnvironment(for: record))
            do {
                try host.connectInteractive()
            } catch {
                return reply(String(describing: error))
            }
            queue.sync { errors[id] = nil }
            if record.registered {
                host.signalResolved()
                return reply(nil)
            }
            addFinderDomain(record) { error in
                if let error { return reply("connected, but adding the Finder location failed: \(error)") }
                self.queue.sync {
                    if let i = self.records.firstIndex(where: { $0.id == id }) {
                        self.records[i].registered = true
                        try? self.store.save(self.records)
                    }
                }
                host.signalResolved()
                reply(nil)
            }
        }
    }

    /// Remove a VM: its Finder domain first, then (only once fileproviderd has finished) the
    /// record and the engine's directories. `.preserveDirtyUserData` keeps every file with edits
    /// not yet uploaded (an offline VM, MQ-035) at the location passed to `reply`; a failed
    /// removal keeps the record and restarts the engine, so nothing is half-removed.
    /// `reply(error, preservedPath)`.
    func remove(_ id: String, reply: @escaping (String?, String?) -> Void) {
        enum Target {
            case record(DomainRecord, EngineHost?)
            case orphan(DomainRecord)
        }
        let target: Result<Target, UnlatchFFIError> = queue.sync {
            guard !removing.contains(id) else {
                return .failure(UnlatchFFIError(code: "Busy", msg: "\(id) is already being removed"))
            }
            if let r = records.first(where: { $0.id == id }) {
                removing.insert(id)
                errors[id] = nil
                return .success(.record(r, hosts.removeValue(forKey: id)))
            }
            if let o = orphans[id] {
                removing.insert(id)
                return .success(.orphan(o))
            }
            return .failure(UnlatchFFIError(code: "NotFound", msg: "no VM with id \(id)"))
        }
        let record: DomainRecord
        switch target {
        case let .failure(e):
            return reply(e.msg, nil)
        case let .success(.orphan(o)):
            record = o
        case let .success(.record(r, host)):
            record = r
            // Dispatchers must go before the engine they point into. With the engine stopped,
            // pending uploads stay dirty in fileproviderd, which is what gets preserved.
            sessionsLock.lock()
            let bound = sessions.allObjects.filter { $0.domain == id }
            sessionsLock.unlock()
            bound.forEach { $0.shutdown() }
            host?.stop()
        }
        removeFinderDomain(EngineHost.domain(for: record)) { [self] preserved, error in
            if let error {
                log.error("remove domain \(id, privacy: .public): \(error.localizedDescription, privacy: .public)")
                let restarted: EngineHost? = queue.sync {
                    removing.remove(id)
                    guard let r = records.first(where: { $0.id == id }) else { return nil }
                    startHostLocked(r)
                    return hosts[id]
                }
                restarted?.signalResolved()
                return reply("Could not remove the Finder location for \(record.displayName): \(error.localizedDescription). Nothing was deleted; try again.", nil)
            }
            if let preserved {
                log.info("remove domain \(id, privacy: .public): unsynced files kept at \(preserved.path, privacy: .public)")
            }
            queue.sync {
                removing.remove(id)
                orphans[id] = nil
                errors[id] = nil
                if let i = records.firstIndex(where: { $0.id == id }) {
                    records.remove(at: i)
                    do {
                        try store.save(records)
                    } catch {
                        log.error("save after removing \(id, privacy: .public): \(String(describing: error), privacy: .public)")
                    }
                }
            }
            store.removeDirectories(for: id)
            reply(nil, preserved?.path)
        }
    }

    func confirmPaused(_ id: String, apply: Bool) throws {
        guard let host = host(for: id) else { throw UnlatchFFIError(code: "NotFound", msg: "no VM with id \(id)") }
        try host.confirmPaused(apply: apply)
    }

    func networkChanged() {
        for host in queue.sync(execute: { Array(hosts.values) }) { host.networkChanged() }
    }

    // MARK: private

    private func errorText(for id: String) -> String? {
        queue.sync { errors[id] }
    }

    private func sshEnvironment(for record: DomainRecord) -> [String: String] {
        ShellEnvironment.sshEnvironment(base: ProcessInfo.processInfo.environment, shell: shellEnv, useShellAgent: record.useShellAgent)
    }

    private func startHostLocked(_ record: DomainRecord) {
        do {
            let dirs = try store.directories(for: record.id)
            let identity = record.identityFile.map { ($0 as NSString).expandingTildeInPath }
            var cfg = EngineConfigJSON(
                name: record.id,
                transport: .ssh(destination: record.destination, port: record.port, identity: identity, extraArgs: []),
                remoteRoot: record.remoteRoot,
                stateDir: dirs.state.path,
                clientName: HostEnvironment.computerName())
            cfg.cacheDir = dirs.cache.path
            cfg.tempDir = dirs.temp.path
            cfg.unlatchdUpload = HostEnvironment.bundledUnlatchd()
            cfg.sshEnv = sshEnvironment(for: record)
            cfg.askpass = HostEnvironment.askpassPath
            cfg.exposeExec = record.exposeExec
            let env = cfg.sshEnv
            DispatchQueue.global(qos: .utility).async {
                LocalNetworkNudge.nudgeIfNeeded(destination: record.destination, port: record.port, environment: env)
            }
            hosts[record.id] = try EngineHost(record: record, config: cfg)
            errors[record.id] = nil
        } catch {
            errors[record.id] = String(describing: error)
            log.error("cannot start engine for \(record.id, privacy: .public): \(String(describing: error), privacy: .public)")
        }
    }

    /// A domain that is already gone (never added, or removed earlier) counts as removed: on
    /// error, check the domain list like `addFinderDomain`.
    private func removeFinderDomain(_ domain: NSFileProviderDomain, completion: @escaping (URL?, Error?) -> Void) {
        NSFileProviderManager.remove(domain, mode: .preserveDirtyUserData) { preserved, error in
            guard let error else { return completion(preserved, nil) }
            NSFileProviderManager.getDomainsWithCompletionHandler { domains, listError in
                let gone = listError == nil && !domains.contains(where: { $0.identifier == domain.identifier })
                completion(nil, gone ? nil : error)
            }
        }
    }

    /// Domains fileproviderd has for us that no record names. Not removed automatically (they
    /// may hold unsynced edits): listed, so Remove (which preserves those edits) reaches them.
    private func findOrphanedDomains() {
        NSFileProviderManager.getDomainsWithCompletionHandler { [self] domains, error in
            if let error {
                log.error("list Finder domains: \(error.localizedDescription, privacy: .public)")
                return
            }
            queue.sync {
                for d in domains where !records.contains(where: { $0.id == d.identifier.rawValue }) {
                    let id = d.identifier.rawValue
                    log.error("Finder domain \(id, privacy: .public) has no record")
                    orphans[id] = DomainRecord(id: id, displayName: d.displayName, destination: "", remoteRoot: "", registered: true)
                }
            }
        }
    }

    /// `add(domain)` can report 4099 after it actually landed (MQ-052): on error, check the
    /// domain list before giving up.
    private func addFinderDomain(_ record: DomainRecord, completion: @escaping (Error?) -> Void) {
        let domain = EngineHost.domain(for: record)
        NSFileProviderManager.add(domain) { error in
            guard let error else { return completion(nil) }
            NSFileProviderManager.getDomainsWithCompletionHandler { domains, _ in
                completion(domains.contains(where: { $0.identifier == domain.identifier }) ? nil : error)
            }
        }
    }
}
