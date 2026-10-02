import FileProvider
import Foundation
import UniformTypeIdentifiers

// Pure mapping between the engine's IPC types and File Provider types. No logic beyond
// translation lives here (review: the Swift layer is a shim); the engine decides everything.

public enum ItemIdentifiers {
    /// Decimal `ItemId`; `ItemId::ROOT` ↔ `.rootContainer`.
    public static func identifier(for id: ItemId) -> NSFileProviderItemIdentifier {
        id == UnlatchIds.root ? .rootContainer : NSFileProviderItemIdentifier(String(id))
    }

    /// `nil` for the working set, the trash and anything that is not one of ours.
    public static func itemId(for identifier: NSFileProviderItemIdentifier) -> ItemId? {
        if identifier == .rootContainer { return UnlatchIds.root }
        if identifier == .workingSet || identifier == .trashContainer { return nil }
        guard let id = ItemId(identifier.rawValue), id != 0 else { return nil }
        return id
    }
}

public enum VersionCoding {
    /// One component as 8 bytes, big-endian (sorts like the number).
    public static func encode(_ value: UInt64) -> Data {
        withUnsafeBytes(of: value.bigEndian) { Data($0) }
    }

    /// `nil` for `beforeFirstSyncComponent` or anything that is not one of our 8-byte components.
    public static func decode(_ data: Data) -> UInt64? {
        guard data.count == 8, data != NSFileProviderItemVersion.beforeFirstSyncComponent else { return nil }
        return data.reduce(UInt64(0)) { ($0 << 8) | UInt64($1) }
    }

    public static func itemVersion(_ v: Version) -> NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: encode(v.content), metadataVersion: encode(v.meta))
    }

    public static func base(_ v: NSFileProviderItemVersion?) -> BaseVersion {
        guard let v else { return BaseVersion(content: nil, meta: nil) }
        return BaseVersion(content: decode(v.contentVersion), meta: decode(v.metadataVersion))
    }
}

public enum FieldCoding {
    private static let pairs: [(NSFileProviderItemFields, UInt32)] = [
        (.contents, IpcFields.contents),
        (.filename, IpcFields.filename),
        (.parentItemIdentifier, IpcFields.parent),
        (.lastUsedDate, IpcFields.lastUsedDate),
        (.tagData, IpcFields.tagData),
        (.favoriteRank, IpcFields.favoriteRank),
        (.creationDate, IpcFields.creationDate),
        (.contentModificationDate, IpcFields.contentModificationDate),
        (.fileSystemFlags, IpcFields.fileSystemFlags),
        (.extendedAttributes, IpcFields.extendedAttributes),
        (.typeAndCreator, IpcFields.typeAndCreator),
    ]

    public static func ipc(_ fields: NSFileProviderItemFields) -> UInt32 {
        pairs.reduce(0) { fields.contains($1.0) ? $0 | $1.1 : $0 }
    }

    public static func fileProvider(_ bits: UInt32) -> NSFileProviderItemFields {
        pairs.reduce(into: NSFileProviderItemFields()) { acc, p in
            if bits & p.1 != 0 { acc.insert(p.0) }
        }
    }
}

public enum CapabilityCoding {
    /// Never `.allowsTrashing` (review D7, MQ-008/009): deletes are real deletes on the VM.
    public static func fileProvider(_ caps: UInt32) -> NSFileProviderItemCapabilities {
        var c: NSFileProviderItemCapabilities = []
        if caps & IpcCaps.reading != 0 { c.formUnion([.allowsReading, .allowsContentEnumerating]) }
        if caps & IpcCaps.writing != 0 { c.formUnion([.allowsWriting, .allowsAddingSubItems]) }
        if caps & IpcCaps.reparenting != 0 { c.insert(.allowsReparenting) }
        if caps & IpcCaps.renaming != 0 { c.insert(.allowsRenaming) }
        if caps & IpcCaps.deleting != 0 { c.insert(.allowsDeleting) }
        if caps & IpcCaps.excludingFromSync != 0 { c.insert(.allowsExcludingFromSync) }
        return c
    }
}

/// One engine item as the system sees it.
public final class UnlatchItem: NSObject, NSFileProviderItem {
    public let ipc: IpcItem

    public init(_ ipc: IpcItem) {
        self.ipc = ipc
    }

    private var entry: Entry { ipc.entry }
    private var isRoot: Bool { entry.id == UnlatchIds.root }

    public var itemIdentifier: NSFileProviderItemIdentifier { ItemIdentifiers.identifier(for: entry.id) }

    public var parentItemIdentifier: NSFileProviderItemIdentifier {
        // The root's parent is itself in the engine; the system wants the root to be its own parent.
        isRoot ? .rootContainer : ItemIdentifiers.identifier(for: entry.parent)
    }

    public var filename: String { ipc.displayName }

    public var contentType: UTType {
        switch entry.kind {
        case .dir:
            return .folder
        case .symlink where !ipc.symlinkBlocked:
            return .symbolicLink
        case .symlink:
            // A blocked symlink is a read-only file whose content is the target text (review D12).
            return .plainText
        case .file:
            let ext = (ipc.displayName as NSString).pathExtension
            return ext.isEmpty ? .data : (UTType(filenameExtension: ext) ?? .data)
        }
    }

    public var capabilities: NSFileProviderItemCapabilities { CapabilityCoding.fileProvider(ipc.caps) }

    public var itemVersion: NSFileProviderItemVersion { VersionCoding.itemVersion(entry.version) }

    public var documentSize: NSNumber? { entry.kind == .dir ? nil : NSNumber(value: entry.size) }

    public var contentModificationDate: Date? { TimeCoding.date(ns: entry.mtimeNs) }

    public var creationDate: Date? { ipc.local.creationNs.map(TimeCoding.date(ns:)) }

    public var lastUsedDate: Date? { ipc.local.lastUsedNs.map(TimeCoding.date(ns:)) }

    /// Returned on every item: without it Finder tags vanish on the next re-download (MQ-043).
    public var tagData: Data? { ipc.local.tagData.map { Data($0) } }

    // No `favoriteRank`: NSFileProviderItem.favoriteRank is iOS-only (API_UNAVAILABLE(macos));
    // LocalMeta.favoriteRank stays in the protocol but a Mac never sets it.

    public var extendedAttributes: [String: Data] {
        Dictionary(ipc.local.xattrs.map { ($0.name, Data($0.value)) }, uniquingKeysWith: { _, last in last })
    }

    public var typeAndCreator: NSFileProviderTypeAndCreator {
        guard let tc = ipc.local.typeCreator else { return NSFileProviderTypeAndCreator() }
        return NSFileProviderTypeAndCreator(type: tc.type, creator: tc.creator)
    }

    public var fileSystemFlags: NSFileProviderFileSystemFlags {
        var f: NSFileProviderFileSystemFlags = [.userReadable]
        if ipc.caps & IpcCaps.writing != 0 && !ipc.symlinkBlocked { f.insert(.userWritable) }
        // Directories must be searchable; files carry the exec bit only when the engine exposes
        // it (review D12: hidden by default, never for .command/.tool/.terminal).
        if entry.kind == .dir || ipc.userExec { f.insert(.userExecutable) }
        if ipc.local.hidden { f.insert(.hidden) }
        return f
    }

    public var symlinkTargetPath: String? {
        entry.kind == .symlink && !ipc.symlinkBlocked ? entry.symlinkTarget : nil
    }

    /// Only the root carries a policy; everything else inherits it (MQ-026: `.inherited` is
    /// neutral). Evict-on-remote-update stops hot files being re-downloaded on every change (D9).
    public var contentPolicy: NSFileProviderContentPolicy {
        isRoot ? .downloadLazilyAndEvictOnRemoteUpdate : .inherited
    }
}

extension LocalMeta {
    /// The Mac-only fields of a system-provided item (template or modified item).
    public init(item: NSFileProviderItem) {
        self.init()
        if let tags = item.tagData ?? nil { tagData = [UInt8](tags) }
        if let date = item.lastUsedDate ?? nil { lastUsedNs = TimeCoding.ns(date) }
        if let date = item.creationDate ?? nil { creationNs = TimeCoding.ns(date) }
        if let attrs = item.extendedAttributes {
            xattrs = attrs.keys.sorted().map { XAttr(name: $0, value: [UInt8](attrs[$0] ?? Data())) }
        }
        if let flags = item.fileSystemFlags { hidden = flags.contains(.hidden) }
        if let tc = item.typeAndCreator, tc.type != 0 || tc.creator != 0 {
            typeCreator = TypeCreator(type: tc.type, creator: tc.creator)
        }
    }
}

public enum CreateKindMapping {
    /// What kind of item a create template is. Order matters: packages and aliases are
    /// directories/files too.
    public static func kind(of template: NSFileProviderItem) -> CreateKind {
        guard let type = template.contentType else { return .file }
        if type.conforms(to: .aliasFile) { return .alias }
        if type.conforms(to: .package) { return .package }
        if type.conforms(to: .symbolicLink) { return .symlink }
        if type.conforms(to: .folder) || type.conforms(to: .directory) { return .dir }
        return .file
    }
}

public enum MacLocalNames {
    /// Names macOS creates that never go to the VM (review §2(a)8; mirrors
    /// `unlatch_proto::is_mac_local_name`).
    public static func isMacLocal(_ name: String) -> Bool {
        name == ".DS_Store" || name.hasPrefix("._") || name == "Icon\r" || name == ".localized"
            || name.hasSuffix(".nosync")
    }
}

/// Where an error is returned; a few codes mean different things per call (MQ-011, MQ-012).
public enum ErrorSite {
    case item, enumerate, changes, fetch, create, modify, delete
}

public enum ErrorMapping {
    public static func unreachable(_ message: String? = nil) -> NSError {
        var info: [String: Any] = [:]
        if let message { info[NSLocalizedDescriptionKey] = message }
        return NSError(domain: NSFileProviderError.errorDomain, code: NSFileProviderError.Code.serverUnreachable.rawValue, userInfo: info)
    }

    public static func fileProvider(_ code: NSFileProviderError.Code, _ message: String) -> NSError {
        NSError(domain: NSFileProviderError.errorDomain, code: code.rawValue, userInfo: [NSLocalizedDescriptionKey: message])
    }

    public static func featureUnsupported() -> NSError {
        NSError(domain: NSCocoaErrorDomain, code: NSFeatureUnsupportedError)
    }

    /// Review §2(e)6. `engineLive` guards `item(for:)`: `.noSuchItem` there makes the system
    /// delete the item from disk (MQ-011), so while the engine is not live it is never answered.
    public static func error(code: ErrorCode, msg: String, current: IpcItem?, site: ErrorSite, engineLive: Bool = true) -> NSError {
        switch code {
        case .deletionRejected:
            if let current {
                return NSError.fileProviderErrorForRejectedDeletion(of: UnlatchItem(current))
            }
            return fileProvider(.deletionRejected, msg)
        case .notFound:
            if site == .item && !engineLive { return unreachable(msg) }
            return fileProvider(.noSuchItem, msg)
        case .notEmpty:
            return fileProvider(.directoryNotEmpty, msg)
        case .noSpace:
            return fileProvider(.insufficientQuota, msg)
        case .anchorExpired:
            return fileProvider(.syncAnchorExpired, msg)
        case .excludedFromSync:
            return fileProvider(.excludedFromSync, msg)
        case .offline, .timeout, .`protocol`, .needsUser:
            return unreachable(msg)
        case .indexChanged:
            // The engine emits Reimport; the host reimports and signals ErrorResolved, after
            // which the system retries this call against the new index.
            return unreachable(msg)
        case .cancelled:
            return NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError, userInfo: [NSLocalizedDescriptionKey: msg])
        case .unsupported:
            return featureUnsupported()
        case .cannotSync, .permission, .exists, .notDir, .isDir, .versionMismatch, .invalidName, .io, .rootReplaced:
            return fileProvider(.cannotSynchronize, msg)
        }
    }
}
