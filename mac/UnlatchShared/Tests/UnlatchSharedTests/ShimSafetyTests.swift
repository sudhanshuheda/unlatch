import Foundation
import XCTest
@testable import UnlatchShared

/// Portable (macOS and Linux) tests for the shim's safety edges: time conversion at the ends of
/// the `i64` range, and decoding the agent's `domains.json`.
final class ShimSafetyTests: XCTestCase {
    // MARK: TimeCoding

    func testTimes() {
        for ns: Int64 in [0, 1, 1_500_000_000_250_000_000, -5_000_000_000] {
            let back = TimeCoding.ns(TimeCoding.date(ns: ns))
            XCTAssertLessThan(abs(back - ns), 1_000, "\(ns)")
        }
    }

    /// unlatchd saturates out-of-range VM times to `i64::MAX`/`MIN` (sys.rs `ts_ns`); a Finder
    /// copy of such a file sends the date back through `ns`, which used to trap (crash loop).
    func testTimesSaturateInsteadOfTrapping() {
        XCTAssertEqual(TimeCoding.ns(TimeCoding.date(ns: .max)), .max)
        XCTAssertEqual(TimeCoding.ns(TimeCoding.date(ns: .min)), .min)
        XCTAssertEqual(TimeCoding.ns(.distantFuture), .max)
        XCTAssertEqual(TimeCoding.ns(.distantPast), .min)
        // Year 2300 (beyond 2262-04-11) and 1600 (before 1677-09-21).
        XCTAssertEqual(TimeCoding.ns(Date(timeIntervalSince1970: 10_413_792_000)), .max)
        XCTAssertEqual(TimeCoding.ns(Date(timeIntervalSince1970: -11_676_096_000)), .min)
        XCTAssertEqual(TimeCoding.ns(Date(timeIntervalSince1970: .infinity)), .max)
        XCTAssertEqual(TimeCoding.ns(Date(timeIntervalSince1970: -.infinity)), .min)
        XCTAssertEqual(TimeCoding.ns(Date(timeIntervalSince1970: .nan)), 0)
        // Just inside the range still converts (to within float precision).
        let edge = TimeCoding.ns(Date(timeIntervalSince1970: 9_223_372_035.5))
        XCTAssertLessThan(abs(edge - 9_223_372_035_500_000_000), 2_000)
    }

    // MARK: domains.json

    private func json(_ s: String) -> Data { Data(s.utf8) }

    /// A record written before `useShellAgent`/`exposeExec`/`registered` existed (or by a
    /// version that dropped one) must still load, with the defaults.
    func testDomainRecordDecodesWithMissingFlags() throws {
        let r = try JSONDecoder().decode(DomainRecord.self, from: json(#"{"id":"dev-1","displayName":"dev","destination":"me@dev","remoteRoot":"~"}"#))
        XCTAssertEqual(r, DomainRecord(id: "dev-1", displayName: "dev", destination: "me@dev", remoteRoot: "~"))
        let full = DomainRecord(id: "b-2", displayName: "b", destination: "b", port: 2222, identityFile: "/k", remoteRoot: "/srv",
                                useShellAgent: true, exposeExec: true, registered: true)
        XCTAssertEqual(try JSONDecoder().decode(DomainRecord.self, from: try JSONEncoder().encode(full)), full)
    }

    /// One unreadable record must not drop the others, and the caller must learn that the file
    /// was damaged (so it keeps a copy before the next save overwrites it).
    func testDomainListKeepsGoodRecordsAndReportsDamage() {
        let good = #"{"id":"a-1","displayName":"a","destination":"a","remoteRoot":"~","useShellAgent":false,"exposeExec":false,"registered":true}"#
        let old = #"{"id":"b-2","displayName":"b","destination":"b","remoteRoot":"/srv"}"#
        let bad = #"{"id":"c-3","displayName":"c"}"#
        let dup = #"{"id":"a-1","displayName":"other","destination":"x","remoteRoot":"/x"}"#

        let clean = DomainRecordList.decode(json("[\(good),\(old)]"))
        XCTAssertEqual(clean.records.map(\.id), ["a-1", "b-2"])
        XCTAssertFalse(clean.damaged)
        XCTAssertTrue(clean.records[0].registered)

        let mixed = DomainRecordList.decode(json("[\(good),\(bad),\(old),\(dup),42]"))
        XCTAssertEqual(mixed.records.map(\.id), ["a-1", "b-2"])
        XCTAssertEqual(mixed.records[0].displayName, "a", "the first record with an id wins")
        XCTAssertTrue(mixed.damaged)

        for garbage in ["", "{", #"{"id":"a"}"#, "null", #"[{"id":"a-1""#] {
            let d = DomainRecordList.decode(json(garbage))
            XCTAssertEqual(d.records, [], garbage)
            XCTAssertTrue(d.damaged, garbage)
        }
        XCTAssertFalse(DomainRecordList.decode(json("[]")).damaged)
    }

    // MARK: agent calls with a deadline (MQ-063: a mach service that never answers)

    func testDeadlineResumesACallThatNeverReplies() async {
        var timedOut = false
        let start = Date()
        do {
            let _: Int = try await CallDeadline.run(seconds: 0.2, onTimeout: { timedOut = true }) { _ in }
            XCTFail("no reply must not succeed")
        } catch let e as CallDeadline.TimedOut {
            XCTAssertEqual(e.seconds, 0.2)
        } catch {
            XCTFail("unexpected \(error)")
        }
        XCTAssertLessThan(Date().timeIntervalSince(start), 5)
        XCTAssertTrue(timedOut, "onTimeout runs before the caller resumes")
    }

    func testDeadlinePassesRepliesThroughOnceAndLateRepliesAreIgnored() async throws {
        let v: Int = try await CallDeadline.run(seconds: 5, onTimeout: { XCTFail("no timeout") }) { done in
            done(.success(7))
            done(.success(8))
            done(.failure(CallDeadline.TimedOut(seconds: 1)))
        }
        XCTAssertEqual(v, 7)
        struct Boom: Error {}
        do {
            let _: Int = try await CallDeadline.run(seconds: 5) { done in
                DispatchQueue.global().async { done(.failure(Boom())) }
            }
            XCTFail("error must propagate")
        } catch is Boom {}
        // A reply after the deadline fired is dropped (no double resume).
        var late: ((Result<Int, Error>) -> Void)?
        do {
            let _: Int = try await CallDeadline.run(seconds: 0.1) { done in late = done }
            XCTFail("must time out")
        } catch is CallDeadline.TimedOut {}
        late?(.success(1))
    }

    // MARK: agent health (MQ-062/063: SMAppService says .enabled while the agent is dead)

    func testUnansweredAgentBecomesRepairableEvenWhileEnabled() {
        var h = AgentHealth()
        XCTAssertEqual(h.state(registration: .enabled, answered: true), .enabled)
        // One miss is a cold start or a blip; two in a row is a dead agent.
        XCTAssertEqual(h.state(registration: .enabled, answered: false, failure: "timed out"), .enabled)
        guard case let .failed(reason) = h.state(registration: .enabled, answered: false, failure: "timed out") else {
            return XCTFail("a dead .enabled agent must offer Repair")
        }
        XCTAssertTrue(reason.contains("timed out"), reason)
        XCTAssertEqual(h.state(registration: .enabled, answered: true), .enabled, "an answer clears it")
    }

    func testRegistrationIsReReadOnEveryRefresh() {
        var h = AgentHealth()
        XCTAssertEqual(h.state(registration: .requiresApproval, answered: false), .requiresApproval)
        XCTAssertEqual(h.state(registration: .requiresApproval, answered: false), .requiresApproval)
        // Approved in Login Items: the banner goes away without relaunching, and the misses while
        // it waited for approval do not count against the agent.
        XCTAssertEqual(h.state(registration: .enabled, answered: false, failure: "cold start"), .enabled)
        XCTAssertEqual(h.state(registration: .enabled, answered: true), .enabled)
        // Unregistered behind our back (`unregister-agent`, a removed bundle): offer Repair.
        if case .failed = h.state(registration: .notRegistered, answered: false) {} else { XCTFail("must offer Repair") }
        if case .failed = h.state(registration: .notFound, answered: false) {} else { XCTFail("must offer Repair") }
        var fresh = AgentHealth()
        XCTAssertEqual(fresh.state(registration: .unknown, answered: true), .unknown)
    }
}
