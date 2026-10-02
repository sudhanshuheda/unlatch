import CUnlatch
import Foundation

/// An error reported by libunlatch through `out_error` (`{"code": …, "msg": …}`).
public struct UnlatchFFIError: Error, Equatable, CustomStringConvertible {
    /// An `unlatch_proto::ErrorCode` name, or libunlatch's own "InvalidArgument" / "Panic".
    public var code: String
    public var msg: String

    public init(code: String, msg: String) {
        self.code = code
        self.msg = msg
    }

    public var errorCode: ErrorCode? { ErrorCode(rawValue: code) }
    public var description: String { "\(code): \(msg)" }

    /// Takes ownership of an `out_error` string (frees it).
    static func take(_ p: UnsafeMutablePointer<CChar>?) -> UnlatchFFIError {
        guard let p else { return UnlatchFFIError(code: "Unknown", msg: "libunlatch reported no error detail") }
        defer { unlatch_free_string(p) }
        let text = String(cString: p)
        struct Wire: Decodable { let code: String; let msg: String }
        if let w = try? JSONDecoder().decode(Wire.self, from: Data(text.utf8)) {
            return UnlatchFFIError(code: w.code, msg: w.msg)
        }
        return UnlatchFFIError(code: "Unknown", msg: text)
    }
}

/// Swift ⇄ frame conversion through libunlatch (serde JSON in the middle), so Swift never needs to
/// know postcard. A frame is what travels over XPC: `u32 LE length`, flags, payload.
public enum UnlatchCodec {
    public static var protoVersion: UInt32 { unlatch_proto_version() }
    public static var libraryVersion: String { String(cString: unlatch_version()) }

    public static func encode(_ frame: IpcFrame<IpcRequest>) throws -> Data {
        try requestFrame(fromJSON: json(frame))
    }

    public static func encode(_ frame: IpcFrame<IpcResponse>) throws -> Data {
        try responseFrame(fromJSON: json(frame))
    }

    public static func decodeRequest(_ frame: Data) throws -> IpcFrame<IpcRequest> {
        try JSONDecoder().decode(IpcFrame<IpcRequest>.self, from: Data(requestJSON(fromFrame: frame).utf8))
    }

    public static func decodeResponse(_ frame: Data) throws -> IpcFrame<IpcResponse> {
        try JSONDecoder().decode(IpcFrame<IpcResponse>.self, from: Data(responseJSON(fromFrame: frame).utf8))
    }

    public static func requestFrame(fromJSON json: String) throws -> Data {
        try bytes { unlatch_ipc_encode_request_json(json, $0) }
    }

    public static func responseFrame(fromJSON json: String) throws -> Data {
        try bytes { unlatch_ipc_encode_response_json(json, $0) }
    }

    public static func requestJSON(fromFrame frame: Data) throws -> String {
        try string(frame) { unlatch_ipc_decode_request_json($0, $1, $2) }
    }

    public static func responseJSON(fromFrame frame: Data) throws -> String {
        try string(frame) { unlatch_ipc_decode_response_json($0, $1, $2) }
    }

    static func json<T: Encodable>(_ value: T) throws -> String {
        String(decoding: try JSONEncoder().encode(value), as: UTF8.self)
    }

    private static func bytes(
        _ body: (UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> UnlatchBytes
    ) throws -> Data {
        var err: UnsafeMutablePointer<CChar>?
        let out = withUnsafeMutablePointer(to: &err) { body($0) }
        guard let ptr = out.ptr else { throw UnlatchFFIError.take(err) }
        defer { unlatch_free_bytes(out) }
        return Data(bytes: ptr, count: Int(out.len))
    }

    private static func string(
        _ frame: Data,
        _ body: (UnsafePointer<UInt8>?, Int, UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> UnsafeMutablePointer<CChar>?
    ) throws -> String {
        var err: UnsafeMutablePointer<CChar>?
        let out: UnsafeMutablePointer<CChar>? = frame.withUnsafeBytes { raw in
            withUnsafeMutablePointer(to: &err) { errPtr in
                body(raw.bindMemory(to: UInt8.self).baseAddress, raw.count, errPtr)
            }
        }
        guard let out else { throw UnlatchFFIError.take(err) }
        defer { unlatch_free_string(out) }
        return String(cString: out)
    }
}
