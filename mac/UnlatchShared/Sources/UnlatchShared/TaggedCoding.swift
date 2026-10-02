import Foundation

/// A coding key for arbitrary strings: serde field names and enum variant tags.
public struct AnyKey: CodingKey, Hashable {
    public var stringValue: String
    public var intValue: Int? { nil }

    public init(_ string: String) { stringValue = string }
    public init?(stringValue: String) { self.stringValue = stringValue }
    public init?(intValue: Int) { return nil }
}

/// Reads serde's *externally tagged* enum representation:
/// a unit variant is the bare string `"Status"`, every other variant a single-key object
/// `{"Item": {...}}` (struct variant) or `{"Anchor": [..]}` (newtype variant).
struct ExternallyTagged {
    let tag: String
    private let object: KeyedDecodingContainer<AnyKey>?
    private let codingPath: [CodingKey]

    init(from decoder: Decoder) throws {
        codingPath = decoder.codingPath
        if let single = try? decoder.singleValueContainer(), let name = try? single.decode(String.self) {
            tag = name
            object = nil
            return
        }
        let container = try decoder.container(keyedBy: AnyKey.self)
        guard container.allKeys.count == 1, let key = container.allKeys.first else {
            throw DecodingError.dataCorrupted(.init(
                codingPath: decoder.codingPath,
                debugDescription: "expected one variant key, found \(container.allKeys.map(\.stringValue))"))
        }
        tag = key.stringValue
        object = container
    }

    /// Fields of a struct variant.
    func fields() throws -> KeyedDecodingContainer<AnyKey> {
        guard let object else { throw corrupted("variant \(tag) needs fields") }
        return try object.nestedContainer(keyedBy: AnyKey.self, forKey: AnyKey(tag))
    }

    /// Payload of a newtype variant.
    func payload<T: Decodable>(_ type: T.Type) throws -> T {
        guard let object else { throw corrupted("variant \(tag) needs a payload") }
        return try object.decode(T.self, forKey: AnyKey(tag))
    }

    /// Asserts a unit variant.
    func unit() throws {
        if object != nil { throw corrupted("variant \(tag) takes no fields") }
    }

    func unknown(_ type: String) -> DecodingError {
        corrupted("unknown \(type) variant \(tag)")
    }

    private func corrupted(_ message: String) -> DecodingError {
        DecodingError.dataCorrupted(.init(codingPath: codingPath, debugDescription: message))
    }
}

extension KeyedDecodingContainer where K == AnyKey {
    /// Required field.
    func field<T: Decodable>(_ key: String) throws -> T {
        try decode(T.self, forKey: AnyKey(key))
    }

    /// `Option<T>`: missing or `null` → nil.
    func optional<T: Decodable>(_ key: String) throws -> T? {
        try decodeIfPresent(T.self, forKey: AnyKey(key))
    }
}

/// Writes serde's externally tagged representation.
enum TaggedWriter {
    static func unit(_ tag: String, to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        try c.encode(tag)
    }

    static func payload<T: Encodable>(_ tag: String, _ value: T, to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: AnyKey.self)
        try c.encode(value, forKey: AnyKey(tag))
    }

    /// Struct variant: `body` fills the variant's field container.
    static func fields(
        _ tag: String,
        to encoder: Encoder,
        _ body: (inout KeyedEncodingContainer<AnyKey>) throws -> Void
    ) throws {
        var c = encoder.container(keyedBy: AnyKey.self)
        var f = c.nestedContainer(keyedBy: AnyKey.self, forKey: AnyKey(tag))
        try body(&f)
    }
}

extension KeyedEncodingContainer where K == AnyKey {
    mutating func put<T: Encodable>(_ value: T, _ key: String) throws {
        try encode(value, forKey: AnyKey(key))
    }

    /// `Option<T>`: nil is omitted; serde reads a missing `Option` field as `None`.
    mutating func putOptional<T: Encodable>(_ value: T?, _ key: String) throws {
        try encodeIfPresent(value, forKey: AnyKey(key))
    }
}
