import FileProvider
import Foundation
import os

/// NSFileProviderReplicatedExtension as a thin shim: every method is one IPC request to the engine
/// agent (review D11). Decisions — conflicts, idempotency, deletion guards, name mapping — are
/// the engine's; this file only translates types and passes answers through unchanged.
final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension {
    private let domain: NSFileProviderDomain
    private let engine: EngineConnection
    private let log = Logger(subsystem: "unlatch", category: "extension")

    init(domain: NSFileProviderDomain) {
        self.domain = domain
        engine = EngineConnection(domain: domain.identifier.rawValue)
        super.init()
    }

    func invalidate() {
        engine.invalidate()
    }

    // MARK: Items and content

    func item(
        for identifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        let progress = Progress()
        if identifier == .trashContainer {
            // Trash is not supported; never `.noSuchItem` here (MQ-009 loop).
            completionHandler(nil, ErrorMapping.featureUnsupported())
            return progress
        }
        guard let id = ItemIdentifiers.itemId(for: identifier) else {
            completionHandler(nil, ErrorMapping.fileProvider(.noSuchItem, "not an Unlatch item: \(identifier.rawValue)"))
            return progress
        }
        engine.call(.item(id: id)) { [engine] result in
            switch result {
            case let .success(.item(item)):
                completionHandler(UnlatchItem(item), nil)
            case let .success(.error(.notFound, msg, _)):
                // `.noSuchItem` makes the system delete the item from disk (MQ-011): only say it
                // when the engine is live, i.e. not offline, syncing or reimporting.
                engine.call(.status) { status in
                    var live = false
                    if case let .success(.status(s)) = status { live = s.state.isLive }
                    completionHandler(nil, ErrorMapping.error(code: .notFound, msg: msg, current: nil, site: .item, engineLive: live))
                }
            case let .success(.error(code, msg, current)):
                completionHandler(nil, ErrorMapping.error(code: code, msg: msg, current: current, site: .item))
            case let .success(other):
                completionHandler(nil, EngineConnection.unexpected(other, for: "Item"))
            case let .failure(error):
                completionHandler(nil, error)
            }
        }
        return progress
    }

    func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        guard let id = ItemIdentifiers.itemId(for: itemIdentifier) else {
            completionHandler(nil, nil, ErrorMapping.fileProvider(.noSuchItem, "not an Unlatch item"))
            return progress
        }
        let destination: URL
        do {
            guard let manager = NSFileProviderManager(for: domain) else {
                throw ErrorMapping.fileProvider(.cannotSynchronize, "no manager for domain")
            }
            // The engine clones the content here (same volume as its cache), never to a path the
            // extension chose elsewhere (review §2(a)2).
            destination = try manager.temporaryDirectoryURL()
        } catch {
            completionHandler(nil, nil, error)
            return progress
        }
        let version = requestedVersion.flatMap { VersionCoding.decode($0.contentVersion) }
        let call = engine.request(
            .fetch(id: id, version: version, destDir: destination.path),
            site: .fetch,
            progress: { done, total in
                progress.totalUnitCount = Int64(clamping: max(total, 1))
                progress.completedUnitCount = Int64(clamping: done)
            }
        ) { result in
            switch result {
            case let .success(.fetched(path, item)):
                progress.completedUnitCount = progress.totalUnitCount
                completionHandler(URL(fileURLWithPath: path), UnlatchItem(item), nil)
            case let .success(other):
                completionHandler(nil, nil, EngineConnection.unexpected(other, for: "Fetch"))
            case let .failure(error):
                completionHandler(nil, nil, error)
            }
        }
        progress.cancellationHandler = { [engine] in engine.cancel(call) }
        return progress
    }

    // MARK: Mutations

    func createItem(
        basedOn itemTemplate: NSFileProviderItem,
        fields: NSFileProviderItemFields,
        contents url: URL?,
        options: NSFileProviderCreateItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        let name = itemTemplate.filename
        let kind = CreateKindMapping.kind(of: itemTemplate)
        let mayAlreadyExist = options.contains(.mayAlreadyExist)
        // Mac-only names and (in v1) bundles/aliases stay local (D8, MQ-046) — except when the
        // system replays a create for something the VM may already have.
        if !mayAlreadyExist && (MacLocalNames.isMacLocal(name) || kind == .package || kind == .alias) {
            completionHandler(nil, [], false, ErrorMapping.fileProvider(.excludedFromSync, "\(name) stays on this Mac"))
            return progress
        }
        guard let parent = ItemIdentifiers.itemId(for: itemTemplate.parentItemIdentifier) else {
            completionHandler(nil, [], false, ErrorMapping.fileProvider(.cannotSynchronize, "unsupported parent"))
            return progress
        }
        var handle: FileHandle?
        if kind == .file, fields.contains(.contents), let url {
            do {
                handle = try FileHandle(forReadingFrom: url)
            } catch {
                completionHandler(nil, [], false, ErrorMapping.fileProvider(.cannotSynchronize, "cannot read new content: \(error.localizedDescription)"))
                return progress
            }
        }
        let args = CreateArgs(
            templateId: itemTemplate.itemIdentifier.rawValue,
            parent: parent,
            name: name,
            kind: kind,
            hasContent: handle != nil,
            symlinkTarget: kind == .symlink ? (itemTemplate.symlinkTargetPath ?? nil) : nil,
            mtimeNs: fields.contains(.contentModificationDate) ? (itemTemplate.contentModificationDate ?? nil).map(TimeCoding.ns) : nil,
            userExec: fields.contains(.fileSystemFlags) ? itemTemplate.fileSystemFlags?.contains(.userExecutable) : nil,
            changedFields: FieldCoding.ipc(fields),
            local: LocalMeta(item: itemTemplate),
            mayAlreadyExist: mayAlreadyExist,
            deletionConflicted: options.contains(.deletionConflicted))
        let call = engine.request(.create(args), site: .create, fileHandle: handle, progress: uploadProgress(progress)) { result in
            Self.completeMutation(result, what: "Create", progress: progress, completionHandler)
        }
        // XPC has already duplicated the descriptor into the message.
        try? handle?.close()
        progress.cancellationHandler = { [engine] in engine.cancel(call) }
        return progress
    }

    func modifyItem(
        _ item: NSFileProviderItem,
        baseVersion version: NSFileProviderItemVersion,
        changedFields: NSFileProviderItemFields,
        contents newContents: URL?,
        options: NSFileProviderModifyItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        guard let id = ItemIdentifiers.itemId(for: item.itemIdentifier) else {
            completionHandler(nil, [], false, ErrorMapping.fileProvider(.noSuchItem, "not an Unlatch item"))
            return progress
        }
        let base = VersionCoding.base(version)
        if changedFields.contains(.parentItemIdentifier) && item.parentItemIdentifier == .trashContainer {
            // The domain has no trash (supportsSyncingTrash = false); a move there is a delete
            // with the same base check (review §2(c)6).
            engine.request(.delete(id: id, base: base, recursive: true), site: .delete) { result in
                switch result {
                case .success(.deleted), .success(.ok):
                    completionHandler(nil, [], false, nil)
                case let .success(other):
                    completionHandler(nil, [], false, EngineConnection.unexpected(other, for: "Delete"))
                case let .failure(error):
                    completionHandler(nil, [], false, error)
                }
            }
            return progress
        }
        var newParent: ItemId?
        if changedFields.contains(.parentItemIdentifier) {
            guard let p = ItemIdentifiers.itemId(for: item.parentItemIdentifier) else {
                completionHandler(nil, [], false, ErrorMapping.fileProvider(.cannotSynchronize, "unsupported parent"))
                return progress
            }
            newParent = p
        }
        var handle: FileHandle?
        if changedFields.contains(.contents), let newContents {
            do {
                handle = try FileHandle(forReadingFrom: newContents)
            } catch {
                completionHandler(nil, [], false, ErrorMapping.fileProvider(.cannotSynchronize, "cannot read new content: \(error.localizedDescription)"))
                return progress
            }
        }
        let args = ModifyArgs(
            id: id,
            base: base,
            changedFields: FieldCoding.ipc(changedFields),
            newParent: newParent,
            newName: changedFields.contains(.filename) ? item.filename : nil,
            hasContent: handle != nil,
            mtimeNs: changedFields.contains(.contentModificationDate) ? (item.contentModificationDate ?? nil).map(TimeCoding.ns) : nil,
            // chmod arrives as .fileSystemFlags and carries only owner-execute (MQ-047).
            userExec: changedFields.contains(.fileSystemFlags) ? item.fileSystemFlags?.contains(.userExecutable) : nil,
            local: LocalMeta(item: item))
        let call = engine.request(.modify(args), site: .modify, fileHandle: handle, progress: uploadProgress(progress)) { result in
            Self.completeMutation(result, what: "Modify", progress: progress, completionHandler)
        }
        try? handle?.close()
        progress.cancellationHandler = { [engine] in engine.cancel(call) }
        return progress
    }

    func deleteItem(
        identifier: NSFileProviderItemIdentifier,
        baseVersion version: NSFileProviderItemVersion,
        options: NSFileProviderDeleteItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        let progress = Progress()
        // Deleting something we do not know is a success (header contract).
        guard let id = ItemIdentifiers.itemId(for: identifier) else {
            completionHandler(nil)
            return progress
        }
        engine.request(
            .delete(id: id, base: VersionCoding.base(version), recursive: options.contains(.recursive)),
            site: .delete
        ) { result in
            switch result {
            case .success(.deleted), .success(.ok):
                completionHandler(nil)
            case let .success(other):
                completionHandler(EngineConnection.unexpected(other, for: "Delete"))
            case .failure(let error as NSError)
                where error.domain == NSFileProviderError.errorDomain && error.code == NSFileProviderError.Code.noSuchItem.rawValue:
                completionHandler(nil)
            case let .failure(error):
                completionHandler(error)
            }
        }
        return progress
    }

    // MARK: Enumeration

    func enumerator(for containerItemIdentifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest) throws -> NSFileProviderEnumerator {
        switch containerItemIdentifier {
        case .workingSet:
            return WorkingSetEnumerator(engine: engine)
        case .trashContainer:
            // NSFeatureUnsupportedError makes the system give up after two tries (MQ-010);
            // `.noSuchItem` would loop forever (MQ-009).
            throw ErrorMapping.featureUnsupported()
        default:
            guard let id = ItemIdentifiers.itemId(for: containerItemIdentifier) else {
                throw ErrorMapping.fileProvider(.noSuchItem, "not an Unlatch container")
            }
            return ContainerEnumerator(container: id, engine: engine, viewer: request.isFileViewerRequest)
        }
    }

    /// The system changed which items are materialized. The engine reports working-set changes
    /// against that set (D5), so hand it the complete set.
    func materializedItemsDidChange(completionHandler: @escaping () -> Void) {
        guard let manager = NSFileProviderManager(for: domain) else {
            completionHandler()
            return
        }
        MaterializedSetWalker.collect(manager: manager) { [engine, log] result in
            switch result {
            case let .success(ids):
                engine.request(.materializedChanged(added: ids, removed: [], full: true), site: .changes) { reply in
                    if case let .failure(error) = reply {
                        log.error("MaterializedChanged failed: \(error.localizedDescription, privacy: .public)")
                    }
                    completionHandler()
                }
            case let .failure(error):
                log.error("materialized walk failed: \(error.localizedDescription, privacy: .public)")
                completionHandler()
            }
        }
    }

    // MARK: Helpers

    private func uploadProgress(_ progress: Progress) -> EngineConnection.ProgressHandler {
        { done, total in
            progress.totalUnitCount = Int64(clamping: max(total, 1))
            progress.completedUnitCount = Int64(clamping: done)
        }
    }

    /// Create/modify replies pass `still_pending` and `should_fetch_content` through unchanged
    /// (review §2(e)5): the engine decides conflicts, and the system believes the version in the
    /// reply (MQ-013).
    private static func completeMutation(
        _ result: Result<IpcResponse, Error>,
        what: String,
        progress: Progress,
        _ completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) {
        switch result {
        case let .success(.done(item, stillPending, shouldFetchContent, _)):
            progress.completedUnitCount = progress.totalUnitCount
            completionHandler(UnlatchItem(item), FieldCoding.fileProvider(stillPending), shouldFetchContent, nil)
        case let .success(other):
            completionHandler(nil, [], false, EngineConnection.unexpected(other, for: what))
        case let .failure(error):
            completionHandler(nil, [], false, error)
        }
    }
}
