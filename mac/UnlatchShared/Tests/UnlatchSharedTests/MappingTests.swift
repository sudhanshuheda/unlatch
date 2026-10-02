import FileProvider
import UniformTypeIdentifiers
import XCTest
@testable import UnlatchShared

final class MappingTests: XCTestCase {
    func sampleItem(_ id: ItemId = 12, kind: Kind = .file, caps: UInt32 = IpcCaps.reading | IpcCaps.writing | IpcCaps.deleting,
                    blocked: Bool = false, name: String = "notes.md", local: LocalMeta = LocalMeta(), exec: Bool = false) -> IpcItem {
        IpcItem(
            entry: Entry(id: id, parent: id == 1 ? 1 : 5, name: name, kind: kind, size: 10, mtimeNs: 1_500_000_000_250_000_000,
                         mode: 0o644, version: Version(content: 0x0102_0304_0506_0708, meta: 9), symlinkTarget: kind == .symlink ? "../x" : nil,
                         lazy: false, seq: 9, access: accessRead | accessWrite),
            displayName: name, caps: caps, local: local, userExec: exec, symlinkBlocked: blocked)
    }

    func testIdentifiers() {
        XCTAssertEqual(ItemIdentifiers.identifier(for: 1), .rootContainer)
        XCTAssertEqual(ItemIdentifiers.identifier(for: 42).rawValue, "42")
        XCTAssertEqual(ItemIdentifiers.itemId(for: .rootContainer), 1)
        XCTAssertEqual(ItemIdentifiers.itemId(for: NSFileProviderItemIdentifier("42")), 42)
        XCTAssertNil(ItemIdentifiers.itemId(for: .workingSet))
        XCTAssertNil(ItemIdentifiers.itemId(for: .trashContainer))
        XCTAssertNil(ItemIdentifiers.itemId(for: NSFileProviderItemIdentifier("0")))
        XCTAssertNil(ItemIdentifiers.itemId(for: NSFileProviderItemIdentifier("NSFileProviderItemTemplate-1")))
    }

    func testVersionsAreBigEndianAndBeforeFirstSyncIsNil() {
        let d = VersionCoding.encode(0x0102_0304_0506_0708)
        XCTAssertEqual([UInt8](d), [1, 2, 3, 4, 5, 6, 7, 8])
        XCTAssertEqual(VersionCoding.decode(d), 0x0102_0304_0506_0708)
        XCTAssertNil(VersionCoding.decode(NSFileProviderItemVersion.beforeFirstSyncComponent))
        XCTAssertNil(VersionCoding.decode(Data([1, 2])))
        let v = VersionCoding.itemVersion(Version(content: 5, meta: 6))
        XCTAssertEqual(VersionCoding.base(v), BaseVersion(content: 5, meta: 6))
        XCTAssertEqual(VersionCoding.base(nil), BaseVersion(content: nil, meta: nil))
    }

    func testFieldsMapBothWays() {
        let all: NSFileProviderItemFields = [.contents, .filename, .parentItemIdentifier, .lastUsedDate, .tagData, .favoriteRank,
                                            .creationDate, .contentModificationDate, .fileSystemFlags, .extendedAttributes, .typeAndCreator]
        let bits = FieldCoding.ipc(all)
        XCTAssertEqual(bits, (1 << 11) - 1)
        XCTAssertEqual(FieldCoding.fileProvider(bits), all)
        XCTAssertEqual(FieldCoding.ipc(.filename), IpcFields.filename)
        XCTAssertEqual(FieldCoding.fileProvider(IpcFields.fileSystemFlags), .fileSystemFlags)
    }

    func testCapabilitiesNeverAllowTrashing() {
        let c = CapabilityCoding.fileProvider(UInt32.max)
        XCTAssertFalse(c.contains(.allowsTrashing))
        XCTAssertTrue(c.contains(.allowsDeleting))
        XCTAssertEqual(CapabilityCoding.fileProvider(IpcCaps.reading), [.allowsReading, .allowsContentEnumerating])
    }

    func testRootItem() {
        let root = UnlatchItem(sampleItem(1, kind: .dir, name: "code"))
        XCTAssertEqual(root.itemIdentifier, .rootContainer)
        XCTAssertEqual(root.parentItemIdentifier, .rootContainer)
        XCTAssertEqual(root.contentType, .folder)
        XCTAssertEqual(root.contentPolicy, .downloadLazilyAndEvictOnRemoteUpdate)
        XCTAssertNil(root.documentSize)
        XCTAssertTrue(root.fileSystemFlags.contains(.userExecutable))
    }

    func testFileItem() {
        let local = LocalMeta(tagData: [1, 2], lastUsedNs: 1_000_000_000, favoriteRank: 3, creationNs: 2_000_000_000,
                              xattrs: [XAttr(name: "user.a", value: [7])], hidden: true, typeCreator: TypeCreator(type: 1, creator: 2))
        let item = UnlatchItem(sampleItem(local: local))
        XCTAssertEqual(item.itemIdentifier.rawValue, "12")
        XCTAssertEqual(item.parentItemIdentifier.rawValue, "5")
        XCTAssertEqual(item.filename, "notes.md")
        XCTAssertEqual(item.contentPolicy, .inherited)
        XCTAssertEqual(item.documentSize, 10)
        XCTAssertEqual([UInt8](item.itemVersion.contentVersion), [1, 2, 3, 4, 5, 6, 7, 8])
        XCTAssertEqual(item.tagData, Data([1, 2]))
        XCTAssertEqual(item.extendedAttributes, ["user.a": Data([7])])
        XCTAssertEqual(item.typeAndCreator.type, 1)
        XCTAssertTrue(item.fileSystemFlags.contains(.hidden))
        XCTAssertTrue(item.fileSystemFlags.contains(.userWritable))
        XCTAssertFalse(item.fileSystemFlags.contains(.userExecutable))
        XCTAssertNil(item.symlinkTargetPath)
        // favoriteRank is iOS-only (unavailable on macOS): it never reaches or leaves the system.
        var roundTripped = local
        roundTripped.favoriteRank = nil
        XCTAssertEqual(LocalMeta(item: item), roundTripped)
    }

    func testSymlinks() {
        let link = UnlatchItem(sampleItem(kind: .symlink, name: "current"))
        XCTAssertEqual(link.contentType, .symbolicLink)
        XCTAssertEqual(link.symlinkTargetPath, "../x")
        let blocked = UnlatchItem(sampleItem(kind: .symlink, caps: IpcCaps.reading, blocked: true, name: "etc"))
        XCTAssertEqual(blocked.contentType, .plainText)
        XCTAssertNil(blocked.symlinkTargetPath)
        XCTAssertFalse(blocked.fileSystemFlags.contains(.userWritable))
        XCTAssertFalse(blocked.capabilities.contains(.allowsWriting))
    }

    func testErrorMapping() {
        func code(_ e: NSError) -> Int { e.code }
        let deleted = ErrorMapping.error(code: .deletionRejected, msg: "x", current: sampleItem(), site: .delete)
        XCTAssertEqual(deleted.domain, NSFileProviderError.errorDomain)
        XCTAssertEqual(deleted.code, NSFileProviderError.Code.deletionRejected.rawValue)
        XCTAssertEqual(code(ErrorMapping.error(code: .offline, msg: "", current: nil, site: .fetch)), NSFileProviderError.Code.serverUnreachable.rawValue)
        XCTAssertEqual(code(ErrorMapping.error(code: .cannotSync, msg: "", current: nil, site: .modify)), NSFileProviderError.Code.cannotSynchronize.rawValue)
        XCTAssertEqual(code(ErrorMapping.error(code: .permission, msg: "", current: nil, site: .modify)), NSFileProviderError.Code.cannotSynchronize.rawValue)
        XCTAssertEqual(code(ErrorMapping.error(code: .anchorExpired, msg: "", current: nil, site: .changes)), NSFileProviderError.Code.syncAnchorExpired.rawValue)
        XCTAssertEqual(code(ErrorMapping.error(code: .excludedFromSync, msg: "", current: nil, site: .create)), NSFileProviderError.Code.excludedFromSync.rawValue)
        // MQ-011: never .noSuchItem from item(for:) while the engine is not live.
        XCTAssertEqual(code(ErrorMapping.error(code: .notFound, msg: "", current: nil, site: .item, engineLive: false)), NSFileProviderError.Code.serverUnreachable.rawValue)
        XCTAssertEqual(code(ErrorMapping.error(code: .notFound, msg: "", current: nil, site: .item, engineLive: true)), NSFileProviderError.Code.noSuchItem.rawValue)
        let trash = ErrorMapping.featureUnsupported()
        XCTAssertEqual(trash.domain, NSCocoaErrorDomain)
        XCTAssertEqual(trash.code, NSFeatureUnsupportedError)
        for c in ErrorCode.allCases {
            _ = ErrorMapping.error(code: c, msg: "m", current: nil, site: .modify)
        }
    }

    func testMacLocalNames() {
        for n in [".DS_Store", "._foo", "Icon\r", ".localized", "x.nosync"] { XCTAssertTrue(MacLocalNames.isMacLocal(n), n) }
        for n in ["DS_Store", "foo._", "Icon", "a.txt"] { XCTAssertFalse(MacLocalNames.isMacLocal(n), n) }
    }
}
