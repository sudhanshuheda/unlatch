import CUnlatch
import Foundation
import os

private final class ReplySink {
    weak var session: ClientSession?
    init(_ session: ClientSession) { self.session = session }
}

private let replyTrampoline: UnlatchReplyCallback = { ctx, bytes, len, fd in
    // No reply carries a descriptor today (the header reserves `fd`); never leak one.
    if fd >= 0 { close(fd) }
    guard let ctx else { return }
    let session = Unmanaged<ReplySink>.fromOpaque(ctx).takeUnretainedValue().session
    guard let bytes else {
        // NULL frame: the engine closed this connection (fault injection, unlatch.h).
        session?.closedByEngine()
        return
    }
    session?.send(Data(bytes: bytes, count: len))
}

/// One XPC connection from a client (the extension or the menu-bar UI). IPC frames go to a
/// libunlatch dispatcher bound to the domain named by the connection's first `Hello`.
final class ClientSession: NSObject, UnlatchEngineXPC {
    private weak var connection: NSXPCConnection?
    private weak var agent: EngineAgent?
    private let queue = DispatchQueue(label: "unlatch.agent.session")
    private var dispatcher: OpaquePointer?
    private var sink: Unmanaged<ReplySink>?
    private var host: EngineHost?
    private var rejection: String?
    private let log = Logger(subsystem: "unlatch", category: "agent")

    private(set) var domain: String?

    init(agent: EngineAgent, connection: NSXPCConnection) {
        self.agent = agent
        self.connection = connection
    }

    deinit {
        // Not `shutdown()`: the last release may happen on `queue` itself. Nothing else can
        // reach this object any more, and a racing reply callback sees a nil weak session.
        if let d = dispatcher { unlatch_dispatcher_free(d) }
        sink?.release()
    }

    /// Free the dispatcher; libunlatch cancels the calls still in flight (an invalidated
    /// connection means "cancel my calls", never "engine gone" — review §2(e)1).
    func shutdown() {
        queue.sync {
            if let d = dispatcher { unlatch_dispatcher_free(d) }
            dispatcher = nil
            sink?.release()
            sink = nil
            host = nil
        }
    }

    fileprivate func closedByEngine() {
        connection?.invalidate()
    }

    fileprivate func send(_ frame: Data) {
        guard let proxy = connection?.remoteObjectProxyWithErrorHandler({ _ in }) as? UnlatchClientXPC else { return }
        proxy.deliver(frame, fileHandle: nil)
    }

    // MARK: UnlatchEngineXPC — IPC

    func submit(_ frame: Data, fileHandle: FileHandle?) {
        // Our own descriptor: libunlatch takes ownership; the FileHandle keeps XPC's copy.
        let fd: Int32 = fileHandle.map { dup($0.fileDescriptor) } ?? -1
        queue.async { [weak self] in
            guard let self else {
                if fd >= 0 { Darwin.close(fd) }
                return
            }
            self.submitLocked(frame, fd: fd)
        }
    }

    private func submitLocked(_ frame: Data, fd: Int32) {
        if dispatcher == nil, !bindLocked(frame) {
            if fd >= 0 { Darwin.close(fd) }
            return
        }
        guard let d = dispatcher else { return }
        let ok = frame.withUnsafeBytes { raw in
            unlatch_dispatcher_submit(d, raw.bindMemory(to: UInt8.self).baseAddress, raw.count, fd)
        }
        if !ok { log.error("rejected malformed IPC frame (\(frame.count) bytes)") }
    }

    /// Bind to the domain named by the first frame (`Hello`). On failure, reply with an error.
    private func bindLocked(_ frame: Data) -> Bool {
        let request: IpcFrame<IpcRequest>
        do {
            request = try UnlatchCodec.decodeRequest(frame)
        } catch {
            log.error("undecodable first frame: \(String(describing: error), privacy: .public)")
            return false
        }
        guard case let .hello(_, name) = request.msg else {
            replyError(call: request.call, code: .protocol, rejection ?? "the first frame on a connection must be Hello")
            return false
        }
        guard let agent, let host = agent.host(for: name) else {
            rejection = "no Unlatch VM with id \(name)"
            replyError(call: request.call, code: .notFound, rejection ?? "")
            return false
        }
        let box = Unmanaged.passRetained(ReplySink(self))
        let created = host.withHandle { h -> OpaquePointer? in
            guard let h else { return nil }
            return unlatch_dispatcher_new(h, replyTrampoline, box.toOpaque())
        }
        guard let created else {
            box.release()
            replyError(call: request.call, code: .offline, "the engine for \(name) is not running")
            return false
        }
        dispatcher = created
        sink = box
        self.host = host
        domain = name
        agent.register(self, domain: name)
        return true
    }

    private func replyError(call: UInt64, code: ErrorCode, _ message: String) {
        let reply = IpcFrame(call: call, msg: IpcResponse.error(code: code, msg: message, current: nil))
        if let frame = try? UnlatchCodec.encode(reply) { send(frame) }
    }

    // MARK: UnlatchEngineXPC — management

    func listDomains(reply: @escaping (Data?, String?) -> Void) {
        guard let agent else { return reply(nil, "agent is shutting down") }
        do {
            reply(try JSONEncoder().encode(agent.snapshots()), nil)
        } catch {
            reply(nil, String(describing: error))
        }
    }

    func addDomain(_ record: Data, reply: @escaping (String?) -> Void) {
        guard let agent else { return reply("agent is shutting down") }
        do {
            try agent.add(try JSONDecoder().decode(DomainRecord.self, from: record))
            reply(nil)
        } catch {
            reply(String(describing: error))
        }
    }

    func removeDomain(_ identifier: String, reply: @escaping (String?) -> Void) {
        guard let agent else { return reply("agent is shutting down") }
        agent.remove(identifier) { error, _ in reply(error) }
    }

    func removeDomainPreservingEdits(_ identifier: String, reply: @escaping (String?, String?) -> Void) {
        guard let agent else { return reply("agent is shutting down", nil) }
        agent.remove(identifier) { reply($0, $1) }
    }

    func connectInteractive(_ identifier: String, reply: @escaping (String?) -> Void) {
        guard let agent else { return reply("agent is shutting down") }
        agent.connectInteractive(identifier) { reply($0) }
    }

    func confirmPaused(_ identifier: String, apply: Bool, reply: @escaping (String?) -> Void) {
        guard let agent else { return reply("agent is shutting down") }
        do {
            try agent.confirmPaused(identifier, apply: apply)
            reply(nil)
        } catch {
            reply(String(describing: error))
        }
    }

    func ping(reply: @escaping (String) -> Void) {
        reply(UnlatchCodec.libraryVersion)
    }
}
