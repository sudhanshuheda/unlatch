import Foundation

/// Nanoseconds since the Unix epoch (the engine's `i64` times) <-> `Date`.
public enum TimeCoding {
    public static func date(ns: Int64) -> Date {
        let seconds = ns / 1_000_000_000
        let rest = ns % 1_000_000_000
        return Date(timeIntervalSince1970: TimeInterval(seconds) + TimeInterval(rest) / 1e9)
    }

    /// Total: a date outside the `i64` nanosecond range (before 1677-09-21 or after 2262-04-11,
    /// e.g. a VM file whose time unlatchd saturated to `i64::MAX`, or `.distantFuture`) saturates
    /// to `Int64.min`/`Int64.max` like unlatchd's `ts_ns`, and NaN is 0. This runs in the File
    /// Provider extension on every create/modify; a trap there is a crash loop (MQ-035).
    public static func ns(_ date: Date) -> Int64 {
        let t = date.timeIntervalSince1970
        if t.isNaN { return 0 }
        // Keeps `Int64(seconds)` below from trapping; the exact edge is found by the overflow
        // checks (|t| here is within ~10x of the representable range).
        guard t > -1e11, t < 1e11 else { return t < 0 ? .min : .max }
        let seconds = t.rounded(.down)
        let frac = Int64(((t - seconds) * 1e9).rounded())
        let (whole, mulOverflow) = Int64(seconds).multipliedReportingOverflow(by: 1_000_000_000)
        if mulOverflow { return seconds < 0 ? .min : .max }
        let (total, addOverflow) = whole.addingReportingOverflow(frac)
        if addOverflow { return frac < 0 ? .min : .max }
        return total
    }
}
