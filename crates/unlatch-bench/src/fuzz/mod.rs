//! **fuzz** — seeded random interleavings of agent operations on the VM root and user operations
//! through fpsim, with fault injection, checked for convergence and for "no VM-side write is
//! ever lost", plus the deterministic crash/replay matrix (review §2(f)3) and targeted races
//! (§2(f)4). A failing seed is shrunk by delta debugging and printed with its minimal op list.
//!
//! Layout: [`ops`] (op model + generator), [`exec`] (executing ops, invariant sweep),
//! [`tracker`] (write-loss / conflict bookkeeping), [`shrink`] (ddmin), [`matrix`] (fixed
//! fault scenarios).

pub mod exec;
pub mod matrix;
pub mod ops;
pub mod shrink;
pub mod tracker;

use crate::cli::Args;
use crate::fpsim::e2e::{E2eConfig, RealWorld};
use crate::fpsim::scenarios::Verdict;
use crate::fpsim::scripted::Flaws;
use crate::fpsim::world::ScriptedWorld;
use anyhow::Result;
use exec::{run_ops, RunOutcome};
use ops::{generate, Op, Profile};
use std::path::PathBuf;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

const USAGE: &str = "usage: unlatch-bench fuzz [--seeds N | --seed S] [--ops N] [--scripted]
                         [--unlatchd PATH] [--engine-host PATH] [--verbose]
                         [--no-random] [--no-matrix] [--shrink-runs N] [--timeout SECS]
                         [--replay MINIMAL.json] [--scripted-flaw NAME] [--only NAME]
  Random interleavings (seeds 1..=N, default 10, 150 ops each), then the crash/replay matrix
  and the targeted race scenarios. Real engine + unlatchd by default (needs --unlatchd or
  $UNLATCHD_BIN); --scripted runs everything against the scripted engine (no faults on the
  wire hop); --scripted-flaw NAME breaks one engine rule on purpose (mutation testing: the run
  is expected to FAIL). --only NAME runs just the matrix/race cells whose name contains NAME
  (no random seeds). Exit code 1 on any failure; failures print the seed and shrunk ops.";

/// Which provider the fuzzer talks to.
#[derive(Clone, Debug)]
pub enum Target {
    /// The scripted engine, optionally with deliberate flaws (mutation testing).
    Scripted(Flaws),
    Real(E2eConfig),
}

impl Target {
    fn profile(&self) -> Profile {
        match self {
            Target::Scripted(_) => Profile {
                faults: true,
                kill_unlatchd: false,
                real_fs: false,
            },
            Target::Real(_) => Profile {
                faults: true,
                kill_unlatchd: true,
                real_fs: true,
            },
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Target::Scripted(_) => " --scripted",
            Target::Real(_) => "",
        }
    }
}

/// Kills the process if a single run hangs (an IPC call cannot be interrupted from outside).
struct Watchdog {
    cancel: Option<Sender<()>>,
}

impl Watchdog {
    fn arm(label: String, limit: Duration) -> Watchdog {
        let (tx, rx) = channel::<()>();
        let spawned = std::thread::Builder::new()
            .name("fuzz-watchdog".into())
            .spawn(move || {
                if let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(limit) {
                    eprintln!("FUZZ HANG: {label} exceeded {}s; aborting", limit.as_secs());
                    std::process::exit(3);
                }
            });
        Watchdog {
            cancel: spawned.ok().map(|_| tx),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
    }
}

fn new_world<T>(target: &Target, f: &mut dyn FnMut(WorldRef<'_>) -> Result<T>) -> Result<T> {
    match target {
        Target::Scripted(flaws) => {
            let mut w = ScriptedWorld::new(flaws.clone())?;
            f(WorldRef::Scripted(&mut w))
        }
        Target::Real(cfg) => {
            let mut w = RealWorld::start(cfg)?;
            f(WorldRef::Real(&mut w))
        }
    }
}

/// A borrowed world of either kind (generic code is instantiated per kind).
pub enum WorldRef<'a> {
    Scripted(&'a mut ScriptedWorld),
    Real(&'a mut RealWorld),
}

macro_rules! with_world {
    ($wr:expr, $w:ident => $body:expr) => {
        match $wr {
            WorldRef::Scripted($w) => $body,
            WorldRef::Real($w) => $body,
        }
    };
}

/// Run one op list in a fresh world.
pub fn run_once(target: &Target, ops: &[Op], verbose: bool) -> Result<RunOutcome> {
    new_world(target, &mut |wr| match wr {
        WorldRef::Scripted(w) => run_ops(w, ops, verbose),
        WorldRef::Real(w) => {
            let out = run_ops(w, ops, verbose)?;
            if verbose && out.failed_at.is_some() {
                // Keep the world (root, state dirs, unlatchd.log) for a post-mortem.
                eprintln!("world kept at {}", w.dir_keep().display());
            }
            Ok(out)
        }
    })
}

/// Run seed `seed` with `n` ops; on failure shrink (≤ `shrink_runs` re-runs).
pub fn fuzz_seed(
    target: &Target,
    seed: u64,
    n: usize,
    shrink_runs: usize,
    verbose: bool,
) -> Result<Option<SeedFailure>> {
    let ops = generate(seed, n, target.profile());
    let first = run_once(target, &ops, verbose)?;
    if first.failed_at.is_none() {
        return Ok(None);
    }
    let (minimal, minimal_problems) = shrink_failure(target, &ops, &first, shrink_runs)?;
    Ok(Some(SeedFailure {
        seed,
        ops: n,
        first,
        minimal,
        minimal_problems,
    }))
}

/// ddmin a failing op list (≤ `shrink_runs` re-runs) towards the *same* failure; returns the
/// minimal list and the problems of one more run of it.
pub fn shrink_failure(
    target: &Target,
    ops: &[Op],
    first: &RunOutcome,
    shrink_runs: usize,
) -> Result<(Vec<Op>, Vec<String>)> {
    let at = first.failed_at.unwrap_or(ops.len());
    // Only the prefix up to the failing check matters.
    let prefix: Vec<Op> = ops[..at.min(ops.len())].to_vec();
    // Shrink towards the *same* failure: a sub-sequence that fails differently (another bug)
    // does not count, or the minimal sequence drifts to whichever failure is easiest to hit.
    let want: Vec<String> = first.problems.iter().map(|p| problem_class(p)).collect();
    let mut still_fails = |sub: &[Op]| match run_once(target, sub, false) {
        Ok(o) => {
            o.failed_at.is_some()
                && (want.is_empty() || o.problems.iter().any(|p| want.contains(&problem_class(p))))
        }
        Err(_) => false,
    };
    let minimal = shrink::ddmin(&prefix, &mut still_fails, shrink_runs);
    let min_run = run_once(target, &minimal, false)?;
    Ok((minimal, min_run.problems))
}

/// The kind of an invariant violation, without the directories and numbers that vary between
/// runs: `"docs/x: on the VM, missing on the Mac"` → `"x: on the VM, missing on the Mac"`,
/// `"agent write lost: 3234 bytes …"` → `"agent write lost"`.
pub fn problem_class(p: &str) -> String {
    let text = match p.split_once(": ") {
        // "<path>: <what>" — a path has a '/' or '.', or is a single word. Keep its last
        // component (op names are fixed strings; the directories they land in are not).
        Some((head, tail)) if head.contains('/') || head.contains('.') || !head.contains(' ') => {
            let base = head.rsplit('/').next().unwrap_or(head);
            format!("{base}: {tail}")
        }
        Some((head, _)) => head.to_string(),
        None => p.to_string(),
    };
    let mut out = String::with_capacity(text.len());
    let mut in_digits = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('#');
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct SeedFailure {
    pub seed: u64,
    pub ops: usize,
    pub first: RunOutcome,
    pub minimal: Vec<Op>,
    pub minimal_problems: Vec<String>,
}

fn print_failure(f: &SeedFailure, target: &Target) {
    println!(
        "FAIL seed {} (failed at op #{:?} of {})",
        f.seed, f.first.failed_at, f.ops
    );
    for p in &f.first.problems {
        println!("       {p}");
    }
    println!("     minimal sequence ({} ops):", f.minimal.len());
    for (i, op) in f.minimal.iter().enumerate() {
        println!("       {i:>3}: {op}");
    }
    if !f.minimal_problems.is_empty() {
        println!("     minimal run problems:");
        for p in &f.minimal_problems {
            println!("       {p}");
        }
    }
    println!(
        "     reproduce: unlatch-bench fuzz --seed {} --ops {}{}",
        f.seed,
        f.ops,
        target.label()
    );
    // $UNLATCH_TMP when set: /tmp is shared by every fuzz run on the box.
    let dir = std::env::var_os("UNLATCH_TMP")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let file = dir.join(format!("unlatch-fuzz-seed{}-minimal.json", f.seed));
    match serde_json::to_vec_pretty(&f.minimal)
        .map_err(anyhow::Error::from)
        .and_then(|b| Ok(std::fs::write(&file, b)?))
    {
        Ok(()) => println!(
            "     replay minimal: unlatch-bench fuzz --replay {}{}",
            file.display(),
            target.label()
        ),
        Err(e) => println!("     (could not save the minimal sequence: {e})"),
    }
}

/// Crash/replay matrix; returns (name, verdict) per cell.
pub fn run_matrix(target: &Target) -> Vec<(String, Result<Verdict>)> {
    run_matrix_only(target, "")
}

/// [`run_matrix`] restricted to cells whose name contains `only`.
pub fn run_matrix_only(target: &Target, only: &str) -> Vec<(String, Result<Verdict>)> {
    matrix::matrix_cases()
        .into_iter()
        .map(|(m, hop)| (format!("replay/{hop:?}/{m:?}"), m, hop))
        .filter(|(name, _, _)| name.contains(only))
        .map(|(name, m, hop)| {
            let v = new_world(
                target,
                &mut |wr| with_world!(wr, w => matrix::run_case(w, m, hop)),
            );
            (name, v)
        })
        .collect()
}

/// Targeted race scenarios; returns (name, verdict).
pub fn run_specials(target: &Target) -> Vec<(String, Result<Verdict>)> {
    run_specials_only(target, "")
}

/// [`run_specials`] restricted to scenarios whose name contains `only`.
pub fn run_specials_only(target: &Target, only: &str) -> Vec<(String, Result<Verdict>)> {
    matrix::SPECIAL
        .iter()
        .filter(|name| format!("race/{name}").contains(only))
        .map(|name| {
            let v = new_world(
                target,
                &mut |wr| with_world!(wr, w => matrix::run_special(name, w)),
            );
            (format!("race/{name}"), v)
        })
        .collect()
}

fn report(results: Vec<(String, Result<Verdict>)>) -> usize {
    let mut failed = 0;
    for (name, v) in results {
        match v {
            Ok(v) if v.skipped.is_some() => {
                println!("SKIP {name:<52} {}", v.skipped.unwrap_or_default())
            }
            Ok(v) if v.problems.is_empty() => println!("PASS {name}"),
            Ok(v) => {
                failed += 1;
                println!("FAIL {name}");
                for p in v.problems {
                    println!("       {p}");
                }
            }
            Err(e) => {
                failed += 1;
                println!("FAIL {name}: harness error: {e:#}");
            }
        }
    }
    failed
}

/// Entry point for `unlatch-bench fuzz`. Returns the process exit code.
pub fn run_cli(args: &[String]) -> Result<i32> {
    let mut a = Args::new(args);
    if a.flag("help") || a.flag("h") {
        println!("{USAGE}");
        return Ok(0);
    }
    let verbose = a.flag("verbose");
    let scripted = a.flag("scripted");
    let no_random = a.flag("no-random");
    let no_matrix = a.flag("no-matrix");
    let seeds: u64 = a
        .opt("seeds")?
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(10);
    let seed: Option<u64> = a.opt("seed")?.map(|s| s.parse()).transpose()?;
    let n_ops: usize = a.opt("ops")?.map(|s| s.parse()).transpose()?.unwrap_or(150);
    let shrink_opt: Option<usize> = a.opt("shrink-runs")?.map(|s| s.parse()).transpose()?;
    let shrink_given = shrink_opt.is_some();
    let shrink_runs = shrink_opt.unwrap_or(60);
    let timeout = Duration::from_secs(
        a.opt("timeout")?
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(900),
    );
    let unlatchd = a.opt("unlatchd")?.map(PathBuf::from);
    let engine_host = a.opt("engine-host")?.map(PathBuf::from);
    let replay = a.opt("replay")?.map(PathBuf::from);
    let flaw = a.opt("scripted-flaw")?;
    let only = a.opt("only")?;
    let rest = a.finish()?;
    // The fuzzer cuts the link on purpose: engine ops then answer Offline after 5 s instead of
    // the default 20 s (inherited by engine-host children; an explicit setting wins).
    if std::env::var_os("UNLATCH_E2E_LIST_TIMEOUT_MS").is_none() {
        std::env::set_var("UNLATCH_E2E_LIST_TIMEOUT_MS", "5000");
    }
    if !rest.is_empty() {
        eprintln!("{USAGE}");
        return Ok(2);
    }
    let target = match (&flaw, scripted) {
        (Some(name), _) => match Flaws::only(name) {
            Some(f) => Target::Scripted(f),
            None => {
                eprintln!("unknown flaw {name:?}; known: {}", Flaws::NAMES.join(", "));
                return Ok(2);
            }
        },
        (None, true) => Target::Scripted(Flaws::default()),
        (None, false) => Target::Real(E2eConfig::discover(unlatchd, engine_host, verbose)?),
    };
    if let Some(file) = replay {
        let ops: Vec<Op> = serde_json::from_slice(&std::fs::read(&file)?)?;
        let out = run_once(&target, &ops, true)?;
        match out.failed_at {
            None => println!("PASS replay {} ({} ops)", file.display(), ops.len()),
            Some(at) => {
                println!("FAIL replay {} at op #{at}", file.display());
                for p in &out.problems {
                    println!("       {p}");
                }
                if shrink_given {
                    // --replay F --shrink-runs N: shrink the replayed sequence further.
                    let (min, probs) = shrink_failure(&target, &ops, &out, shrink_runs)?;
                    let dest = file.with_extension("shrunk.json");
                    std::fs::write(&dest, serde_json::to_vec_pretty(&min)?)?;
                    println!("     shrunk to {} ops: {}", min.len(), dest.display());
                    for (i, op) in min.iter().enumerate() {
                        println!("       {i:>3}: {op}");
                    }
                    for p in probs {
                        println!("       {p}");
                    }
                }
            }
        }
        return Ok(i32::from(out.failed_at.is_some()));
    }
    let mut failed = 0usize;
    if let Some(only) = &only {
        let _wd = Watchdog::arm(format!("--only {only}"), timeout);
        failed += report(run_matrix_only(&target, only));
        failed += report(run_specials_only(&target, only));
        println!("fuzz: {failed} failures");
        return Ok(i32::from(failed > 0));
    }
    if !no_random {
        let list: Vec<u64> = match seed {
            Some(s) => vec![s],
            None => (1..=seeds).collect(),
        };
        for s in list {
            let started = Instant::now();
            let _wd = Watchdog::arm(format!("seed {s}"), timeout);
            match fuzz_seed(&target, s, n_ops, shrink_runs, verbose) {
                Ok(None) => println!(
                    "PASS seed {s} ({n_ops} ops, {:.1}s)",
                    started.elapsed().as_secs_f64()
                ),
                Ok(Some(f)) => {
                    failed += 1;
                    print_failure(&f, &target);
                }
                Err(e) => {
                    failed += 1;
                    println!("FAIL seed {s}: harness error: {e:#}");
                }
            }
        }
    }
    if !no_matrix && seed.is_none() {
        let _wd = Watchdog::arm("crash/replay matrix".into(), timeout);
        failed += report(run_matrix(&target));
        failed += report(run_specials(&target));
    }
    println!("fuzz: {failed} failures");
    Ok(i32::from(failed > 0))
}

#[cfg(test)]
mod tests {
    use super::problem_class;

    #[test]
    fn problem_classes_ignore_paths_and_numbers() {
        assert_eq!(
            problem_class("docs.old.old/docs/node_modules/x: on the VM, missing on the Mac"),
            problem_class("x: on the VM, missing on the Mac")
        );
        assert_eq!(
            problem_class("src/a 2.txt: shown on the Mac, missing on the VM"),
            "a #.txt: shown on the Mac, missing on the VM"
        );
        assert_ne!(
            problem_class("docs/node_modules/x: on the VM, missing on the Mac"),
            problem_class("café.md: on the VM, missing on the Mac")
        );
        assert_eq!(
            problem_class("agent write lost: 3234 bytes written to café.md by op #9 (epoch 1)"),
            "agent write lost"
        );
        assert_ne!(
            problem_class("b.txt: materialized content differs from the VM (2226 vs 502 bytes)"),
            problem_class("b.txt: on the VM, missing on the Mac")
        );
    }
}
