//! Performance targets (DESIGN §6 as amended by review §2(a)6 and §2(f)7) and their evaluation.
//!
//! Only Unlatch rows are judged; baselines keep `pass = None`. Rules that depend on other systems
//! (T3 vs local/sshfs, T8 vs raw `cat`) read the peer rows of the same profile.

use crate::measure::{Measurement, Status, System};
use crate::netlab::Profile;

/// Metric names shared by the scenarios, the targets and the scorecard.
pub mod metric {
    pub const LIST_1000_P50_MS: &str = "list_1000_p50_ms";
    pub const STAT_P50_US: &str = "stat_p50_us";
    pub const LS_LA_MS: &str = "ls_la_ms";
    pub const LS_LA_COLD_MS: &str = "ls_la_cold_ms";
    pub const VISIBLE_P50_MS: &str = "visible_p50_ms";
    pub const VISIBLE_EVENT_P50_MS: &str = "visible_event_p50_ms";
    pub const VISIBLE_READDIR_P50_MS: &str = "visible_readdir_p50_ms";
    pub const BURST_ALL_VISIBLE_MS: &str = "burst_all_visible_ms";
    pub const OPEN_SMALL_WARM_P50_MS: &str = "open_small_warm_p50_ms";
    pub const OPEN_4K_COLD_MS: &str = "open_4k_cold_ms";
    pub const OPEN_256K_COLD_MS: &str = "open_256k_cold_ms";
    pub const OPEN_4K_COLD_IDLE_MS: &str = "open_4k_cold_idle_ms";
    pub const OPEN_256K_COLD_IDLE_MS: &str = "open_256k_cold_idle_ms";
    pub const THROUGHPUT_MBIT: &str = "throughput_mbit";
    pub const UPLOAD_4K_P50_MS: &str = "upload_4k_p50_ms";
    pub const RECONNECT_CATCHUP_MS: &str = "reconnect_catchup_ms";
    pub const FULL_TREE_KNOWN_S: &str = "full_tree_known_s";
    pub const INITIAL_SYNC_COLD_DAEMON_S: &str = "initial_sync_cold_daemon_s";
    pub const INITIAL_SYNC_BYTES: &str = "initial_sync_bytes";
    pub const RSS_BYTES_PER_ENTRY: &str = "daemon_rss_bytes_per_entry";
    pub const P99_INTERACTIVE_UNDER_LOAD_MS: &str = "p99_interactive_under_load_ms";
    pub const P99_PONG_UNDER_LOAD_MS: &str = "p99_pong_under_load_ms";
    pub const P99_LISTDIR_UNDER_LOAD_MS: &str = "p99_listdir_under_load_ms";
    pub const BYTES_MOVED_RATIO: &str = "bytes_moved_ratio";
    pub const LISTDIR_ENTRIES_SENT: &str = "listdir_entries_sent";
    pub const WATCHES_ADDED: &str = "watches_added";
    pub const RESTART_WELCOME_MS: &str = "restart_welcome_ms";
    pub const RESTART_SNAPSHOT_BYTES: &str = "restart_snapshot_bytes";
    pub const REWRITE_NEW_VERSION_PCT: &str = "same_size_rewrite_new_version_pct";
    pub const RTT_P50_MS: &str = "rtt_p50_ms";
    pub const DOWN_MBIT: &str = "down_mbit";
    pub const UP_MBIT: &str = "up_mbit";
    pub const FETCH_256K_WARM_MS: &str = "fetch_256k_warm_ms";
    pub const FETCH_256K_IDLE_MS: &str = "fetch_256k_idle_ms";
}

/// TCP initial window after idle (IW10 × 1460 B), review §2(a)6 T7 formula.
const IW_BYTES: f64 = 14.6 * 1024.0;

/// Target for `(id, metric)` at `profile`: (description, judge). The judge sees the row value
/// and the profile's peer rows and returns `None` when the target does not apply here.
fn rule(
    m: &Measurement,
    profile: &Profile,
    peers: &[Measurement],
) -> Option<(String, Option<bool>)> {
    use metric::*;
    let v = m.value?;
    let rtt = f64::from(profile.rtt_ms);
    let peer = |system: System, metric: &str| {
        peers
            .iter()
            .find(|p| {
                p.system == system && p.metric == metric && p.id == m.id && p.status == Status::Ok
            })
            .and_then(|p| p.value)
    };
    let max = |limit: f64, text: String| Some((text, Some(v <= limit)));
    match (m.id.as_str(), m.metric.as_str()) {
        ("T1", LIST_1000_P50_MS) => max(0.5, "≤ 0.5 ms (engine API)".into()),
        ("T2", STAT_P50_US) => max(20.0, "≤ 20 µs (engine API)".into()),
        ("T3", LS_LA_MS) => {
            let local = peer(System::Local, LS_LA_MS);
            let sshfs = peer(System::Sshfs, LS_LA_COLD_MS);
            let mut text = "≤ 2× local".to_string();
            let mut ok = local.map(|l| v <= 2.0 * l);
            if profile.rtt_ms == 40 {
                text.push_str("; ≥ 10× faster than sshfs (cold) at RTT 40");
                let s = sshfs.map(|s| s >= 10.0 * v);
                ok = match (ok, s) {
                    (Some(a), Some(b)) => Some(a && b),
                    (a, b) => a.or(b),
                };
            }
            Some((text, ok))
        }
        ("T4", VISIBLE_P50_MS) | ("T4", VISIBLE_READDIR_P50_MS) | ("T4", VISIBLE_EVENT_P50_MS) => {
            let limit = rtt / 2.0 + 15.0;
            max(limit, format!("≤ RTT/2 + 15 ms = {limit:.0} ms"))
        }
        ("T5", BURST_ALL_VISIBLE_MS) => {
            let limit = 300.0 + rtt;
            max(limit, format!("≤ 300 ms + RTT = {limit:.0} ms"))
        }
        ("T6", OPEN_SMALL_WARM_P50_MS) => max(1.0, "≤ 1 ms (prefetched, engine API)".into()),
        ("T7", OPEN_4K_COLD_MS) | ("T7", OPEN_4K_COLD_IDLE_MS) => {
            let limit = rtt + 5.0;
            max(limit, format!("≤ 1 RTT + 5 ms = {limit:.0} ms"))
        }
        ("T7", OPEN_256K_COLD_MS) | ("T7", OPEN_256K_COLD_IDLE_MS) => {
            let size = 256.0 * 1024.0;
            let rounds = 1.0 + (size / IW_BYTES).log2().ceil();
            let ser_ms = profile
                .rate_bytes_per_sec()
                .map(|bps| size / bps as f64 * 1e3)
                .unwrap_or(0.0);
            let limit = rounds * rtt + ser_ms + 5.0;
            max(
                limit,
                format!("≤ (1+⌈log2(size/14.6KiB)⌉)·RTT + size/bw (+5 ms) = {limit:.0} ms"),
            )
        }
        ("T8", THROUGHPUT_MBIT) => {
            let raw = peer(System::Raw, THROUGHPUT_MBIT);
            Some((
                "≥ 80% of raw `cat` over the same link".into(),
                raw.map(|r| v >= 0.8 * r),
            ))
        }
        ("T9", UPLOAD_4K_P50_MS) => {
            let limit = rtt + 5.0;
            max(limit, format!("≤ 1 RTT + 5 ms = {limit:.0} ms"))
        }
        ("T10", RECONNECT_CATCHUP_MS) => {
            if profile.rtt_ms <= 40 {
                max(1500.0, "≤ 1.5 s (RTT ≤ 40 ms)".into())
            } else {
                Some(("≤ 1.5 s at RTT 40 ms (informational here)".into(), None))
            }
        }
        ("T11", FULL_TREE_KNOWN_S) => {
            if profile.rtt_ms <= 40 {
                max(3.0, "≤ 3 s at RTT 40 ms (100k + 30k lazy)".into())
            } else {
                Some(("≤ 3 s at RTT 40 ms (informational here)".into(), None))
            }
        }
        ("T12", RSS_BYTES_PER_ENTRY) => max(250.0, "≤ 250 B/entry".into()),
        ("T13", P99_INTERACTIVE_UNDER_LOAD_MS)
        | ("T13", P99_PONG_UNDER_LOAD_MS)
        | ("T13", P99_LISTDIR_UNDER_LOAD_MS) => {
            let limit = rtt + 30.0;
            max(limit, format!("≤ RTT + 30 ms = {limit:.0} ms"))
        }
        ("T14", BYTES_MOVED_RATIO) => max(2.0, "≤ 2× appended bytes".into()),
        ("T16", RESTART_WELCOME_MS) => max(200.0, "≤ 200 ms, Resume".into()),
        ("T16", RESTART_SNAPSHOT_BYTES) => max(0.0, "0 snapshot bytes".into()),
        ("T17", REWRITE_NEW_VERSION_PCT) => {
            Some(("100% new content version".into(), Some(v >= 100.0)))
        }
        _ => None,
    }
}

/// Fill `target`/`pass` on every Unlatch row of `rows` (all rows of one profile).
pub fn evaluate(rows: &mut [Measurement], profile: &Profile) {
    let peers = rows.to_vec();
    for m in rows.iter_mut() {
        if m.system != System::Unlatch {
            continue;
        }
        // Scenario-judged rows (T15) keep their own verdict.
        if m.target.is_some() && m.pass.is_some() {
            continue;
        }
        if m.status != Status::Ok {
            if m.target.is_none() {
                m.target = describe(&m.id, &m.metric, profile);
            }
            m.pass = None;
            continue;
        }
        if let Some((text, pass)) = rule(m, profile, &peers) {
            m.target = Some(text);
            m.pass = pass;
        }
    }
}

/// Target text without a value (for rows that could not be measured).
fn describe(id: &str, metric: &str, profile: &Profile) -> Option<String> {
    let mut probe = Measurement::new(
        id,
        metric,
        System::Unlatch,
        &profile.name,
        "",
        crate::measure::Better::Lower,
    );
    probe.value = Some(0.0);
    rule(&probe, profile, &[]).map(|(t, _)| t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measure::Better;

    fn row(id: &str, metric: &str, sys: System, v: f64) -> Measurement {
        Measurement::new(id, metric, sys, "rtt40-bw50", "ms", Better::Lower).value(v, 1)
    }

    #[test]
    fn absolute_and_rtt_targets() {
        let p = Profile::parse("rtt40-bw50").unwrap();
        let mut rows = vec![
            row("T1", metric::LIST_1000_P50_MS, System::Unlatch, 0.4),
            row("T4", metric::VISIBLE_P50_MS, System::Unlatch, 36.0),
            row("T9", metric::UPLOAD_4K_P50_MS, System::Unlatch, 44.0),
            row("T9", metric::UPLOAD_4K_P50_MS, System::Sshfs, 400.0),
        ];
        evaluate(&mut rows, &p);
        assert_eq!(rows[0].pass, Some(true));
        assert_eq!(rows[1].pass, Some(false)); // 36 > 20 + 15
        assert_eq!(rows[2].pass, Some(true));
        assert_eq!(rows[3].pass, None);
        assert!(rows[1].target.as_ref().unwrap().contains("35 ms"));
    }

    #[test]
    fn relative_targets() {
        let p = Profile::parse("rtt40-bw50").unwrap();
        let mut rows = vec![
            row("T3", metric::LS_LA_MS, System::Unlatch, 5.0),
            row("T3", metric::LS_LA_MS, System::Local, 3.0),
            row("T3", metric::LS_LA_COLD_MS, System::Sshfs, 400.0),
            row("T8", metric::THROUGHPUT_MBIT, System::Unlatch, 30.0),
            row("T8", metric::THROUGHPUT_MBIT, System::Raw, 40.0),
        ];
        evaluate(&mut rows, &p);
        assert_eq!(rows[0].pass, Some(true));
        assert_eq!(rows[3].pass, Some(false)); // 30 < 32
        rows[2].value = Some(40.0);
        rows[0].pass = None;
        rows[0].target = None;
        evaluate(&mut rows, &p);
        assert_eq!(rows[0].pass, Some(false)); // sshfs only 8× slower
    }

    #[test]
    fn t7_formula() {
        let p = Profile::parse("rtt40-bw50").unwrap();
        let mut rows = vec![row(
            "T7",
            metric::OPEN_256K_COLD_IDLE_MS,
            System::Unlatch,
            250.0,
        )];
        evaluate(&mut rows, &p);
        // 1 + ceil(log2(256/14.6)) = 6 rounds → 240 ms + 42 ms serialization + 5.
        assert!(
            rows[0].target.as_ref().unwrap().contains("287 ms"),
            "{:?}",
            rows[0].target
        );
        assert_eq!(rows[0].pass, Some(true));
    }

    #[test]
    fn unavailable_rows_get_target_text_only() {
        let p = Profile::parse("rtt40-bw50").unwrap();
        let mut rows = vec![Measurement::new(
            "T2",
            metric::STAT_P50_US,
            System::Unlatch,
            "rtt40-bw50",
            "us",
            Better::Lower,
        )
        .status(Status::Unavailable, "engine not built")];
        evaluate(&mut rows, &p);
        assert_eq!(rows[0].pass, None);
        assert!(rows[0].target.is_some());
    }
}
