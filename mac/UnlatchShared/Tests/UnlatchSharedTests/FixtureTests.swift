import XCTest
@testable import UnlatchShared

/// Decodes every Rust-generated fixture (crates/unlatch-ffi/tests/fixtures.rs) into the Swift
/// mirrors, re-encodes it, and checks both the JSON and the libunlatch frame bytes agree — so any
/// drift between `unlatch_proto` and IpcModels.swift fails here.
final class FixtureTests: XCTestCase {
    static let fixturesDir = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent() // UnlatchSharedTests
        .deletingLastPathComponent() // Tests
        .deletingLastPathComponent() // UnlatchShared
        .appendingPathComponent("Fixtures", isDirectory: true)

    func fixtures(prefix: String) throws -> [(String, Data)] {
        let names = try FileManager.default.contentsOfDirectory(atPath: Self.fixturesDir.path)
            .filter { $0.hasPrefix(prefix) && $0.hasSuffix(".json") }
            .sorted()
        XCTAssertFalse(names.isEmpty, "no fixtures with prefix \(prefix) in \(Self.fixturesDir.path)")
        return try names.map { ($0, try Data(contentsOf: Self.fixturesDir.appendingPathComponent($0))) }
    }

    /// JSON value with nulls removed (Swift omits nil optionals; serde reads them as None).
    static func normalized(_ data: Data) throws -> NSObject {
        func strip(_ v: Any) -> Any {
            if let d = v as? [String: Any] {
                return d.filter { !($0.value is NSNull) }.mapValues(strip)
            }
            if let a = v as? [Any] { return a.map(strip) }
            return v
        }
        let obj = try JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
        guard let o = strip(obj) as? NSObject else { throw CocoaError(.coderInvalidValue) }
        return o
    }

    func assertRoundTrip<T: Codable & Equatable>(_ type: T.Type, _ name: String, _ data: Data, file: StaticString = #filePath, line: UInt = #line) throws -> T {
        let value: T
        do {
            value = try JSONDecoder().decode(T.self, from: data)
        } catch {
            XCTFail("\(name): Swift cannot decode Rust JSON: \(error)", file: file, line: line)
            throw error
        }
        let again = try JSONEncoder().encode(value)
        XCTAssertEqual(try Self.normalized(again), try Self.normalized(data), "\(name): Swift re-encoding differs from Rust JSON", file: file, line: line)
        XCTAssertEqual(try JSONDecoder().decode(T.self, from: again), value, "\(name)", file: file, line: line)
        return value
    }

    func testEveryRequestFixture() throws {
        var seen = Set<String>()
        for (name, data) in try fixtures(prefix: "ipc_request_") {
            let frame = try assertRoundTrip(IpcFrame<IpcRequest>.self, name, data)
            seen.insert(frame.msg.variantName)
            // Swift's JSON (nils omitted) must produce the same frame bytes as Rust's JSON.
            let rustJSON = String(decoding: data, as: UTF8.self)
            let fromRust = try UnlatchCodec.requestFrame(fromJSON: rustJSON)
            XCTAssertEqual(try UnlatchCodec.encode(frame), fromRust, name)
            XCTAssertEqual(try UnlatchCodec.decodeRequest(fromRust), frame, name)
        }
        XCTAssertEqual(seen, Set(IpcRequest.variantNames), "a request variant has no fixture or no Swift case")
    }

    func testEveryResponseFixture() throws {
        var seen = Set<String>()
        var codes = Set<ErrorCode>()
        for (name, data) in try fixtures(prefix: "ipc_response_") {
            let frame = try assertRoundTrip(IpcFrame<IpcResponse>.self, name, data)
            seen.insert(frame.msg.variantName)
            if case let .error(code, _, _) = frame.msg { codes.insert(code) }
            let fromRust = try UnlatchCodec.responseFrame(fromJSON: String(decoding: data, as: UTF8.self))
            XCTAssertEqual(try UnlatchCodec.encode(frame), fromRust, name)
            XCTAssertEqual(try UnlatchCodec.decodeResponse(fromRust), frame, name)
        }
        XCTAssertEqual(seen, Set(IpcResponse.variantNames))
        XCTAssertEqual(codes, Set(ErrorCode.allCases), "an ErrorCode has no fixture or no Swift case")
    }

    func testEngineStatusFixtures() throws {
        var states = Set<String>()
        for (name, data) in try fixtures(prefix: "engine_status_") {
            let status = try assertRoundTrip(EngineStatus.self, name, data)
            let json = try JSONSerialization.jsonObject(with: JSONEncoder().encode(status.state), options: [.fragmentsAllowed])
            states.insert((json as? String) ?? ((json as? [String: Any])?.keys.first ?? "?"))
        }
        XCTAssertEqual(states, Set(ConnState.variantNames))
    }

    func testEventFixtures() throws {
        var types = Set<String>()
        for (name, data) in try fixtures(prefix: "event_") {
            let event = try assertRoundTrip(EngineEvent.self, name, data)
            XCTAssertEqual(event.domain, "devbox")
            let obj = try JSONSerialization.jsonObject(with: data) as? [String: Any]
            types.insert(obj?["type"] as? String ?? "?")
        }
        XCTAssertEqual(types, Set(EngineEvent.typeNames))
    }

    func testEngineConfigFixtures() throws {
        for (name, data) in try fixtures(prefix: "engine_config_") {
            _ = try assertRoundTrip(EngineConfigJSON.self, name, data)
        }
    }

    func testCodecErrorsComeBackTyped() {
        XCTAssertThrowsError(try UnlatchCodec.requestFrame(fromJSON: "{")) { error in
            XCTAssertEqual((error as? UnlatchFFIError)?.code, "InvalidArgument")
        }
        XCTAssertThrowsError(try UnlatchCodec.decodeResponse(Data([1, 0, 0]))) { error in
            XCTAssertEqual((error as? UnlatchFFIError)?.code, "InvalidArgument")
        }
        XCTAssertGreaterThan(UnlatchCodec.protoVersion, 0)
        XCTAssertFalse(UnlatchCodec.libraryVersion.isEmpty)
    }
}
