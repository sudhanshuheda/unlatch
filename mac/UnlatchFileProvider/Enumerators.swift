import FileProvider
import Foundation

/// Page size when the observer does not suggest one. The engine caps it too.
private let defaultPageSize = 500

private func pageLimit(_ suggested: Int?) -> UInt32 {
    UInt32(clamping: min(max(suggested ?? defaultPageSize, 50), 2_000))
}

private func isInitialPage(_ page: NSFileProviderPage) -> Bool {
    page.rawValue == NSFileProviderPage.initialPageSortedByName as Data
        || page.rawValue == NSFileProviderPage.initialPageSortedByDate as Data
}

/// A directory's children. Each folder is enumerated once, ever (MQ-001); later changes arrive
/// through the working set, so `enumerateChanges` only reports the current anchor.
final class ContainerEnumerator: NSObject, NSFileProviderEnumerator {
    private let container: ItemId
    private let engine: EngineConnection
    /// `isFileViewerRequest`: a person is looking, so the engine may prefetch small files (D25).
    private let viewer: Bool
    private let lock = NSLock()
    private var inFlight: UInt64?

    init(container: ItemId, engine: EngineConnection, viewer: Bool) {
        self.container = container
        self.engine = engine
        self.viewer = viewer
    }

    func invalidate() {
        // Cancels any prefetch the listing started (review §2(a)5).
        lock.lock()
        let call = inFlight
        inFlight = nil
        lock.unlock()
        if let call { engine.cancel(call) }
    }

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        let cursor: [UInt8]? = isInitialPage(page) ? nil : [UInt8](page.rawValue)
        let limit = pageLimit(observer.suggestedPageSize as Int?)
        let request = IpcRequest.enumerate(container: container, cursor: cursor, limit: limit, viewer: viewer)
        let call = engine.request(request, site: .enumerate) { [weak self] result in
            self?.finished()
            switch result {
            case let .success(.page(items, next)):
                observer.didEnumerate(items.map(UnlatchItem.init))
                observer.finishEnumerating(upTo: next.map { NSFileProviderPage(Data($0)) })
            case let .success(other):
                observer.finishEnumeratingWithError(EngineConnection.unexpected(other, for: "Enumerate"))
            case let .failure(error):
                observer.finishEnumeratingWithError(error)
            }
        }
        lock.lock()
        inFlight = call
        lock.unlock()
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        engine.request(.currentAnchor, site: .changes) { result in
            switch result {
            case let .success(.anchor(bytes)):
                observer.finishEnumeratingChanges(upTo: NSFileProviderSyncAnchor(Data(bytes)), moreComing: false)
            case let .success(other):
                observer.finishEnumeratingWithError(EngineConnection.unexpected(other, for: "CurrentAnchor"))
            case let .failure(error):
                observer.finishEnumeratingWithError(error)
            }
        }
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        currentAnchor(engine, completionHandler)
    }

    private func finished() {
        lock.lock()
        inFlight = nil
        lock.unlock()
    }
}

/// The working set: a change stream only (MQ-002). Changes are the engine's journal filtered to
/// the materialized set (D5); the engine never answers an empty batch while it is ahead of the
/// anchor (MQ-004).
final class WorkingSetEnumerator: NSObject, NSFileProviderEnumerator {
    private let engine: EngineConnection

    init(engine: EngineConnection) {
        self.engine = engine
    }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        observer.finishEnumerating(upTo: nil)
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        let limit = pageLimit(observer.suggestedBatchSize as Int?)
        engine.request(.changesSince(anchor: [UInt8](anchor.rawValue), limit: limit), site: .changes) { result in
            switch result {
            case let .success(.changes(updated, removed, newAnchor, more)):
                // Updates first: moves out of a directory precede its removal (wire ordering).
                if !updated.isEmpty { observer.didUpdate(updated.map(UnlatchItem.init)) }
                if !removed.isEmpty { observer.didDeleteItems(withIdentifiers: removed.map(ItemIdentifiers.identifier(for:))) }
                observer.finishEnumeratingChanges(upTo: NSFileProviderSyncAnchor(Data(newAnchor)), moreComing: more)
            case let .success(other):
                observer.finishEnumeratingWithError(EngineConnection.unexpected(other, for: "ChangesSince"))
            case let .failure(error):
                observer.finishEnumeratingWithError(error)
            }
        }
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        currentAnchor(engine, completionHandler)
    }
}

private func currentAnchor(_ engine: EngineConnection, _ completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
    engine.request(.currentAnchor, site: .changes) { result in
        if case let .success(.anchor(bytes)) = result {
            completionHandler(NSFileProviderSyncAnchor(Data(bytes)))
        } else {
            completionHandler(nil)
        }
    }
}
