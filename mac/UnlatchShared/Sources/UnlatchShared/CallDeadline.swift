import Foundation

/// Bridges one reply-or-error callback call to async/await with a deadline. An XPC call to a
/// mach service that accepts the connection and never answers (MQ-063) gets neither a reply nor
/// an error; without a deadline the caller's continuation, and the reply block, are never freed.
public enum CallDeadline {
    public struct TimedOut: Error, CustomStringConvertible, Equatable {
        public let seconds: TimeInterval
        public init(seconds: TimeInterval) { self.seconds = seconds }
        public var description: String { "no reply within \(Int(seconds.rounded(.up))) s" }
    }

    /// Resumes exactly once: with the first result `body` reports, or with `TimedOut` after
    /// `seconds` (then `onTimeout` runs, e.g. to drop the connection, and later results are
    /// ignored).
    public static func run<T>(
        seconds: TimeInterval,
        onTimeout: @escaping () -> Void = {},
        _ body: @escaping (@escaping (Result<T, Error>) -> Void) -> Void
    ) async throws -> T {
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<T, Error>) in
            let once = ResumeOnce()
            DispatchQueue.global().asyncAfter(deadline: .now() + seconds) {
                guard once.claim() else { return }
                onTimeout()
                cont.resume(throwing: TimedOut(seconds: seconds))
            }
            body { result in
                if once.claim() { cont.resume(with: result) }
            }
        }
    }
}

/// Lock-protected, so safe to share with the timer and the reply.
final class ResumeOnce: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false

    func claim() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if done { return false }
        done = true
        return true
    }
}
