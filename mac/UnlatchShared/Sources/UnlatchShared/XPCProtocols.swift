import Foundation

/// Exported by the engine agent on its MachService. Every IPC message is one complete libunlatch
/// frame (`IpcFrame<IpcRequest>`), optionally with the content file handle of a create/modify.
/// The first frame on a connection must be `Hello { domain }`: it binds the connection to that
/// domain's engine. Replies come back through `UnlatchClientXPC.deliver` on the same connection.
@objc(UnlatchEngineXPC)
public protocol UnlatchEngineXPC {
    func submit(_ frame: Data, fileHandle: FileHandle?)

    // Management (menu-bar UI).
    /// JSON `[DomainSnapshot]`.
    func listDomains(reply: @escaping (Data?, String?) -> Void)
    /// `record` is JSON `DomainRecord`; reply = error message or nil.
    func addDomain(_ record: Data, reply: @escaping (String?) -> Void)
    /// Kept for clients built before `removeDomainPreservingEdits` (reply = error message or nil).
    func removeDomain(_ identifier: String, reply: @escaping (String?) -> Void)
    /// Removes the Finder domain keeping files with edits not yet uploaded; reply = (error
    /// message or nil, path where those files were kept or nil when there were none).
    func removeDomainPreservingEdits(_ identifier: String, reply: @escaping (String?, String?) -> Void)
    /// Blocks (in the agent) while ssh may show askpass dialogs.
    func connectInteractive(_ identifier: String, reply: @escaping (String?) -> Void)
    func confirmPaused(_ identifier: String, apply: Bool, reply: @escaping (String?) -> Void)
    /// Liveness check; replies with the agent's libunlatch version.
    func ping(reply: @escaping (String) -> Void)
}

/// Exported by clients (the extension) for replies: one complete `IpcFrame<IpcResponse>` frame.
@objc(UnlatchClientXPC)
public protocol UnlatchClientXPC {
    func deliver(_ frame: Data, fileHandle: FileHandle?)
}

public enum XPCInterfaces {
    public static func engine() -> NSXPCInterface { NSXPCInterface(with: UnlatchEngineXPC.self) }
    public static func client() -> NSXPCInterface { NSXPCInterface(with: UnlatchClientXPC.self) }
}
