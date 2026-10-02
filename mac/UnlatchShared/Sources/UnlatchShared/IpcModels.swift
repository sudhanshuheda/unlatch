import Foundation

// Codable mirrors of `unlatch_proto` (crates/unlatch-proto/src/{lib.rs,ipc.rs}) in serde's JSON form.
// libunlatch converts this JSON to and from the postcard frames that travel over XPC, so these
// types must match serde exactly: field names are the Rust snake_case names, enums are externally
// tagged, `Option` fields may be omitted (serde reads a missing Option as None), byte vectors are
// arrays of numbers. `crates/unlatch-ffi/tests/fixtures.rs` writes a JSON sample of every variant to
// mac/UnlatchShared/Fixtures and `UnlatchSharedTests` decodes, re-encodes and compares each of them.

public typealias ItemId = UInt64

public enum UnlatchIds {
    /// `ItemId::ROOT` — the configured root, `.rootContainer` for the system.
    public static let root: ItemId = 1
}

public struct Version: Codable, Equatable, Hashable {
    public var content: UInt64
    public var meta: UInt64

    public init(content: UInt64, meta: UInt64) {
        self.content = content
        self.meta = meta
    }
}

/// `nil` component = unknown (`NSFileProviderItemVersion.beforeFirstSyncComponent`).
public struct BaseVersion: Codable, Equatable {
    public var content: UInt64?
    public var meta: UInt64?

    public init(content: UInt64?, meta: UInt64?) {
        self.content = content
        self.meta = meta
    }
}

public enum Kind: String, Codable, Equatable {
    case file = "File"
    case dir = "Dir"
    case symlink = "Symlink"
}

public let accessRead: UInt8 = 1
public let accessWrite: UInt8 = 2
public let accessExec: UInt8 = 4

public struct Entry: Codable, Equatable {
    public var id: ItemId
    public var parent: ItemId
    public var name: String
    public var kind: Kind
    public var size: UInt64
    public var mtimeNs: Int64
    public var mode: UInt32
    public var version: Version
    public var symlinkTarget: String?
    public var lazy: Bool
    public var seq: UInt64
    public var access: UInt8

    enum CodingKeys: String, CodingKey {
        case id, parent, name, kind, size, mode, version, lazy, seq, access
        case mtimeNs = "mtime_ns"
        case symlinkTarget = "symlink_target"
    }
}

/// Append-only in Rust (encoding freeze), so an unknown code means Swift is out of date.
public enum ErrorCode: String, Codable, Equatable, CaseIterable {
    case notFound = "NotFound"
    case exists = "Exists"
    case notDir = "NotDir"
    case isDir = "IsDir"
    case notEmpty = "NotEmpty"
    case permission = "Permission"
    case versionMismatch = "VersionMismatch"
    case invalidName = "InvalidName"
    case noSpace = "NoSpace"
    case offline = "Offline"
    case anchorExpired = "AnchorExpired"
    case unsupported = "Unsupported"
    case timeout = "Timeout"
    case io = "Io"
    case `protocol` = "Protocol"
    case indexChanged = "IndexChanged"
    case deletionRejected = "DeletionRejected"
    case cannotSync = "CannotSync"
    case excludedFromSync = "ExcludedFromSync"
    case needsUser = "NeedsUser"
    case rootReplaced = "RootReplaced"
    case cancelled = "Cancelled"
}

/// `(String, Vec<u8>)` — serde writes a tuple as a two-element array.
public struct XAttr: Codable, Equatable {
    public var name: String
    public var value: [UInt8]

    public init(name: String, value: [UInt8]) {
        self.name = name
        self.value = value
    }

    public init(from decoder: Decoder) throws {
        var c = try decoder.unkeyedContainer()
        name = try c.decode(String.self)
        value = try c.decode([UInt8].self)
        guard c.isAtEnd else {
            throw DecodingError.dataCorruptedError(in: c, debugDescription: "xattr tuple has extra elements")
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.unkeyedContainer()
        try c.encode(name)
        try c.encode(value)
    }
}

/// `(u32, u32)` — HFS type and creator codes.
public struct TypeCreator: Codable, Equatable {
    public var type: UInt32
    public var creator: UInt32

    public init(type: UInt32, creator: UInt32) {
        self.type = type
        self.creator = creator
    }

    public init(from decoder: Decoder) throws {
        var c = try decoder.unkeyedContainer()
        type = try c.decode(UInt32.self)
        creator = try c.decode(UInt32.self)
        guard c.isAtEnd else {
            throw DecodingError.dataCorruptedError(in: c, debugDescription: "type/creator tuple has extra elements")
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.unkeyedContainer()
        try c.encode(type)
        try c.encode(creator)
    }
}

/// Mac-only metadata kept by the engine, never sent to the VM (review D8).
public struct LocalMeta: Codable, Equatable {
    public var tagData: [UInt8]?
    public var lastUsedNs: Int64?
    public var favoriteRank: UInt64?
    public var creationNs: Int64?
    public var xattrs: [XAttr]
    public var hidden: Bool
    public var typeCreator: TypeCreator?

    public init(
        tagData: [UInt8]? = nil,
        lastUsedNs: Int64? = nil,
        favoriteRank: UInt64? = nil,
        creationNs: Int64? = nil,
        xattrs: [XAttr] = [],
        hidden: Bool = false,
        typeCreator: TypeCreator? = nil
    ) {
        self.tagData = tagData
        self.lastUsedNs = lastUsedNs
        self.favoriteRank = favoriteRank
        self.creationNs = creationNs
        self.xattrs = xattrs
        self.hidden = hidden
        self.typeCreator = typeCreator
    }

    enum CodingKeys: String, CodingKey {
        case xattrs, hidden
        case tagData = "tag_data"
        case lastUsedNs = "last_used_ns"
        case favoriteRank = "favorite_rank"
        case creationNs = "creation_ns"
        case typeCreator = "type_creator"
    }
}

/// `unlatch_proto::ipc::caps` — the NSFileProviderItemCapabilities bits the engine computes.
public enum IpcCaps {
    public static let reading: UInt32 = 1 << 0
    public static let writing: UInt32 = 1 << 1
    public static let reparenting: UInt32 = 1 << 2
    public static let renaming: UInt32 = 1 << 3
    public static let deleting: UInt32 = 1 << 5
    public static let excludingFromSync: UInt32 = 1 << 7
}

/// `unlatch_proto::ipc::fields` — `changed_fields` / `still_pending` bits.
public enum IpcFields {
    public static let contents: UInt32 = 1 << 0
    public static let filename: UInt32 = 1 << 1
    public static let parent: UInt32 = 1 << 2
    public static let lastUsedDate: UInt32 = 1 << 3
    public static let tagData: UInt32 = 1 << 4
    public static let favoriteRank: UInt32 = 1 << 5
    public static let creationDate: UInt32 = 1 << 6
    public static let contentModificationDate: UInt32 = 1 << 7
    public static let fileSystemFlags: UInt32 = 1 << 8
    public static let extendedAttributes: UInt32 = 1 << 9
    public static let typeAndCreator: UInt32 = 1 << 10
}

public struct IpcItem: Codable, Equatable {
    public var entry: Entry
    public var displayName: String
    public var caps: UInt32
    public var local: LocalMeta
    public var userExec: Bool
    public var symlinkBlocked: Bool

    enum CodingKeys: String, CodingKey {
        case entry, caps, local
        case displayName = "display_name"
        case userExec = "user_exec"
        case symlinkBlocked = "symlink_blocked"
    }
}

public enum CreateKind: String, Codable, Equatable {
    case file = "File"
    case dir = "Dir"
    case symlink = "Symlink"
    case package = "Package"
    case alias = "Alias"
}

public struct CreateArgs: Equatable {
    public var templateId: String
    public var parent: ItemId
    public var name: String
    public var kind: CreateKind
    public var hasContent: Bool
    public var symlinkTarget: String?
    public var mtimeNs: Int64?
    public var userExec: Bool?
    public var changedFields: UInt32
    public var local: LocalMeta
    public var mayAlreadyExist: Bool
    public var deletionConflicted: Bool

    public init(
        templateId: String, parent: ItemId, name: String, kind: CreateKind, hasContent: Bool,
        symlinkTarget: String?, mtimeNs: Int64?, userExec: Bool?, changedFields: UInt32,
        local: LocalMeta, mayAlreadyExist: Bool, deletionConflicted: Bool
    ) {
        self.templateId = templateId
        self.parent = parent
        self.name = name
        self.kind = kind
        self.hasContent = hasContent
        self.symlinkTarget = symlinkTarget
        self.mtimeNs = mtimeNs
        self.userExec = userExec
        self.changedFields = changedFields
        self.local = local
        self.mayAlreadyExist = mayAlreadyExist
        self.deletionConflicted = deletionConflicted
    }
}

public struct ModifyArgs: Equatable {
    public var id: ItemId
    public var base: BaseVersion
    public var changedFields: UInt32
    public var newParent: ItemId?
    public var newName: String?
    public var hasContent: Bool
    public var mtimeNs: Int64?
    public var userExec: Bool?
    public var local: LocalMeta

    public init(
        id: ItemId, base: BaseVersion, changedFields: UInt32, newParent: ItemId?, newName: String?,
        hasContent: Bool, mtimeNs: Int64?, userExec: Bool?, local: LocalMeta
    ) {
        self.id = id
        self.base = base
        self.changedFields = changedFields
        self.newParent = newParent
        self.newName = newName
        self.hasContent = hasContent
        self.mtimeNs = mtimeNs
        self.userExec = userExec
        self.local = local
    }
}

/// `unlatch_proto::ipc::IpcRequest`.
public enum IpcRequest: Codable, Equatable {
    case hello(proto: UInt32, domain: String)
    case item(id: ItemId)
    case enumerate(container: ItemId, cursor: [UInt8]?, limit: UInt32, viewer: Bool)
    case currentAnchor
    case changesSince(anchor: [UInt8], limit: UInt32)
    case materializedChanged(added: [ItemId], removed: [ItemId], full: Bool)
    case fetch(id: ItemId, version: UInt64?, destDir: String)
    case create(CreateArgs)
    case modify(ModifyArgs)
    case delete(id: ItemId, base: BaseVersion, recursive: Bool)
    case cancel(call: UInt64)
    case status
    case confirmPaused(apply: Bool)

    /// Every variant name, in Rust declaration order (checked against the fixtures).
    public static let variantNames = [
        "Hello", "Item", "Enumerate", "CurrentAnchor", "ChangesSince", "MaterializedChanged",
        "Fetch", "Create", "Modify", "Delete", "Cancel", "Status", "ConfirmPaused",
    ]

    public var variantName: String {
        switch self {
        case .hello: return "Hello"
        case .item: return "Item"
        case .enumerate: return "Enumerate"
        case .currentAnchor: return "CurrentAnchor"
        case .changesSince: return "ChangesSince"
        case .materializedChanged: return "MaterializedChanged"
        case .fetch: return "Fetch"
        case .create: return "Create"
        case .modify: return "Modify"
        case .delete: return "Delete"
        case .cancel: return "Cancel"
        case .status: return "Status"
        case .confirmPaused: return "ConfirmPaused"
        }
    }

    public init(from decoder: Decoder) throws {
        let t = try ExternallyTagged(from: decoder)
        switch t.tag {
        case "Hello":
            let f = try t.fields()
            self = .hello(proto: try f.field("proto"), domain: try f.field("domain"))
        case "Item":
            self = .item(id: try t.fields().field("id"))
        case "Enumerate":
            let f = try t.fields()
            self = .enumerate(
                container: try f.field("container"), cursor: try f.optional("cursor"),
                limit: try f.field("limit"), viewer: try f.field("viewer"))
        case "CurrentAnchor":
            try t.unit()
            self = .currentAnchor
        case "ChangesSince":
            let f = try t.fields()
            self = .changesSince(anchor: try f.field("anchor"), limit: try f.field("limit"))
        case "MaterializedChanged":
            let f = try t.fields()
            self = .materializedChanged(
                added: try f.field("added"), removed: try f.field("removed"), full: try f.field("full"))
        case "Fetch":
            let f = try t.fields()
            self = .fetch(id: try f.field("id"), version: try f.optional("version"), destDir: try f.field("dest_dir"))
        case "Create":
            let f = try t.fields()
            self = .create(CreateArgs(
                templateId: try f.field("template_id"),
                parent: try f.field("parent"),
                name: try f.field("name"),
                kind: try f.field("kind"),
                hasContent: try f.field("has_content"),
                symlinkTarget: try f.optional("symlink_target"),
                mtimeNs: try f.optional("mtime_ns"),
                userExec: try f.optional("user_exec"),
                changedFields: try f.field("changed_fields"),
                local: try f.field("local"),
                mayAlreadyExist: try f.field("may_already_exist"),
                deletionConflicted: try f.field("deletion_conflicted")))
        case "Modify":
            let f = try t.fields()
            self = .modify(ModifyArgs(
                id: try f.field("id"),
                base: try f.field("base"),
                changedFields: try f.field("changed_fields"),
                newParent: try f.optional("new_parent"),
                newName: try f.optional("new_name"),
                hasContent: try f.field("has_content"),
                mtimeNs: try f.optional("mtime_ns"),
                userExec: try f.optional("user_exec"),
                local: try f.field("local")))
        case "Delete":
            let f = try t.fields()
            self = .delete(id: try f.field("id"), base: try f.field("base"), recursive: try f.field("recursive"))
        case "Cancel":
            self = .cancel(call: try t.fields().field("call"))
        case "Status":
            try t.unit()
            self = .status
        case "ConfirmPaused":
            self = .confirmPaused(apply: try t.fields().field("apply"))
        default:
            throw t.unknown("IpcRequest")
        }
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case let .hello(proto, domain):
            try TaggedWriter.fields("Hello", to: encoder) { f in
                try f.put(proto, "proto")
                try f.put(domain, "domain")
            }
        case let .item(id):
            try TaggedWriter.fields("Item", to: encoder) { try $0.put(id, "id") }
        case let .enumerate(container, cursor, limit, viewer):
            try TaggedWriter.fields("Enumerate", to: encoder) { f in
                try f.put(container, "container")
                try f.putOptional(cursor, "cursor")
                try f.put(limit, "limit")
                try f.put(viewer, "viewer")
            }
        case .currentAnchor:
            try TaggedWriter.unit("CurrentAnchor", to: encoder)
        case let .changesSince(anchor, limit):
            try TaggedWriter.fields("ChangesSince", to: encoder) { f in
                try f.put(anchor, "anchor")
                try f.put(limit, "limit")
            }
        case let .materializedChanged(added, removed, full):
            try TaggedWriter.fields("MaterializedChanged", to: encoder) { f in
                try f.put(added, "added")
                try f.put(removed, "removed")
                try f.put(full, "full")
            }
        case let .fetch(id, version, destDir):
            try TaggedWriter.fields("Fetch", to: encoder) { f in
                try f.put(id, "id")
                try f.putOptional(version, "version")
                try f.put(destDir, "dest_dir")
            }
        case let .create(a):
            try TaggedWriter.fields("Create", to: encoder) { f in
                try f.put(a.templateId, "template_id")
                try f.put(a.parent, "parent")
                try f.put(a.name, "name")
                try f.put(a.kind, "kind")
                try f.put(a.hasContent, "has_content")
                try f.putOptional(a.symlinkTarget, "symlink_target")
                try f.putOptional(a.mtimeNs, "mtime_ns")
                try f.putOptional(a.userExec, "user_exec")
                try f.put(a.changedFields, "changed_fields")
                try f.put(a.local, "local")
                try f.put(a.mayAlreadyExist, "may_already_exist")
                try f.put(a.deletionConflicted, "deletion_conflicted")
            }
        case let .modify(a):
            try TaggedWriter.fields("Modify", to: encoder) { f in
                try f.put(a.id, "id")
                try f.put(a.base, "base")
                try f.put(a.changedFields, "changed_fields")
                try f.putOptional(a.newParent, "new_parent")
                try f.putOptional(a.newName, "new_name")
                try f.put(a.hasContent, "has_content")
                try f.putOptional(a.mtimeNs, "mtime_ns")
                try f.putOptional(a.userExec, "user_exec")
                try f.put(a.local, "local")
            }
        case let .delete(id, base, recursive):
            try TaggedWriter.fields("Delete", to: encoder) { f in
                try f.put(id, "id")
                try f.put(base, "base")
                try f.put(recursive, "recursive")
            }
        case let .cancel(call):
            try TaggedWriter.fields("Cancel", to: encoder) { try $0.put(call, "call") }
        case .status:
            try TaggedWriter.unit("Status", to: encoder)
        case let .confirmPaused(apply):
            try TaggedWriter.fields("ConfirmPaused", to: encoder) { try $0.put(apply, "apply") }
        }
    }
}

public struct ServerInfo: Codable, Equatable {
    public var version: String
    public var hostname: String
    public var rootPath: String
    public var entries: UInt64
    public var watches: UInt64
    public var polled: Bool
    public var warnings: [String]

    enum CodingKeys: String, CodingKey {
        case version, hostname, entries, watches, polled, warnings
        case rootPath = "root_path"
    }
}

/// `unlatch_proto::ipc::ConnState`.
public enum ConnState: Codable, Equatable {
    case connecting
    case syncing(received: UInt64)
    case live
    case offline(error: String, retryInMs: UInt64)
    case needsUser(reason: String, url: String?)
    case paused(reason: String)

    public static let variantNames = ["Connecting", "Syncing", "Live", "Offline", "NeedsUser", "Paused"]

    public init(from decoder: Decoder) throws {
        let t = try ExternallyTagged(from: decoder)
        switch t.tag {
        case "Connecting":
            try t.unit()
            self = .connecting
        case "Syncing":
            self = .syncing(received: try t.fields().field("received"))
        case "Live":
            try t.unit()
            self = .live
        case "Offline":
            let f = try t.fields()
            self = .offline(error: try f.field("error"), retryInMs: try f.field("retry_in_ms"))
        case "NeedsUser":
            let f = try t.fields()
            self = .needsUser(reason: try f.field("reason"), url: try f.optional("url"))
        case "Paused":
            self = .paused(reason: try t.fields().field("reason"))
        default:
            throw t.unknown("ConnState")
        }
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .connecting:
            try TaggedWriter.unit("Connecting", to: encoder)
        case let .syncing(received):
            try TaggedWriter.fields("Syncing", to: encoder) { try $0.put(received, "received") }
        case .live:
            try TaggedWriter.unit("Live", to: encoder)
        case let .offline(error, retryInMs):
            try TaggedWriter.fields("Offline", to: encoder) { f in
                try f.put(error, "error")
                try f.put(retryInMs, "retry_in_ms")
            }
        case let .needsUser(reason, url):
            try TaggedWriter.fields("NeedsUser", to: encoder) { f in
                try f.put(reason, "reason")
                try f.putOptional(url, "url")
            }
        case let .paused(reason):
            try TaggedWriter.fields("Paused", to: encoder) { try $0.put(reason, "reason") }
        }
    }

    public var isLive: Bool {
        if case .live = self { return true }
        return false
    }
}

public struct EngineStatus: Codable, Equatable {
    public var state: ConnState
    public var entries: UInt64
    public var anchor: [UInt8]
    /// Last measured round-trip time in microseconds.
    public var rttUs: UInt64?
    public var cacheBytes: UInt64
    public var pendingUploads: UInt32
    public var server: ServerInfo?

    enum CodingKeys: String, CodingKey {
        case state, entries, anchor, server
        case rttUs = "rtt_us"
        case cacheBytes = "cache_bytes"
        case pendingUploads = "pending_uploads"
    }
}

/// `unlatch_proto::ipc::IpcResponse`.
public enum IpcResponse: Codable, Equatable {
    case hello(proto: UInt32, domain: String)
    case item(IpcItem)
    case page(items: [IpcItem], next: [UInt8]?)
    case anchor([UInt8])
    case changes(updated: [IpcItem], removed: [ItemId], anchor: [UInt8], more: Bool)
    case progress(done: UInt64, total: UInt64)
    case fetched(path: String, item: IpcItem)
    case done(item: IpcItem, stillPending: UInt32, shouldFetchContent: Bool, conflictCopy: IpcItem?)
    case deleted
    case status(EngineStatus)
    case ok
    case error(code: ErrorCode, msg: String, current: IpcItem?)

    public static let variantNames = [
        "Hello", "Item", "Page", "Anchor", "Changes", "Progress", "Fetched", "Done", "Deleted",
        "Status", "Ok", "Error",
    ]

    public var variantName: String {
        switch self {
        case .hello: return "Hello"
        case .item: return "Item"
        case .page: return "Page"
        case .anchor: return "Anchor"
        case .changes: return "Changes"
        case .progress: return "Progress"
        case .fetched: return "Fetched"
        case .done: return "Done"
        case .deleted: return "Deleted"
        case .status: return "Status"
        case .ok: return "Ok"
        case .error: return "Error"
        }
    }

    public init(from decoder: Decoder) throws {
        let t = try ExternallyTagged(from: decoder)
        switch t.tag {
        case "Hello":
            let f = try t.fields()
            self = .hello(proto: try f.field("proto"), domain: try f.field("domain"))
        case "Item":
            self = .item(try t.payload(IpcItem.self))
        case "Page":
            let f = try t.fields()
            self = .page(items: try f.field("items"), next: try f.optional("next"))
        case "Anchor":
            self = .anchor(try t.payload([UInt8].self))
        case "Changes":
            let f = try t.fields()
            self = .changes(
                updated: try f.field("updated"), removed: try f.field("removed"),
                anchor: try f.field("anchor"), more: try f.field("more"))
        case "Progress":
            let f = try t.fields()
            self = .progress(done: try f.field("done"), total: try f.field("total"))
        case "Fetched":
            let f = try t.fields()
            self = .fetched(path: try f.field("path"), item: try f.field("item"))
        case "Done":
            let f = try t.fields()
            self = .done(
                item: try f.field("item"), stillPending: try f.field("still_pending"),
                shouldFetchContent: try f.field("should_fetch_content"),
                conflictCopy: try f.optional("conflict_copy"))
        case "Deleted":
            try t.unit()
            self = .deleted
        case "Status":
            self = .status(try t.payload(EngineStatus.self))
        case "Ok":
            try t.unit()
            self = .ok
        case "Error":
            let f = try t.fields()
            self = .error(code: try f.field("code"), msg: try f.field("msg"), current: try f.optional("current"))
        default:
            throw t.unknown("IpcResponse")
        }
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case let .hello(proto, domain):
            try TaggedWriter.fields("Hello", to: encoder) { f in
                try f.put(proto, "proto")
                try f.put(domain, "domain")
            }
        case let .item(item):
            try TaggedWriter.payload("Item", item, to: encoder)
        case let .page(items, next):
            try TaggedWriter.fields("Page", to: encoder) { f in
                try f.put(items, "items")
                try f.putOptional(next, "next")
            }
        case let .anchor(anchor):
            try TaggedWriter.payload("Anchor", anchor, to: encoder)
        case let .changes(updated, removed, anchor, more):
            try TaggedWriter.fields("Changes", to: encoder) { f in
                try f.put(updated, "updated")
                try f.put(removed, "removed")
                try f.put(anchor, "anchor")
                try f.put(more, "more")
            }
        case let .progress(done, total):
            try TaggedWriter.fields("Progress", to: encoder) { f in
                try f.put(done, "done")
                try f.put(total, "total")
            }
        case let .fetched(path, item):
            try TaggedWriter.fields("Fetched", to: encoder) { f in
                try f.put(path, "path")
                try f.put(item, "item")
            }
        case let .done(item, stillPending, shouldFetchContent, conflictCopy):
            try TaggedWriter.fields("Done", to: encoder) { f in
                try f.put(item, "item")
                try f.put(stillPending, "still_pending")
                try f.put(shouldFetchContent, "should_fetch_content")
                try f.putOptional(conflictCopy, "conflict_copy")
            }
        case .deleted:
            try TaggedWriter.unit("Deleted", to: encoder)
        case let .status(status):
            try TaggedWriter.payload("Status", status, to: encoder)
        case .ok:
            try TaggedWriter.unit("Ok", to: encoder)
        case let .error(code, msg, current):
            try TaggedWriter.fields("Error", to: encoder) { f in
                try f.put(code, "code")
                try f.put(msg, "msg")
                try f.putOptional(current, "current")
            }
        }
    }
}

/// `unlatch_proto::ipc::IpcFrame<T>` — every IPC message carries its caller-chosen call id.
public struct IpcFrame<Message: Codable & Equatable>: Codable, Equatable {
    public var call: UInt64
    public var msg: Message

    public init(call: UInt64, msg: Message) {
        self.call = call
        self.msg = msg
    }
}

/// JSON the engine's event callback delivers (`crates/unlatch-ffi/src/event.rs`), internally
/// tagged by `"type"`.
public enum EngineEvent: Codable, Equatable {
    case workingSetChanged(domain: String, anchor: [UInt8])
    case errorResolved(domain: String)
    case reimport(domain: String, below: ItemId)
    case needsUser(domain: String, reason: String, url: String?)
    case statusChanged(domain: String, status: EngineStatus)

    public static let typeNames = ["WorkingSetChanged", "ErrorResolved", "Reimport", "NeedsUser", "StatusChanged"]

    public var domain: String {
        switch self {
        case let .workingSetChanged(d, _), let .errorResolved(d), let .reimport(d, _),
             let .needsUser(d, _, _), let .statusChanged(d, _):
            return d
        }
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: AnyKey.self)
        let type: String = try c.field("type")
        let domain: String = try c.field("domain")
        switch type {
        case "WorkingSetChanged":
            self = .workingSetChanged(domain: domain, anchor: try c.field("anchor"))
        case "ErrorResolved":
            self = .errorResolved(domain: domain)
        case "Reimport":
            self = .reimport(domain: domain, below: try c.field("below"))
        case "NeedsUser":
            self = .needsUser(domain: domain, reason: try c.field("reason"), url: try c.optional("url"))
        case "StatusChanged":
            self = .statusChanged(domain: domain, status: try c.field("status"))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: AnyKey("type"), in: c, debugDescription: "unknown engine event \(type)")
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: AnyKey.self)
        try c.put(domain, "domain")
        switch self {
        case let .workingSetChanged(_, anchor):
            try c.put("WorkingSetChanged", "type")
            try c.put(anchor, "anchor")
        case .errorResolved:
            try c.put("ErrorResolved", "type")
        case let .reimport(_, below):
            try c.put("Reimport", "type")
            try c.put(below, "below")
        case let .needsUser(_, reason, url):
            try c.put("NeedsUser", "type")
            try c.put(reason, "reason")
            try c.putOptional(url, "url")
        case let .statusChanged(_, status):
            try c.put("StatusChanged", "type")
            try c.put(status, "status")
        }
    }
}
