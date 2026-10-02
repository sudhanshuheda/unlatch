import Foundation

/// How the menu-bar UI presents the engine agent.
public enum AgentState: Equatable {
    case unknown
    case enabled
    case requiresApproval
    case failed(String)
}

/// `SMAppService.Status`, mirrored so the decision below is testable off macOS.
public enum AgentRegistration: Equatable {
    case enabled, requiresApproval, notRegistered, notFound, unknown
}

/// The UI's view of the agent, re-derived on every refresh. `SMAppService` keeps reporting
/// `.enabled` while launchd cannot spawn a replaced bundle (MQ-062) or the mach service accepts
/// connections and answers nothing (MQ-063), so registration alone cannot tell the menu to
/// offer "Repair Background Agent": unanswered calls do.
public struct AgentHealth {
    /// Consecutive unanswered refreshes before the agent counts as dead (one can be a cold
    /// start by launchd).
    public static let missesBeforeRepair = 2

    public private(set) var consecutiveMisses = 0

    public init() {}

    /// `registration` as SMAppService reports it now; `answered` whether `listDomains` replied.
    public mutating func state(registration: AgentRegistration, answered: Bool, failure: String? = nil) -> AgentState {
        switch registration {
        case .requiresApproval:
            consecutiveMisses = 0
            return .requiresApproval
        case .notRegistered, .notFound:
            consecutiveMisses = 0
            return .failed("The background agent is not registered, so Finder cannot reach your VMs.")
        case .enabled, .unknown:
            consecutiveMisses = answered ? 0 : consecutiveMisses + 1
            if consecutiveMisses >= Self.missesBeforeRepair {
                return .failed("The background agent is not responding (\(failure ?? "no reply")). This can happen after an update.")
            }
            return registration == .enabled ? .enabled : .unknown
        }
    }
}
