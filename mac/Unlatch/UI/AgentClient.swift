import Foundation

/// The UI's (and the CLI's) XPC client for the agent's management calls. launchd starts the
/// agent on demand when this connects to its MachService.
///
/// Every call has a deadline: a mach service that accepts connections and answers nothing
/// (MQ-063) produces neither a reply nor an error, so without one the caller waits forever and
/// each 2 s refresh leaks another pending reply. The polling calls share one connection, dropped
/// (and so freed) on a timeout; every management call gets its own connection, so a timeout or
/// error on one can never cancel another (e.g. an interactive connect with askpass dialogs).
final class AgentClient {
    struct Failure: Error, CustomStringConvertible {
        let description: String
    }

    private enum Lane {
        /// listDomains / ping: shared connection, short deadline.
        case poll
        /// Everything else: a connection of its own, invalidated when the call finishes.
        case own
    }

    private let config: BundleConfig
    private let lock = NSLock()
    private var pollConnection: NSXPCConnection?

    init(config: BundleConfig) {
        self.config = config
    }

    private func makeConnection(onDrop: ((NSXPCConnection?) -> Void)? = nil) -> NSXPCConnection {
        let c = NSXPCConnection(machServiceName: config.machServiceName, options: [])
        c.remoteObjectInterface = XPCInterfaces.engine()
        c.setCodeSigningRequirement(config.agentRequirement)
        if let onDrop {
            c.invalidationHandler = { [weak c] in onDrop(c) }
            c.interruptionHandler = { [weak c] in onDrop(c) }
        }
        c.resume()
        return c
    }

    private func connection(for lane: Lane) -> NSXPCConnection {
        switch lane {
        case .own:
            return makeConnection()
        case .poll:
            lock.lock()
            defer { lock.unlock() }
            if let c = pollConnection { return c }
            let c = makeConnection { [weak self] c in self?.forgetPoll(c) }
            pollConnection = c
            return c
        }
    }

    private func forgetPoll(_ c: NSXPCConnection?) {
        lock.lock()
        if let c, pollConnection === c { pollConnection = nil }
        lock.unlock()
    }

    /// One reply-or-error XPC call, resumed exactly once: reply, XPC error, or the deadline.
    private func perform<T>(
        _ lane: Lane, timeout: TimeInterval,
        _ body: @escaping (UnlatchEngineXPC, @escaping (Result<T, Error>) -> Void) -> Void
    ) async throws -> T {
        let c = connection(for: lane)
        defer { if lane == .own { c.invalidate() } }
        do {
            return try await CallDeadline.run(seconds: timeout, onTimeout: { [weak self] in
                // Invalidating frees the reply blocks the dead service will never call.
                if lane == .poll { self?.forgetPoll(c) }
                c.invalidate()
            }) { finish in
                let p = c.remoteObjectProxyWithErrorHandler {
                    finish(.failure(Failure(description: "Unlatch agent unavailable: \($0.localizedDescription)")))
                }
                guard let proxy = p as? UnlatchEngineXPC else {
                    return finish(.failure(Failure(description: "Unlatch agent unavailable")))
                }
                body(proxy, finish)
            }
        } catch let e as CallDeadline.TimedOut {
            throw Failure(description: "the Unlatch background agent did not answer (\(e))")
        }
    }

    private static func check(_ error: String?) -> Result<Void, Error> {
        error.map { .failure(Failure(description: $0)) } ?? .success(())
    }

    func listDomains() async throws -> [DomainSnapshot] {
        let data: Data = try await perform(.poll, timeout: 10) { p, done in
            p.listDomains { data, error in
                if let data { done(.success(data)) } else { done(.failure(Failure(description: error ?? "no reply"))) }
            }
        }
        return try JSONDecoder().decode([DomainSnapshot].self, from: data)
    }

    func addDomain(_ record: DomainRecord) async throws {
        let data = try JSONEncoder().encode(record)
        try await perform(.own, timeout: 30) { p, done in p.addDomain(data) { done(Self.check($0)) } }
    }

    /// Blocks in the agent while ssh shows askpass dialogs: the same bound as `--cli add`.
    func connectInteractive(_ id: String) async throws {
        try await perform(.own, timeout: 600) { p, done in p.connectInteractive(id) { done(Self.check($0)) } }
    }

    /// Returns where files with edits not yet uploaded were kept, or nil when there were none.
    @discardableResult
    func removeDomain(_ id: String) async throws -> String? {
        try await perform(.own, timeout: 120) { p, done in
            p.removeDomainPreservingEdits(id) { error, preserved in
                if let error {
                    done(.failure(Failure(description: error)))
                } else {
                    done(.success(preserved))
                }
            }
        }
    }

    func confirmPaused(_ id: String, apply: Bool) async throws {
        try await perform(.own, timeout: 30) { p, done in p.confirmPaused(id, apply: apply) { done(Self.check($0)) } }
    }

    func ping() async throws -> String {
        try await perform(.poll, timeout: 10) { p, done in p.ping { done(.success($0)) } }
    }
}
