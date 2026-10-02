import Foundation
import XCTest
@testable import UnlatchShared

/// Tests that need neither FileProvider nor AppKit (they also run on Linux, which is how they
/// were first checked).
final class PortableTests: XCTestCase {
    func testBundleConfig() throws {
        let c = try BundleConfig.from(infoDictionary: ["UnlatchAppGroup": "ABCDE12345.dev.unlatch.unlatch", "UnlatchTeamID": "ABCDE12345", "UnlatchBundlePrefix": "dev.unlatch"])
        XCTAssertEqual(c.machServiceName, "ABCDE12345.dev.unlatch.unlatch.engine")
        XCTAssertEqual(c.extensionBundleID, "dev.unlatch.unlatch.fileprovider")
        XCTAssertEqual(c.launchAgentPlistName, "dev.unlatch.unlatch.agent.plist")
        XCTAssertEqual(c.agentClientRequirement,
                       "anchor apple generic and certificate leaf[subject.OU] = \"ABCDE12345\" and (identifier \"dev.unlatch.unlatch\" or identifier \"dev.unlatch.unlatch.fileprovider\")")
        XCTAssertThrowsError(try BundleConfig.from(infoDictionary: ["UnlatchAppGroup": "$(UNLATCH_APP_GROUP)", "UnlatchTeamID": "A", "UnlatchBundlePrefix": "b"]))
        let evil = BundleConfig(appGroup: "g", teamID: "A\" or true", bundlePrefix: "p")
        XCTAssertFalse(evil.agentClientRequirement.contains("\" or true"))
    }

    func testDestinationParsing() throws {
        XCTAssertEqual(try SSHDestination.parse("devbox"), SSHDestination(destination: "devbox", port: nil))
        XCTAssertEqual(try SSHDestination.parse(" me@10.0.0.5:2222 "), SSHDestination(destination: "me@10.0.0.5", port: 2222))
        XCTAssertEqual(try SSHDestination.parse("me@[fe80::1]:22"), SSHDestination(destination: "me@fe80::1", port: 22))
        XCTAssertEqual(try SSHDestination.parse("fe80::1"), SSHDestination(destination: "fe80::1", port: nil))
        XCTAssertThrowsError(try SSHDestination.parse(""))
        XCTAssertThrowsError(try SSHDestination.parse("-oProxyCommand=x"))
        XCTAssertThrowsError(try SSHDestination.parse("host:0"))
        XCTAssertThrowsError(try SSHDestination.parse("host:99999"))
        XCTAssertThrowsError(try SSHDestination.parse("a b"))
        let id = DomainRecord.makeIdentifier(displayName: "Dev Mum (GPU)!")
        XCTAssertTrue(id.hasPrefix("devbox-gpu-"), id)
        XCTAssertEqual(DomainRecord.makeIdentifier(displayName: "日本").prefix(3), "vm-")
    }

    func testSSHConfigParsing() {
        let text = """
        # comment
        Host devbox gpu
            HostName 10.0.0.5
            User me
            Port 2222
            IdentityFile ~/.ssh/id_ed25519
        Host *.corp !bad ?x
            User nobody
        Host=bastion
          HostName=bastion.example.com
        Match host foo
          User ignored
        Include extra
        Host "quoted"
        """
        let hosts = SSHConfigParser.parse(text, include: { arg in
            arg == "extra" ? ["Host from-include\n  HostName 192.168.1.9"] : []
        })
        XCTAssertEqual(hosts.map(\.alias), ["devbox", "gpu", "bastion", "from-include", "quoted"])
        XCTAssertEqual(hosts[0], SSHConfigHost(alias: "devbox", hostName: "10.0.0.5", user: "me", port: 2222, identityFile: "~/.ssh/id_ed25519"))
        XCTAssertEqual(hosts[2].hostName, "bastion.example.com")
        XCTAssertEqual(hosts[3].hostName, "192.168.1.9")
    }

    func testSSHConfigFromDisk() throws {
        let home = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let ssh = home.appendingPathComponent(".ssh/config.d", isDirectory: true)
        try FileManager.default.createDirectory(at: ssh, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: home) }
        try "Include config.d/*.conf\nHost a\n".write(to: home.appendingPathComponent(".ssh/config"), atomically: true, encoding: .utf8)
        try "Host b\nHost a\n".write(to: ssh.appendingPathComponent("1.conf"), atomically: true, encoding: .utf8)
        try "Host c\n".write(to: ssh.appendingPathComponent("2.conf"), atomically: true, encoding: .utf8)
        try "Host nope\n".write(to: ssh.appendingPathComponent("3.txt"), atomically: true, encoding: .utf8)
        XCTAssertEqual(SSHConfigParser.load(home: home).map(\.alias), ["a", "b", "c"])
    }

    func testShellEnvironmentParsing() {
        let marker = "UNLATCH_ENV_X"
        var out = Data("motd banner\n\(marker)".utf8)
        out.append(Data("PATH=/opt/homebrew/bin:/usr/bin\u{0}SSH_AUTH_SOCK=/s\u{0}EQ=a=b\u{0}".utf8))
        out.append(Data("\(marker)trailing".utf8))
        let env = ShellEnvironment.parse(out, marker: marker)
        XCTAssertEqual(env?["PATH"], "/opt/homebrew/bin:/usr/bin")
        XCTAssertEqual(env?["EQ"], "a=b")
        XCTAssertNil(ShellEnvironment.parse(Data("no markers".utf8), marker: marker))
        let merged = ShellEnvironment.sshEnvironment(base: ["PATH": "/usr/bin", "SSH_AUTH_SOCK": "/launchd", "HOME": "/h"], shell: env, useShellAgent: false)
        XCTAssertEqual(merged["PATH"], "/opt/homebrew/bin:/usr/bin")
        XCTAssertEqual(merged["SSH_AUTH_SOCK"], "/launchd")
        XCTAssertEqual(ShellEnvironment.sshEnvironment(base: [:], shell: env, useShellAgent: true)["SSH_AUTH_SOCK"], "/s")
        // The real thing, with /bin/sh as the "login shell".
        let real = ShellEnvironment.resolve(shell: "/bin/sh", timeout: 5)
        XCTAssertNotNil(real?["HOME"], "login-shell environment was not captured")
    }

    func testLocalNetworkClassification() {
        for h in ["10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.64.2", "169.254.1.1", "fe80::1", "fd12::1", "[fe80::1%en0]", "nas.local", "::ffff:192.168.1.1"] {
            XCTAssertTrue(LocalNetworkClassifier.isLocalNetwork(host: h), h)
        }
        for h in ["8.8.8.8", "172.32.0.1", "100.100.1.1", "127.0.0.1", "::1", "2001:db8::1", "example.com"] {
            XCTAssertFalse(LocalNetworkClassifier.isLocalNetwork(host: h), h)
        }
        XCTAssertFalse(LocalNetworkClassifier.resolvesToLocalNetwork(host: "localhost"))
    }
}
