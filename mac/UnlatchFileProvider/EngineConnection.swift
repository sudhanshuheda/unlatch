import Foundation
import os

/// The extension's XPC client to the engine agent. Stateless apart from in-flight calls: the
/// system starts a fresh extension instance per working-set signal and kills idle ones
/// (MQ-003, MQ-073), and the engine — not this process — owns every piece of state.
///
/// Calls are multiplexed by call id over one connection; replies arrive out of order through
/// `deliver`. An invalidated connection fails the calls in flight with `.serverUnreachable` and
/// is recreated on the next call; it never means "disconnect the domain" (MQ-073).
final class EngineConnection: NSObject, UnlatchClientXPC {
    typealias Completion = (Result<IpcResponse, Error>) -> Void
    typealias ProgressHandler = (UInt64, UInt64) -> Void

    private struct Pending {
        let progress: ProgressHandler?
        let completion: Completion
    }

    private let domain: String
    private let config: BundleConfig?
    /// Guards `connection`, `nextCall`, `pending`. Never runs client callbacks.
    private let queue = DispatchQueue(label: "unlatch.extension.engine-connection")
    /// Runs progress and completion callbacks in arrival order, outside `queue`, so a callback may
    /// start another call without deadlocking.
    private let callbacks = DispatchQueue(label: "unlatch.extension.engine-callbacks")
    private var connection: NSXPCConnection?
    private var nextCall: UInt64 = 1
    private var pending: [UInt64: Pending] = [:]
    private let log = Logger(subsystem: "unlatch", category: "extension")

    init(domain: String) {
        self.domain = domain
        var loaded: BundleConfig?
        do {
            loaded = try BundleConfig.from(bundle: .main)
        } catch {
            Logger(subsystem: "unlatch", category: "extension")
                .fault("bundle misconfigured: \(String(describing: error), privacy: .public)")
        }
        config = loaded
        super.init()
    }

    /// Send one request. Returns its call id (for `cancel`). `completion` runs exactly once.
    @discardableResult
    func call(
        _ request: IpcRequest,
        fileHandle: FileHandle? = nil,
        progress: ProgressHandler? = nil,
        completion: @escaping Completion
    ) -> UInt64 {
        let (id, failure): (UInt64, Error?) = queue.sync {
            let id = nextCall
            nextCall += 1
            guard let proxy = proxyLocked() else {
                return (id, ErrorMapping.unreachable("The Unlatch engine agent is not available"))
            }
            let frame: Data
            do {
                frame = try UnlatchCodec.encode(IpcFrame(call: id, msg: request))
            } catch {
                return (id, ErrorMapping.fileProvider(.cannotSynchronize, "cannot encode request: \(error)"))
            }
            pending[id] = Pending(progress: progress, completion: completion)
            proxy.submit(frame, fileHandle: fileHandle)
            return (id, nil)
        }
        if let failure {
            callbacks.async { completion(.failure(failure)) }
        }
        return id
    }

    /// Ask the engine to abort a call; its final reply is `Error(Cancelled)`.
    func cancel(_ call: UInt64) {
        queue.async { [weak self] in
            guard let self, self.pending[call] != nil, let proxy = self.proxyLocked() else { return }
            let id = self.nextCall
            self.nextCall += 1
            if let frame = try? UnlatchCodec.encode(IpcFrame(call: id, msg: IpcRequest.cancel(call: call))) {
                proxy.submit(frame, fileHandle: nil)
            }
        }
    }

    func invalidate() {
        let failed: [Pending] = queue.sync {
            connection?.invalidate()
            connection = nil
            return takeAllLocked()
        }
        deliverFailure(failed, ErrorMapping.fileProvider(.cannotSynchronize, "extension invalidated"))
    }

    // MARK: UnlatchClientXPC

    func deliver(_ frame: Data, fileHandle: FileHandle?) {
        let decoded: IpcFrame<IpcResponse>
        do {
            decoded = try UnlatchCodec.decodeResponse(frame)
        } catch {
            log.error("undecodable reply: \(String(describing: error), privacy: .public)")
            return
        }
        queue.async { [weak self] in
            guard let self else { return }
            if case let .progress(done, total) = decoded.msg {
                if let handler = self.pending[decoded.call]?.progress {
                    self.callbacks.async { handler(done, total) }
                }
                return
            }
            guard let p = self.pending.removeValue(forKey: decoded.call) else { return }
            self.callbacks.async { p.completion(.success(decoded.msg)) }
        }
    }

    // MARK: private (on `queue`)

    private func proxyLocked() -> UnlatchEngineXPC? {
        if connection == nil { connection = makeConnectionLocked() }
        guard let c = connection else { return nil }
        let proxy = c.remoteObjectProxyWithErrorHandler { [weak self, weak c] error in
            self?.connectionFailed(c, error)
        }
        return proxy as? UnlatchEngineXPC
    }

    private func makeConnectionLocked() -> NSXPCConnection? {
        guard let config else { return nil }
        let c = NSXPCConnection(machServiceName: config.machServiceName, options: [])
        c.remoteObjectInterface = XPCInterfaces.engine()
        c.exportedInterface = XPCInterfaces.client()
        c.exportedObject = self
        // Both sides check each other (review §2(e)1).
        c.setCodeSigningRequirement(config.agentRequirement)
        c.invalidationHandler = { [weak self, weak c] in self?.connectionFailed(c, nil) }
        c.interruptionHandler = { [weak self, weak c] in self?.connectionFailed(c, nil) }
        c.resume()
        // The first frame binds this connection to the domain's engine. Its reply (call 0) has
        // no pending entry and is dropped; if the agent rejects it, every later call on this
        // connection gets an error reply.
        let hello = IpcFrame(call: 0, msg: IpcRequest.hello(proto: UnlatchCodec.protoVersion, domain: domain))
        if let frame = try? UnlatchCodec.encode(hello),
           let proxy = c.remoteObjectProxyWithErrorHandler({ [weak self, weak c] e in self?.connectionFailed(c, e) }) as? UnlatchEngineXPC {
            proxy.submit(frame, fileHandle: nil)
        }
        return c
    }

    private func connectionFailed(_ failed: NSXPCConnection?, _ error: Error?) {
        queue.async { [weak self] in
            guard let self else { return }
            // Ignore late notifications from a connection that was already replaced.
            if let failed, let current = self.connection, failed !== current { return }
            self.connection?.invalidationHandler = nil
            self.connection?.interruptionHandler = nil
            self.connection = nil
            let calls = self.takeAllLocked()
            let message = error.map { "engine agent connection lost: \($0.localizedDescription)" } ?? "engine agent connection lost"
            self.deliverFailure(calls, ErrorMapping.unreachable(message))
        }
    }

    private func takeAllLocked() -> [Pending] {
        let calls = Array(pending.values)
        pending.removeAll()
        return calls
    }

    private func deliverFailure(_ calls: [Pending], _ error: Error) {
        guard !calls.isEmpty else { return }
        callbacks.async {
            for p in calls { p.completion(.failure(error)) }
        }
    }
}

extension EngineConnection {
    /// `call` with the common reply handling: `Error` replies become NSErrors for `site`.
    @discardableResult
    func request(
        _ request: IpcRequest,
        site: ErrorSite,
        fileHandle: FileHandle? = nil,
        progress: ProgressHandler? = nil,
        completion: @escaping (Result<IpcResponse, Error>) -> Void
    ) -> UInt64 {
        call(request, fileHandle: fileHandle, progress: progress) { result in
            switch result {
            case let .success(.error(code, msg, current)):
                completion(.failure(ErrorMapping.error(code: code, msg: msg, current: current, site: site)))
            default:
                completion(result)
            }
        }
    }

    /// An unexpected reply variant for a request.
    static func unexpected(_ response: IpcResponse, for what: String) -> NSError {
        ErrorMapping.fileProvider(.cannotSynchronize, "unexpected engine reply \(response.variantName) to \(what)")
    }
}
