import CUnlatch
import FileProvider
import Foundation
import os

/// Holds the weak link from a libunlatch callback context back to its Swift owner, so a callback
/// racing with teardown finds `nil` instead of a deallocating object.
private final class WeakBox<T: AnyObject> {
    weak var value: T?
    init(_ value: T) { self.value = value }
}

private let engineEventTrampoline: UnlatchEventCallback = { ctx, json in
    guard let ctx, let json else { return }
    let text = String(cString: json)
    Unmanaged<WeakBox<EngineHost>>.fromOpaque(ctx).takeUnretainedValue().value?.receive(eventJSON: text)
}

/// One running engine (libunlatch) for one File Provider domain, and the host-side File Provider
/// calls its events drive (review §2(e)8).
final class EngineHost {
    let record: DomainRecord
    private let manager: NSFileProviderManager?
    /// Guards `handle`/`context`/`active`. `stop` waits until no call is using the handle, so a
    /// long `connectInteractive` can never see it freed underneath.
    private let cond = NSCondition()
    private var handle: OpaquePointer?
    private var context: Unmanaged<WeakBox<EngineHost>>?
    private var active = 0
    private let stateLock = NSLock()
    private var needsUser: (reason: String, url: String?)?
    /// Only touched on `events`.
    private let events = DispatchQueue(label: "unlatch.agent.engine-events")
    private var wasLive = false
    private let log = Logger(subsystem: "unlatch", category: "agent")

    init(record: DomainRecord, config: EngineConfigJSON) throws {
        self.record = record
        manager = NSFileProviderManager(for: Self.domain(for: record))
        let json = try config.jsonString()
        let box = Unmanaged.passRetained(WeakBox<EngineHost>(self))
        var err: UnsafeMutablePointer<CChar>?
        guard let h = unlatch_engine_start(json, engineEventTrampoline, box.toOpaque(), &err) else {
            box.release()
            throw UnlatchFFIError.take(err)
        }
        handle = h
        context = box
    }

    deinit {
        stop()
    }

    static func domain(for record: DomainRecord) -> NSFileProviderDomain {
        let d = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(rawValue: record.id), displayName: record.displayName)
        // D7 / MQ-008: the default is YES; Unlatch has no trash (deletes are real deletes on the VM).
        d.supportsSyncingTrash = false
        return d
    }

    /// Stop the engine. After this returns no event callback runs. Idempotent. Must not be
    /// called from inside `withHandle`.
    func stop() {
        cond.lock()
        let h = handle
        let ctx = context
        handle = nil
        context = nil
        while active > 0 { cond.wait() }
        cond.unlock()
        if let h { unlatch_engine_stop(h) }
        ctx?.release()
    }

    /// Run `body` with the live handle (nil once stopped); `stop` waits for it to return.
    func withHandle<T>(_ body: (OpaquePointer?) throws -> T) rethrows -> T {
        cond.lock()
        let h = handle
        if h != nil { active += 1 }
        cond.unlock()
        defer {
            if h != nil {
                cond.lock()
                active -= 1
                cond.broadcast()
                cond.unlock()
            }
        }
        return try body(h)
    }

    // MARK: engine calls

    func status() -> EngineStatus? {
        withHandle { h -> EngineStatus? in
            guard let h, let json = unlatch_engine_status_json(h) else { return nil }
            defer { unlatch_free_string(json) }
            return try? JSONDecoder().decode(EngineStatus.self, from: Data(String(cString: json).utf8))
        }
    }

    var needsUserReason: String? {
        stateLock.lock()
        defer { stateLock.unlock() }
        return needsUser.map { r in r.url.map { "\(r.reason) — \($0)" } ?? r.reason }
    }

    /// Blocking: may show askpass dialogs. Call off the main thread.
    func connectInteractive() throws {
        try withHandle { h in
            guard let h else { throw UnlatchFFIError(code: "Offline", msg: "engine stopped") }
            var err: UnsafeMutablePointer<CChar>?
            guard unlatch_engine_connect_interactive(h, &err) else { throw UnlatchFFIError.take(err) }
        }
        stateLock.lock()
        needsUser = nil
        stateLock.unlock()
    }

    func confirmPaused(apply: Bool) throws {
        try withHandle { h in
            guard let h else { throw UnlatchFFIError(code: "Offline", msg: "engine stopped") }
            var err: UnsafeMutablePointer<CChar>?
            guard unlatch_engine_confirm_paused(h, apply, &err) else { throw UnlatchFFIError.take(err) }
        }
    }

    func networkChanged() {
        withHandle { h in
            if let h { unlatch_engine_network_changed(h) }
        }
    }

    /// One in-process IPC call (no fd).
    func call(_ request: IpcRequest) throws -> IpcResponse {
        let json = try UnlatchCodec.json(IpcFrame(call: 1, msg: request))
        let out: String = try withHandle { h in
            guard let h else { throw UnlatchFFIError(code: "Offline", msg: "engine stopped") }
            var err: UnsafeMutablePointer<CChar>?
            guard let s = unlatch_engine_call_json(h, json, &err) else { throw UnlatchFFIError.take(err) }
            defer { unlatch_free_string(s) }
            return String(cString: s)
        }
        return try JSONDecoder().decode(IpcFrame<IpcResponse>.self, from: Data(out.utf8)).msg
    }

    // MARK: host-side File Provider calls

    /// `signalErrorResolved(.serverUnreachable)` then signal the working set: the only thing that
    /// lifts the system's backoff on failing enumerations (MQ-005) and flushes writes queued
    /// while unreachable (MQ-037).
    func signalResolved() {
        guard let manager else { return }
        manager.signalErrorResolved(NSFileProviderError(.serverUnreachable)) { [log] error in
            if let error { log.debug("signalErrorResolved: \(error.localizedDescription, privacy: .public)") }
            manager.signalEnumerator(for: .workingSet) { error in
                if let error { log.debug("signalEnumerator: \(error.localizedDescription, privacy: .public)") }
            }
        }
    }

    /// Tell the engine the complete materialized set (D5); done at agent start, since the set
    /// may have changed while no engine was running.
    func syncMaterializedSet() {
        guard let manager else { return }
        MaterializedSetWalker.collect(manager: manager) { [weak self] result in
            guard let self else { return }
            switch result {
            case let .success(ids):
                do {
                    _ = try self.call(.materializedChanged(added: ids, removed: [], full: true))
                } catch {
                    self.log.error("MaterializedChanged: \(String(describing: error), privacy: .public)")
                }
            case let .failure(error):
                self.log.error("materialized walk: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    // MARK: events (libunlatch thread → our queue)

    fileprivate func receive(eventJSON: String) {
        guard let event = try? JSONDecoder().decode(EngineEvent.self, from: Data(eventJSON.utf8)) else {
            log.error("undecodable engine event: \(eventJSON, privacy: .public)")
            return
        }
        events.async { [weak self] in self?.apply(event) }
    }

    private func apply(_ event: EngineEvent) {
        switch event {
        case .workingSetChanged:
            manager?.signalEnumerator(for: .workingSet) { _ in }
        case .errorResolved:
            signalResolved()
        case let .reimport(_, below):
            manager?.reimportItems(below: ItemIdentifiers.identifier(for: below)) { [log] error in
                if let error { log.error("reimportItems: \(error.localizedDescription, privacy: .public)") }
            }
        case let .needsUser(_, reason, url):
            stateLock.lock()
            needsUser = (reason, url)
            stateLock.unlock()
        case let .statusChanged(_, status):
            let live = status.state.isLive
            if live && !wasLive { signalResolved() }
            wasLive = live
        }
    }
}
