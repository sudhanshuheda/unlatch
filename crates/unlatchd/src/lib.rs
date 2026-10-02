//! `unlatchd`: the Unlatch VM-side daemon (see `docs/DESIGN.md` as amended by
//! `docs/review/2026-09-30-design-review.md` §2(d)).

pub mod config;
pub mod core;
pub mod fault;
pub mod index;
pub mod lifecycle;
pub mod log;
pub mod ops;
pub mod outbox;
pub mod persist;
pub mod reconcile;
pub mod session;
pub mod sys;
pub mod watch;
