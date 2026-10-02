import AppKit
import SwiftUI

/// The "Add VM" window (AppKit-hosted so it never opens by itself at launch).
@MainActor
enum AddVMWindow {
    private static var window: NSWindow?

    static func show(model: AppModel) {
        if let window {
            window.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
            return
        }
        let w = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 480, height: 420),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false)
        w.title = "Add VM"
        w.isReleasedWhenClosed = false
        w.contentViewController = NSHostingController(rootView: AddVMView(model: model) {
            w.close()
            AddVMWindow.window = nil
        })
        w.center()
        window = w
        w.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }
}

struct AddVMView: View {
    @ObservedObject var model: AppModel
    let onClose: () -> Void

    private enum Source: Hashable { case sshConfig, manual }

    @State private var hosts: [SSHConfigHost] = SSHConfigParser.load()
    @State private var source: Source = .sshConfig
    @State private var alias = ""
    @State private var manual = ""
    @State private var remoteRoot = "~"
    @State private var identity = ""
    @State private var displayName = ""
    @State private var useShellAgent = false
    @State private var busy = false
    @State private var error: String?

    init(model: AppModel, onClose: @escaping () -> Void) {
        self.model = model
        self.onClose = onClose
    }

    var body: some View {
        Form {
            Picker("Host", selection: $source) {
                Text("From ~/.ssh/config").tag(Source.sshConfig)
                Text("user@host[:port]").tag(Source.manual)
            }
            .pickerStyle(.segmented)
            if source == .sshConfig {
                if hosts.isEmpty {
                    Text("No Host entries found in ~/.ssh/config.").foregroundStyle(.secondary)
                } else {
                    Picker("Alias", selection: $alias) {
                        Text("Choose…").tag("")
                        ForEach(hosts) { h in
                            Text(label(for: h)).tag(h.alias)
                        }
                    }
                }
            } else {
                TextField("Destination", text: $manual, prompt: Text("me@10.0.0.5:22"))
            }
            TextField("Remote folder", text: $remoteRoot, prompt: Text("~/code"))
            HStack {
                TextField("Identity file (optional)", text: $identity, prompt: Text("~/.ssh/id_ed25519"))
                Button("Choose…", action: chooseIdentity)
            }
            TextField("Name in Finder", text: $displayName, prompt: Text(defaultName.isEmpty ? "dev-vm" : defaultName))
            Toggle("Use the SSH agent from my login shell (1Password, Secretive…)", isOn: $useShellAgent)
            if let error {
                Text(error).foregroundStyle(.red).font(.callout).textSelection(.enabled)
            }
            HStack {
                if busy {
                    ProgressView().controlSize(.small)
                    Text("Connecting — answer any SSH prompts…").font(.callout)
                }
                Spacer()
                Button("Cancel", action: onClose).keyboardShortcut(.cancelAction)
                Button("Connect", action: connect)
                    .keyboardShortcut(.defaultAction)
                    .disabled(busy)
            }
        }
        .formStyle(.grouped)
        .padding()
        .frame(minWidth: 460)
    }

    private var defaultName: String {
        source == .sshConfig ? alias : ((try? SSHDestination.parse(manual))?.destination.split(separator: "@").last.map(String.init) ?? "")
    }

    private func label(for h: SSHConfigHost) -> String {
        let target = [h.user.map { "\($0)@" }, h.hostName].compactMap { $0 }.joined()
        return target.isEmpty ? h.alias : "\(h.alias) (\(target))"
    }

    private func chooseIdentity() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.showsHiddenFiles = true
        panel.directoryURL = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".ssh")
        if panel.runModal() == .OK, let url = panel.url {
            identity = url.path
        }
    }

    private func connect() {
        error = nil
        let dest: SSHDestination
        do {
            dest = try SSHDestination.parse(source == .sshConfig ? alias : manual)
        } catch {
            self.error = source == .sshConfig ? "Choose a host." : "Enter a destination like me@host or me@host:2222."
            return
        }
        let root = remoteRoot.trimmingCharacters(in: .whitespaces)
        guard !root.isEmpty else {
            error = "Enter the folder on the VM to show in Finder."
            return
        }
        let name = displayName.trimmingCharacters(in: .whitespaces).isEmpty ? defaultName : displayName
        let record = DomainRecord(
            id: DomainRecord.makeIdentifier(displayName: name),
            displayName: name.isEmpty ? dest.destination : name,
            destination: dest.destination,
            port: dest.port,
            identityFile: identity.isEmpty ? nil : identity,
            remoteRoot: root,
            useShellAgent: useShellAgent)
        busy = true
        Task {
            defer { busy = false }
            do {
                try await model.addAndConnect(record)
                onClose()
            } catch {
                self.error = "\(error)"
            }
        }
    }
}
