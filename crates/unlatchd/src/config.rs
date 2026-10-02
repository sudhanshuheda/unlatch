//! Environment knobs (shared test contract).

use std::time::Duration;

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Adaptive inotify coalescing (DESIGN §4). A batch closes once the queue has been quiet for
    /// `settle` (`UNLATCHD_SETTLE_US`, default 2 ms, at most `debounce`): a lone change goes out
    /// after ~`settle`, since one write's CREATE/MODIFY/CLOSE_WRITE arrive within microseconds.
    /// While events keep arriving (a burst) the batch keeps growing: `settle` steps until
    /// `debounce` (`UNLATCHD_DEBOUNCE_MS`, default 8 ms), then `debounce` steps, up to
    /// `debounce_max` (50 ms).
    pub settle: Duration,
    pub debounce: Duration,
    pub debounce_max: Duration,
    /// Configured watch cap (`UNLATCHD_MAX_WATCHES`); the effective budget is
    /// `min(this, 50% of the per-uid free watches)` (D14).
    pub max_watches: Option<u64>,
    /// `UNLATCHD_POLL=1`: poll every directory instead of using inotify.
    pub force_poll: bool,
    /// Exit a background server after this long without clients (24 h).
    pub idle_exit: Duration,
    /// Collapse lazy expansions nobody listed for this long (30 min).
    pub collapse_after: Duration,
    /// Parallel verify/scan walker threads (16–64, D17).
    pub walkers: usize,
    /// Tombstone GC horizon (30 days).
    pub tomb_horizon_secs: u64,
    /// Tombstone count cap (`UNLATCHD_TOMB_MAX`, default 1M, as the engine's journal): beyond it
    /// the oldest are dropped and clients resuming from before them get a Snapshot.
    pub tomb_max: usize,
    /// Hot-file publish throttle (§2(d)6): at most one content update per `hot_interval`
    /// (1 s) while a file keeps changing, the last one after `hot_quiet` (300 ms) of quiet.
    pub hot_interval: Duration,
    pub hot_quiet: Duration,
    /// Deferred full stat audit (see `Core::process_events`): runs once events have been quiet
    /// for `audit_quiet` (300 ms), at the latest `audit_max_delay` (5 s, the engine's liveness
    /// Ping) after it was first due, and at every barrier.
    pub audit_quiet: Duration,
    pub audit_max_delay: Duration,
}

impl Config {
    pub fn from_env() -> Config {
        let debounce = Duration::from_millis(env_u64("UNLATCHD_DEBOUNCE_MS").unwrap_or(8));
        // musl's allocator serializes on one lock: past ~16 walkers the scan gets slower.
        let cap = if cfg!(target_env = "musl") { 16 } else { 64 };
        let walkers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(16, cap);
        let settle = env_u64("UNLATCHD_SETTLE_US")
            .map(Duration::from_micros)
            .unwrap_or(Duration::from_millis(2))
            .min(debounce);
        Config {
            settle,
            debounce,
            debounce_max: debounce.max(Duration::from_millis(50)),
            max_watches: env_u64("UNLATCHD_MAX_WATCHES"),
            force_poll: std::env::var("UNLATCHD_POLL")
                .map(|v| v == "1")
                .unwrap_or(false),
            idle_exit: Duration::from_secs(env_u64("UNLATCHD_IDLE_EXIT_SECS").unwrap_or(24 * 3600)),
            collapse_after: Duration::from_secs(
                env_u64("UNLATCHD_COLLAPSE_SECS").unwrap_or(30 * 60),
            ),
            walkers,
            tomb_horizon_secs: 30 * 24 * 3600,
            tomb_max: env_u64("UNLATCHD_TOMB_MAX").unwrap_or(1_000_000) as usize,
            hot_interval: Duration::from_secs(1),
            hot_quiet: Duration::from_millis(300),
            audit_quiet: Duration::from_millis(300),
            audit_max_delay: Duration::from_secs(5),
        }
    }
}
