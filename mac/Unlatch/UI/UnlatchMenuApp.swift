import SwiftUI

/// The menu-bar UI (no Dock icon: LSUIElement). The engine runs in the separate agent process.
struct UnlatchMenuApp: App {
    @StateObject private var model = AppModel()

    var body: some Scene {
        MenuBarExtra {
            MenuContent(model: model)
        } label: {
            Image(systemName: model.menuSymbol)
        }
        .menuBarExtraStyle(.window)
    }
}
