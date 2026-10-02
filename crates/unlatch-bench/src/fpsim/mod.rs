//! **fpsim** — a model of macOS `fileproviderd` that drives the Unlatch engine through exactly the
//! IPC the Swift shim forwards (`unlatch_core::ipc::IpcClient` over `Engine::serve_ipc`).
//!
//! What it models (see `docs/TESTING.md` for the full list and what it does *not* cover):
//! * the local disk: a case- and normalization-insensitive namespace (APFS), dataless vs
//!   materialized files, an item database keyed by provider identifier with versions;
//! * the measured fileproviderd rules MQ-001/004/005/006/009/010/011/013/014/015/016/029/035/036/
//!   037/046/047/048/049/080 (ids from `docs/review/sshdrive-macos-quirks.md`), plus
//!   replay-with-same-template-id, "a directory stays until its children are deleted" and
//!   evict-on-remote-update;
//! * signal-driven working-set enumeration fed by the engine's `EventHandler`
//!   (`WorkingSetChanged` / `ErrorResolved` / `Reimport`), `materializedItemsDidChange`, and user
//!   actions (browse, open, edit, save, rename, move, delete, mkdir, drag in, chmod, tags).
//!
//! Time is virtual, so a 47-minute throttle costs nothing.
//!
//! Layout: [`sim`] (the state machine), [`disk`] (local volume + item db), [`names`] (APFS folding
//! and Finder/engine renames), [`backend`] (the IPC hop), [`check`] (invariants), [`vmfs`] (agent
//! side), [`scripted`] (a flaw-injectable fake engine for testing fpsim itself), [`world`] +
//! [`scenarios`] (one failing-first scenario per rule), [`e2e`] (real engine + `unlatchd`),
//! [`host`] (out-of-process engine for engine-side fault injection), [`rawipc`] (a minimal
//! frame-level IPC client/server used to test the codec path against the scripted engine).

pub mod backend;
pub mod check;
pub mod disk;
pub mod e2e;
pub mod host;
pub mod names;
pub mod rawipc;
pub mod scenarios;
pub mod scripted;
pub mod sim;
pub mod vmfs;
pub mod world;

pub use backend::{event_channel, Backend, IpcBackend};
pub use sim::{ActionError, FpSim, PumpReport, SimConfig, TrashAnswer, VisibleItem};
pub use world::{ScriptedWorld, World};

use crate::cli::Args;
use anyhow::Result;
use std::path::PathBuf;

const USAGE: &str = "usage: unlatch-bench fpsim [--scripted] [--unlatchd PATH] [--engine-host PATH]
                          [--only NAME[,NAME..]] [--verbose]
       unlatch-bench fpsim list
       unlatch-bench fpsim engine-host --socket S --root R --state D --argv ARG [--argv ARG ..]
  Default: run every MQ scenario end-to-end against the real engine + unlatchd (needs --unlatchd
  or $UNLATCHD_BIN). --scripted: run each scenario against the scripted engine twice — correct
  (must pass) and with the defect the rule guards against (must fail).
  --seeds/--seed/--ops are accepted for symmetry with `fuzz` and ignored (scenarios are fixed).";

/// Entry point for `unlatch-bench fpsim`. Returns the process exit code.
pub fn run_cli(args: &[String]) -> Result<i32> {
    if args.first().map(String::as_str) == Some("engine-host") {
        return host::main(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("list") {
        for s in scenarios::SCENARIOS {
            println!("{:<60} {:<14} {}", s.name, s.rule, s.what);
        }
        return Ok(0);
    }
    let mut a = Args::new(args);
    if a.flag("help") || a.flag("h") {
        println!("{USAGE}");
        return Ok(0);
    }
    let scripted = a.flag("scripted");
    let verbose = a.flag("verbose");
    let unlatchd = a.opt("unlatchd")?.map(PathBuf::from);
    let engine_host = a.opt("engine-host")?.map(PathBuf::from);
    let only: Option<Vec<String>> = a
        .opt("only")?
        .map(|s| s.split(',').map(str::to_string).collect());
    let _ = (a.opt("seeds")?, a.opt("seed")?, a.opt("ops")?);
    let rest = a.finish()?;
    if !rest.is_empty() {
        eprintln!("{USAGE}");
        return Ok(2);
    }
    let selected: Vec<&scenarios::ScenarioInfo> = scenarios::SCENARIOS
        .iter()
        .filter(|s| only.as_ref().is_none_or(|o| o.iter().any(|n| n == s.name)))
        .collect();
    if scripted {
        return run_scripted(&selected, verbose);
    }
    let cfg = e2e::E2eConfig::discover(unlatchd, engine_host, verbose)?;
    let mut failed = 0;
    for s in &selected {
        let v = match e2e::run_scenario(&cfg, s.name) {
            Ok(v) => v,
            Err(e) => scenarios::Verdict {
                problems: vec![format!("harness error: {e:#}")],
                skipped: None,
            },
        };
        match (&v.skipped, v.problems.is_empty()) {
            (Some(why), _) => println!("SKIP {:<60} {why}", s.name),
            (None, true) => println!("PASS {:<60} {}", s.name, s.rule),
            (None, false) => {
                failed += 1;
                println!("FAIL {:<60} {}", s.name, s.rule);
                for p in &v.problems {
                    println!("       {p}");
                }
            }
        }
    }
    println!("fpsim e2e: {} scenarios, {failed} failed", selected.len());
    Ok(i32::from(failed > 0))
}

fn run_scripted(selected: &[&scenarios::ScenarioInfo], verbose: bool) -> Result<i32> {
    let mut failed = 0;
    for s in selected {
        let r = scenarios::run_scripted_pair(s)?;
        if r.ok() {
            println!(
                "PASS {:<60} {:<14} (broken engine caught: {} problems)",
                r.name,
                r.rule,
                r.broken.problems.len()
            );
        } else {
            failed += 1;
            println!("FAIL {:<60} {}", r.name, r.rule);
            if !r.correct.pass() {
                println!(
                    "       correct engine did not pass: {:?} {:?}",
                    r.correct.skipped, r.correct.problems
                );
            }
            if r.broken.problems.is_empty() {
                println!("       broken engine was NOT caught (model does not defend the rule)");
            }
        }
        if verbose {
            for p in &r.broken.problems {
                println!("       broken: {p}");
            }
        }
    }
    println!(
        "fpsim scripted: {} scenarios, {failed} failed",
        selected.len()
    );
    Ok(i32::from(failed > 0))
}
