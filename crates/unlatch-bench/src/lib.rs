//! Unlatch performance harness and verification tooling.
//!
//! * [`netlab`] — kernel-shaped network lab (unprivileged netns + `tc netem`), stdio bridge,
//!   per-connection server spawner, link calibration.
//! * [`sshlab`] — real `ssh` ⇄ `sshd -i` around the shaped link (`-ssh` profiles).
//! * [`tree`] — deterministic synthetic VM trees.
//! * [`scenarios`] — T1..T17 as `(target, system)` scenarios, each run in a subprocess.
//! * [`runner`], [`report`], [`targets`], [`measure`] — orchestration, JSON/scorecard,
//!   target evaluation, regression comparison.
//! * [`wire_client`] — minimal blocking wire client for daemon-level scenarios.
//! * [`fpsim`], [`fuzz`] — File Provider simulator and correctness fuzz.

pub mod fpsim;
pub mod fuzz;

pub mod cli;
pub mod measure;
pub mod netlab;
pub mod report;
pub mod runner;
pub mod scenarios;
pub mod sshlab;
pub mod targets;
pub mod tree;
pub mod wire_client;
