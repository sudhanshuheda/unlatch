//! The fuzzer against the scripted engine: its machinery (generation, execution, invariants,
//! shrinking, the crash/replay matrix at the IPC hop, the targeted races) must pass on a correct
//! engine and catch deliberately broken ones.

use unlatch_bench::fpsim::scripted::Flaws;
use unlatch_bench::fuzz::matrix::{Hop, Mutation};
use unlatch_bench::fuzz::ops::{generate, Op, Profile};
use unlatch_bench::fuzz::{fuzz_seed, run_matrix, run_once, run_specials, Target};

const SCRIPTED: Profile = Profile {
    faults: true,
    kill_unlatchd: false,
    real_fs: false,
};

#[test]
fn random_interleavings_converge_on_a_correct_engine() {
    let target = Target::Scripted(Flaws::default());
    for seed in 1..=25 {
        let f = fuzz_seed(&target, seed, 200, 40, false).expect("run");
        if let Some(f) = f {
            panic!(
                "seed {seed} failed: {:#?}\nminimal: {:#?}",
                f.first.problems, f.minimal
            );
        }
    }
}

#[test]
fn crash_replay_matrix_ipc_hop() {
    let target = Target::Scripted(Flaws::default());
    for (name, v) in run_matrix(&target) {
        let v = v.unwrap_or_else(|e| panic!("{name}: {e:#}"));
        if name.contains("/Wire/") {
            assert!(
                v.skipped.is_some(),
                "{name}: the scripted world has no wire hop"
            );
            continue;
        }
        assert!(v.pass(), "{name}: {:?} {:#?}", v.skipped, v.problems);
    }
}

#[test]
fn crash_replay_matrix_catches_a_non_idempotent_engine() {
    let target = Target::Scripted(Flaws {
        non_idempotent: true,
        ..Flaws::default()
    });
    let caught: Vec<String> = run_matrix(&target)
        .into_iter()
        .filter_map(|(name, v)| v.ok().filter(|v| !v.problems.is_empty()).map(|_| name))
        .collect();
    // Creates duplicate and modifies conflict with themselves when replays execute twice.
    assert!(caught.iter().any(|n| n.ends_with("/Create")), "{caught:?}");
    assert!(caught.iter().any(|n| n.ends_with("/Modify")), "{caught:?}");
}

#[test]
fn targeted_races_pass_where_the_scripted_world_can_run_them() {
    let target = Target::Scripted(Flaws::default());
    for (name, v) in run_specials(&target) {
        let v = v.unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert!(
            v.skipped.is_some() || v.problems.is_empty(),
            "{name}: {:#?}",
            v.problems
        );
    }
}

#[test]
fn fuzzer_catches_broken_engines_by_random_search() {
    // Flaws the random op mix reaches quickly; each must be caught within 12 seeds.
    for flaw in [
        "signal_before_commit",
        "ws_only_materialized_ids",
        "dir_modify_unsupported",
        "no_display_mapping",
    ] {
        let target = Target::Scripted(Flaws::only(flaw).expect("known flaw"));
        let caught = (1..=12).any(|seed| {
            let ops = generate(seed, 200, SCRIPTED);
            run_once(&target, &ops, false)
                .map(|o| o.failed_at.is_some())
                .unwrap_or(true)
        });
        assert!(caught, "the fuzzer did not catch {flaw}");
    }
}

#[test]
fn failures_shrink_to_a_short_sequence() {
    let target = Target::Scripted(Flaws::only("dir_modify_unsupported").expect("flaw"));
    let f = (1..=12)
        .find_map(|seed| fuzz_seed(&target, seed, 200, 200, false).expect("run"))
        .expect("some seed fails");
    assert!(f.minimal.len() <= 6, "not shrunk: {:#?}", f.minimal);
    assert!(
        f.minimal.iter().any(|o| matches!(o, Op::MacSave { .. })),
        "{:#?}",
        f.minimal
    );
    assert!(!f.minimal_problems.is_empty());
}

#[test]
fn matrix_covers_every_mutation_at_both_hops() {
    let cells = unlatch_bench::fuzz::matrix::matrix_cases();
    for m in Mutation::ALL {
        for hop in [Hop::Ipc, Hop::Wire] {
            assert!(cells.contains(&(m, hop)), "{m:?} at {hop:?}");
        }
    }
}
