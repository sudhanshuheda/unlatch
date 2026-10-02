import Foundation
import os

/// Accepts XPC connections on the agent's MachService, checking each peer's code signature.
final class AgentListener: NSObject, NSXPCListenerDelegate {
    private let agent: EngineAgent

    init(agent: EngineAgent) {
        self.agent = agent
    }

    func listener(_ listener: NSXPCListener, shouldAcceptNewConnection connection: NSXPCConnection) -> Bool {
        // Only Unlatch's own UI and extension (same team, allow-listed ids) may drive the VM
        // through this agent's authenticated ssh session (review D11).
        connection.setCodeSigningRequirement(agent.config.agentClientRequirement)
        connection.exportedInterface = XPCInterfaces.engine()
        connection.remoteObjectInterface = XPCInterfaces.client()
        let session = ClientSession(agent: agent, connection: connection)
        connection.exportedObject = session
        connection.invalidationHandler = { session.shutdown() }
        connection.resume()
        return true
    }
}

enum AgentMain {
    private static var retained: [AnyObject] = []

    static func run() -> Never {
        let log = Logger(subsystem: "unlatch", category: "agent")
        let config: BundleConfig
        let agent: EngineAgent
        do {
            config = try BundleConfig.from(bundle: .main)
            agent = try EngineAgent(config: config)
        } catch {
            // launchd would restart us in a tight loop (KeepAlive); fail slowly and loudly.
            log.fault("agent cannot start: \(String(describing: error), privacy: .public)")
            sleep(30)
            exit(78) // EX_CONFIG
        }
        let listener = NSXPCListener(machServiceName: config.machServiceName)
        let delegate = AgentListener(agent: agent)
        listener.delegate = delegate
        agent.start()
        listener.resume()
        retained = [agent, listener, delegate]
        log.info("agent listening on \(config.machServiceName, privacy: .public)")
        while true {
            RunLoop.main.run()
        }
    }
}
