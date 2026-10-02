//! Fault injection (shared test contract): `UNLATCH_FAULT` is a comma-separated token list read
//! once. unlatchd honours:
//!
//! * `die_after_commit:<op>` (op ∈ write|mkdir|symlink|rename|remove|setattr): exit(99) right
//!   after the op's result is durable in the ops table, before the Response;
//! * `overflow_after_events:<n>`: once the inotify reader has read `n` events, it drops every
//!   further event it read in that pass and reports `IN_Q_OVERFLOW` instead, exactly what the
//!   kernel does when its queue is full (one-shot per process; the sysctl needs root).

use std::sync::OnceLock;

fn tokens() -> &'static [String] {
    static T: OnceLock<Vec<String>> = OnceLock::new();
    T.get_or_init(|| {
        std::env::var("UNLATCH_FAULT")
            .map(|v| {
                v.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    })
}

pub fn has(token: &str) -> bool {
    tokens().iter().any(|t| t == token)
}

/// The value of a `name:<value>` token, if present.
pub fn value(name: &str) -> Option<&'static str> {
    tokens().iter().find_map(|t| {
        t.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix(':'))
            .map(|v| v.trim())
    })
}

/// `overflow_after_events:<n>`.
pub fn overflow_after_events() -> Option<u64> {
    value("overflow_after_events").and_then(|v| v.parse().ok())
}

/// Called after the op record is fsync'd.
pub fn after_commit(op: &str) {
    if has(&format!("die_after_commit:{op}")) {
        crate::log!("UNLATCH_FAULT die_after_commit:{op} → exit(99)");
        std::process::exit(99);
    }
}
