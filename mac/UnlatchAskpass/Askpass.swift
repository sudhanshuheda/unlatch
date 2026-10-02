import AppKit

/// `SSH_ASKPASS` helper for interactive connects (review D21): shows ssh's prompt in a native
/// dialog and prints the answer on stdout. Exit status 0 = answered, 1 = cancelled.
///
/// ssh sets `SSH_ASKPASS_PROMPT=confirm` for yes/no confirmations (exit status is the answer)
/// and `none` for notices (e.g. "touch your security key"). Host-key questions arrive as
/// ordinary prompts ending in "(yes/no/[fingerprint])?", answered with the word "yes".
@main
enum Askpass {
    @MainActor
    static func main() {
        let prompt = CommandLine.arguments.dropFirst().joined(separator: " ")
        let kind = ProcessInfo.processInfo.environment["SSH_ASKPASS_PROMPT"] ?? ""
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)

        let alert = NSAlert()
        alert.messageText = "Unlatch — SSH"
        alert.informativeText = prompt.isEmpty ? "ssh is asking for input." : prompt
        alert.alertStyle = .informational

        if kind == "none" {
            alert.addButton(withTitle: "OK")
            app.activate(ignoringOtherApps: true)
            _ = alert.runModal()
            exit(0)
        }

        let isHostKey = prompt.contains("(yes/no")
        if kind == "confirm" || isHostKey {
            alert.alertStyle = isHostKey ? .warning : .informational
            alert.addButton(withTitle: isHostKey ? "Trust Host and Connect" : "Allow")
            alert.addButton(withTitle: "Cancel")
            app.activate(ignoringOtherApps: true)
            guard alert.runModal() == .alertFirstButtonReturn else { exit(1) }
            if isHostKey { print("yes") }
            exit(0)
        }

        let field = NSSecureTextField(frame: NSRect(x: 0, y: 0, width: 300, height: 24))
        alert.accessoryView = field
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: "Cancel")
        alert.window.initialFirstResponder = field
        app.activate(ignoringOtherApps: true)
        guard alert.runModal() == .alertFirstButtonReturn else { exit(1) }
        FileHandle.standardOutput.write(Data((field.stringValue + "\n").utf8))
        exit(0)
    }
}
