import Foundation

/// The login shell's environment, resolved once per agent launch (review D21): launchd gives the
/// agent a bare PATH and Apple's SSH_AUTH_SOCK, which breaks ProxyCommands that call Homebrew
/// tools and agents exported from `.zshrc`. Like VS Code, run `$SHELL -l -i -c` and print the
/// environment between markers (interactive shells may print banners around it).
public enum ShellEnvironment {
    public static func command(marker: String) -> String {
        "printf '%s' '\(marker)'; env -0; printf '%s' '\(marker)'"
    }

    /// `env -0` output between two markers → variables. `nil` if the markers are missing.
    public static func parse(_ output: Data, marker: String) -> [String: String]? {
        let m = Data(marker.utf8)
        guard let start = output.range(of: m),
              let end = output.range(of: m, options: [], in: start.upperBound..<output.endIndex)
        else { return nil }
        let body = output[start.upperBound..<end.lowerBound]
        var env: [String: String] = [:]
        for chunk in body.split(separator: 0, omittingEmptySubsequences: true) {
            guard let s = String(data: Data(chunk), encoding: .utf8), let eq = s.firstIndex(of: "=") else { continue }
            env[String(s[..<eq])] = String(s[s.index(after: eq)...])
        }
        return env
    }

    /// The environment to spawn ssh with: ours, with PATH from the login shell and — only when
    /// the user opted in for this VM — its SSH_AUTH_SOCK.
    public static func sshEnvironment(base: [String: String], shell: [String: String]?, useShellAgent: Bool) -> [String: String] {
        var env = base
        if let path = shell?["PATH"], !path.isEmpty { env["PATH"] = path }
        if useShellAgent, let sock = shell?["SSH_AUTH_SOCK"], !sock.isEmpty { env["SSH_AUTH_SOCK"] = sock }
        return env
    }

    /// Run the login shell with a timeout; `nil` on failure (callers fall back to our own env).
    /// Stops reading as soon as both markers are in: a `.zshrc` that starts a background daemon
    /// keeps the pipe open, so waiting for EOF could hang.
    public static func resolve(shell: String? = nil, timeout: TimeInterval = 5) -> [String: String]? {
        let shellPath = shell ?? loginShell()
        let marker = "UNLATCH_ENV_" + UUID().uuidString.replacingOccurrences(of: "-", with: "")
        let p = Process()
        p.executableURL = URL(fileURLWithPath: shellPath)
        p.arguments = ["-l", "-i", "-c", command(marker: marker)]
        p.standardInput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        let pipe = Pipe()
        p.standardOutput = pipe
        do { try p.run() } catch { return nil }
        // Our copy of the write end would keep the pipe from ever reaching EOF.
        try? pipe.fileHandleForWriting.close()

        let collected = OutputCollector()
        let done = DispatchSemaphore(value: 0)
        let fd = pipe.fileHandleForReading.fileDescriptor
        let reader = Thread {
            var buf = [UInt8](repeating: 0, count: 64 * 1024)
            while true {
                let n = buf.withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
                if n < 0 && errno == EINTR { continue }
                if n <= 0 { break }
                collected.append(Data(buf[0..<n]))
                if parse(collected.data, marker: marker) != nil { break }
            }
            done.signal()
        }
        reader.start()
        _ = done.wait(timeout: .now() + timeout)
        if p.isRunning {
            // Interactive shells ignore SIGTERM.
            kill(p.processIdentifier, SIGKILL)
        }
        return parse(collected.data, marker: marker)
    }

    /// `$SHELL`, else the account's shell (launchd agents often have no SHELL), else zsh.
    public static func loginShell() -> String {
        if let s = ProcessInfo.processInfo.environment["SHELL"], !s.isEmpty { return s }
        if let pw = getpwuid(getuid()), let sh = pw.pointee.pw_shell {
            let s = String(cString: sh)
            if !s.isEmpty { return s }
        }
        return "/bin/zsh"
    }
}

private final class OutputCollector {
    private let lock = NSLock()
    private var buffer = Data()

    func append(_ d: Data) {
        lock.lock()
        buffer.append(d)
        lock.unlock()
    }

    var data: Data {
        lock.lock()
        defer { lock.unlock() }
        return buffer
    }
}
