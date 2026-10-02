import Foundation

/// The `config_json` of `unlatch_engine_start` (crates/unlatch-ffi/src/config.rs). libunlatch rejects
/// unknown keys, so this struct is the whole schema.
public struct EngineConfigJSON: Codable, Equatable {
    public enum Transport: Codable, Equatable {
        case ssh(destination: String, port: UInt16?, identity: String?, extraArgs: [String])
        case command(argv: [String], env: [String: String])

        public init(from decoder: Decoder) throws {
            let t = try ExternallyTagged(from: decoder)
            switch t.tag {
            case "ssh":
                let f = try t.fields()
                self = .ssh(
                    destination: try f.field("destination"), port: try f.optional("port"),
                    identity: try f.optional("identity"), extraArgs: try f.optional("extra_args") ?? [])
            case "command":
                let f = try t.fields()
                self = .command(argv: try f.field("argv"), env: try f.optional("env") ?? [:])
            default:
                throw t.unknown("transport")
            }
        }

        public func encode(to encoder: Encoder) throws {
            switch self {
            case let .ssh(destination, port, identity, extraArgs):
                try TaggedWriter.fields("ssh", to: encoder) { f in
                    try f.put(destination, "destination")
                    try f.putOptional(port, "port")
                    try f.putOptional(identity, "identity")
                    try f.put(extraArgs, "extra_args")
                }
            case let .command(argv, env):
                try TaggedWriter.fields("command", to: encoder) { f in
                    try f.put(argv, "argv")
                    try f.put(env, "env")
                }
            }
        }
    }

    public struct UnlatchdBinary: Codable, Equatable {
        public var arch: String
        public var path: String
        public var sha256Hex: String

        public init(arch: String, path: String, sha256Hex: String) {
            self.arch = arch
            self.path = path
            self.sha256Hex = sha256Hex
        }

        enum CodingKeys: String, CodingKey {
            case arch, path
            case sha256Hex = "sha256_hex"
        }
    }

    public struct Prefetch: Codable, Equatable {
        public var maxFile: UInt64?
        public var perContainer: UInt64?
        public var bytesPerMin: UInt64?
        public var burst: UInt64?

        enum CodingKeys: String, CodingKey {
            case burst
            case maxFile = "max_file"
            case perContainer = "per_container"
            case bytesPerMin = "bytes_per_min"
        }
    }

    public var name: String
    public var transport: Transport
    public var remoteRoot: String
    public var stateDir: String
    public var clientName: String
    public var cacheDir: String?
    public var tempDir: String?
    public var unlatchdCommand: String?
    /// VM directory the bootstrap probe tries first (tests, unusual VM layouts).
    public var remoteInstallDir: String?
    public var unlatchdUpload: [UnlatchdBinary]
    public var cacheBudget: UInt64?
    public var prefetch: Prefetch?
    public var defaultLazyNames: [String]?
    public var sshEnv: [String: String]
    public var askpass: String?
    public var listTimeoutMs: UInt64?
    public var exposeExec: Bool?
    public var massDeleteFrac: Double?
    public var massDeleteAbs: UInt64?
    /// Floor of the mass-deletion fraction rule (engine default 32).
    public var massDeleteMin: UInt64?

    public init(name: String, transport: Transport, remoteRoot: String, stateDir: String, clientName: String) {
        self.name = name
        self.transport = transport
        self.remoteRoot = remoteRoot
        self.stateDir = stateDir
        self.clientName = clientName
        unlatchdUpload = []
        sshEnv = [:]
    }

    enum CodingKeys: String, CodingKey {
        case name, transport, prefetch, askpass
        case remoteRoot = "remote_root"
        case stateDir = "state_dir"
        case clientName = "client_name"
        case cacheDir = "cache_dir"
        case tempDir = "temp_dir"
        case unlatchdCommand = "unlatchd_command"
        case remoteInstallDir = "remote_install_dir"
        case unlatchdUpload = "unlatchd_upload"
        case cacheBudget = "cache_budget"
        case defaultLazyNames = "default_lazy_names"
        case sshEnv = "ssh_env"
        case listTimeoutMs = "list_timeout_ms"
        case exposeExec = "expose_exec"
        case massDeleteFrac = "mass_delete_frac"
        case massDeleteAbs = "mass_delete_abs"
        case massDeleteMin = "mass_delete_min"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try c.decode(String.self, forKey: .name)
        transport = try c.decode(Transport.self, forKey: .transport)
        remoteRoot = try c.decode(String.self, forKey: .remoteRoot)
        stateDir = try c.decode(String.self, forKey: .stateDir)
        clientName = try c.decode(String.self, forKey: .clientName)
        cacheDir = try c.decodeIfPresent(String.self, forKey: .cacheDir)
        tempDir = try c.decodeIfPresent(String.self, forKey: .tempDir)
        unlatchdCommand = try c.decodeIfPresent(String.self, forKey: .unlatchdCommand)
        remoteInstallDir = try c.decodeIfPresent(String.self, forKey: .remoteInstallDir)
        unlatchdUpload = try c.decodeIfPresent([UnlatchdBinary].self, forKey: .unlatchdUpload) ?? []
        cacheBudget = try c.decodeIfPresent(UInt64.self, forKey: .cacheBudget)
        prefetch = try c.decodeIfPresent(Prefetch.self, forKey: .prefetch)
        defaultLazyNames = try c.decodeIfPresent([String].self, forKey: .defaultLazyNames)
        sshEnv = try c.decodeIfPresent([String: String].self, forKey: .sshEnv) ?? [:]
        askpass = try c.decodeIfPresent(String.self, forKey: .askpass)
        listTimeoutMs = try c.decodeIfPresent(UInt64.self, forKey: .listTimeoutMs)
        exposeExec = try c.decodeIfPresent(Bool.self, forKey: .exposeExec)
        massDeleteFrac = try c.decodeIfPresent(Double.self, forKey: .massDeleteFrac)
        massDeleteAbs = try c.decodeIfPresent(UInt64.self, forKey: .massDeleteAbs)
        massDeleteMin = try c.decodeIfPresent(UInt64.self, forKey: .massDeleteMin)
    }

    public func jsonString() throws -> String {
        let data = try JSONEncoder().encode(self)
        return String(decoding: data, as: UTF8.self)
    }
}
