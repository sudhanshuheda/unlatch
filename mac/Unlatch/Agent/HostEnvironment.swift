import Foundation
import Network
import os
import SystemConfiguration

enum HostEnvironment {
    /// The Mac's user-visible name, used in conflict file names on the VM (review D19: never
    /// `/etc/hostname`, which does not exist on macOS).
    static func computerName() -> String {
        if let name = SCDynamicStoreCopyComputerName(nil, nil) as String?, !name.isEmpty { return name }
        return Host.current().localizedName ?? "Mac"
    }

    /// `unlatchd` binaries staged by `mac/scripts/build-rust.sh` into `Resources/unlatchd/`, keyed by
    /// the VM's `uname -m`, each with its `.sha256` (verified on the VM before every exec).
    static func bundledUnlatchd(bundle: Bundle = .main) -> [EngineConfigJSON.UnlatchdBinary] {
        guard let dir = bundle.resourceURL?.appendingPathComponent("unlatchd", isDirectory: true) else { return [] }
        return ["x86_64", "aarch64"].compactMap { arch in
            let binary = dir.appendingPathComponent("unlatchd-\(arch)")
            let sumFile = dir.appendingPathComponent("unlatchd-\(arch).sha256")
            guard FileManager.default.isExecutableFile(atPath: binary.path),
                  let text = try? String(contentsOf: sumFile, encoding: .utf8),
                  let sum = text.split(whereSeparator: \.isWhitespace).first.map(String.init),
                  sum.count == 64, sum.allSatisfy(\.isHexDigit)
            else { return nil }
            return EngineConfigJSON.UnlatchdBinary(arch: arch, path: binary.path, sha256Hex: sum.lowercased())
        }
    }

    static var askpassPath: String? {
        Bundle.main.url(forAuxiliaryExecutable: "unlatch-askpass")?.path
    }
}

/// Local Network privacy nudge (MQ-069, review §2(e)9): for a LAN target, open and cancel an
/// NWConnection from our own process before spawning ssh, so the permission prompt appears in
/// Unlatch's name instead of ssh failing with "No route to host".
enum LocalNetworkNudge {
    private static let log = Logger(subsystem: "unlatch", category: "agent")

    static func nudgeIfNeeded(destination: String, port: UInt16?, environment: [String: String]) {
        let target = resolve(destination: destination, port: port, environment: environment)
        guard LocalNetworkClassifier.resolvesToLocalNetwork(host: target.host) else { return }
        log.info("local-network target \(target.host, privacy: .public); nudging permission")
        guard let nwPort = NWEndpoint.Port(rawValue: target.port) else { return }
        let connection = NWConnection(host: NWEndpoint.Host(target.host), port: nwPort, using: .tcp)
        let settled = DispatchSemaphore(value: 0)
        connection.stateUpdateHandler = { state in
            switch state {
            case .ready, .failed, .waiting, .cancelled:
                settled.signal()
            default:
                break
            }
        }
        connection.start(queue: .global(qos: .utility))
        _ = settled.wait(timeout: .now() + 3)
        connection.cancel()
    }

    /// The real host/port behind an ssh-config alias, from `ssh -G` (resolves Include/Match).
    static func resolve(destination: String, port: UInt16?, environment: [String: String]) -> (host: String, port: UInt16) {
        let fallbackHost = destination.split(separator: "@").last.map(String.init) ?? destination
        var host = fallbackHost
        var resolvedPort = port ?? 22
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/ssh")
        p.arguments = (port.map { ["-p", String($0)] } ?? []) + ["-G", "--", destination]
        p.environment = environment
        let out = Pipe()
        p.standardOutput = out
        p.standardError = FileHandle.nullDevice
        p.standardInput = FileHandle.nullDevice
        let done = DispatchSemaphore(value: 0)
        p.terminationHandler = { _ in done.signal() }
        do {
            try p.run()
        } catch {
            return (host, resolvedPort)
        }
        try? out.fileHandleForWriting.close()
        if done.wait(timeout: .now() + 3) == .timedOut {
            p.terminate()
            return (host, resolvedPort)
        }
        // `ssh -G` output is a few KB and fits in the pipe buffer.
        let text = String(decoding: out.fileHandleForReading.availableData, as: UTF8.self)
        for line in text.split(separator: "\n") {
            let parts = line.split(separator: " ", maxSplits: 1).map(String.init)
            guard parts.count == 2 else { continue }
            if parts[0] == "hostname" { host = parts[1] }
            if parts[0] == "port", let n = UInt16(parts[1]) { resolvedPort = n }
        }
        return (host, resolvedPort)
    }
}
