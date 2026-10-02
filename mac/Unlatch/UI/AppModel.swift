import AppKit
import FileProvider
import Foundation
import ServiceManagement

/// UI state: the agent's registration and the per-VM snapshots it reports.
@MainActor
final class AppModel: ObservableObject {
    @Published private(set) var snapshots: [DomainSnapshot] = []
    @Published private(set) var agentState: AgentState = .unknown
    @Published var message: String?
    @Published private(set) var repairing = false

    let config: BundleConfig?
    private let client: AgentClient?
    private var timer: Timer?
    private var health = AgentHealth()
    /// At most one refresh in flight: a tick while the previous one waits is skipped.
    private var refreshing = false

    init() {
        let config = try? BundleConfig.from(bundle: .main)
        self.config = config
        client = config.map(AgentClient.init(config:))
        if config == nil {
            agentState = .failed("This build has no Unlatch signing configuration (see docs/MACOS.md).")
        }
        ensureAgentRegistered()
        timer = Timer.scheduledTimer(withTimeInterval: 2, repeats: true) { [weak self] _ in
            Task { @MainActor in await self?.refresh() }
        }
        Task { await refresh() }
    }

    var menuSymbol: String {
        if case .failed = agentState { return "exclamationmark.triangle" }
        let states = snapshots.compactMap { $0.status?.state }
        if states.contains(where: { if case .needsUser = $0 { return true }; if case .paused = $0 { return true }; return false }) {
            return "exclamationmark.triangle"
        }
        return "externaldrive.connected.to.line.below"
    }

    // MARK: agent registration

    private var agentService: SMAppService? {
        config.map { SMAppService.agent(plistName: $0.launchAgentPlistName) }
    }

    private var registration: AgentRegistration {
        switch agentService?.status {
        case .enabled?: return .enabled
        case .requiresApproval?: return .requiresApproval
        case .notRegistered?: return .notRegistered
        case .notFound?: return .notFound
        default: return .unknown
        }
    }

    func ensureAgentRegistered() {
        guard let service = agentService else { return }
        switch service.status {
        case .enabled:
            agentState = .enabled
        case .requiresApproval:
            agentState = .requiresApproval
        case .notRegistered, .notFound:
            do {
                try service.register()
                agentState = service.status == .requiresApproval ? .requiresApproval : .enabled
            } catch {
                agentState = .failed("Could not register the background agent: \(error.localizedDescription)")
            }
        @unknown default:
            agentState = .unknown
        }
    }

    func openLoginItemsSettings() {
        SMAppService.openSystemSettingsLoginItems()
    }

    /// `register()` does not repair a registration whose bundle was replaced (MQ-062), and
    /// `unregister()` returns before launchd drops the job (MQ-063): unregister, wait, register.
    func repairAgent() async {
        guard let service = agentService, !repairing else { return }
        repairing = true
        do {
            try await service.unregister()
        } catch {
            message = "Unregister failed: \(error.localizedDescription)"
        }
        try? await Task.sleep(nanoseconds: 6_000_000_000)
        health = AgentHealth()
        ensureAgentRegistered()
        repairing = false
        await refresh()
    }

    // MARK: domains

    /// Also re-derives `agentState`: the registration is re-read (approval in Login Items clears
    /// the banner), and an agent SMAppService calls `.enabled` that does not answer is shown as
    /// failed, which offers "Repair Background Agent" (MQ-062/063).
    func refresh() async {
        guard let client, !refreshing else { return }
        refreshing = true
        defer { refreshing = false }
        var failure: String?
        do {
            snapshots = try await client.listDomains()
        } catch {
            failure = "\(error)"
        }
        // A repair is unregistering on purpose; its own refresh follows.
        guard !repairing else { return }
        agentState = health.state(registration: registration, answered: failure == nil, failure: failure)
    }

    /// Add Unlatch VM: persist it in the agent, connect interactively (askpass dialogs may
    /// appear), which also adds the Finder location. A failed first connect removes it again so
    /// the user can fix the details and retry.
    func addAndConnect(_ record: DomainRecord) async throws {
        guard let client else { throw AgentClient.Failure(description: "Unlatch is not configured") }
        try await client.addDomain(record)
        do {
            try await client.connectInteractive(record.id)
        } catch {
            _ = try? await client.removeDomain(record.id)
            await refresh()
            throw error
        }
        await refresh()
    }

    func retry(_ id: String) {
        Task {
            do {
                try await client?.connectInteractive(id)
            } catch {
                message = "\(error)"
            }
            await refresh()
        }
    }

    func remove(_ id: String) {
        let name = snapshots.first(where: { $0.id == id })?.record.displayName ?? id
        Task {
            do {
                if let kept = try await client?.removeDomain(id) {
                    // Edits that never reached the VM: say where they are, and show them.
                    message = "Removed \(name). Edits that had not reached the VM were kept in \(kept)"
                    NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: kept)])
                }
            } catch {
                message = "\(error)"
            }
            await refresh()
        }
    }

    func confirmPaused(_ id: String, apply: Bool) {
        Task {
            do {
                try await client?.confirmPaused(id, apply: apply)
            } catch {
                message = "\(error)"
            }
            await refresh()
        }
    }

    func revealInFinder(_ record: DomainRecord) {
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(rawValue: record.id), displayName: record.displayName)
        guard let manager = NSFileProviderManager(for: domain) else {
            message = "The Finder location for \(record.displayName) is not available."
            return
        }
        manager.getUserVisibleURL(for: .rootContainer) { url, error in
            Task { @MainActor [weak self] in
                if let url {
                    NSWorkspace.shared.activateFileViewerSelecting([url])
                } else {
                    self?.message = "Cannot locate \(record.displayName): \(error?.localizedDescription ?? "unknown error")"
                }
            }
        }
    }

    func showAddVM() {
        AddVMWindow.show(model: self)
    }
}
