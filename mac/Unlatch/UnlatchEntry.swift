import Foundation

/// One executable, two roles: the menu-bar UI, and — when launchd starts it with `--agent` via
/// the bundled LaunchAgent plist — the engine agent hosting libunlatch (review §2(a)2).
@main
enum UnlatchEntry {
    @MainActor
    static func main() {
        // `--cli …`: scripted management (npx unlatch); prints, exits, never shows UI.
        if CommandLine.arguments.contains("--cli") {
            CLIMain.run()
        }
        if CommandLine.arguments.contains("--agent") {
            AgentMain.run()
        }
        UnlatchMenuApp.main()
    }
}
