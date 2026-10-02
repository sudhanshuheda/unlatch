//! Every fpsim scenario end to end: fpsim → `unlatch_core::ipc::IpcClient` → `Engine::serve_ipc`
//! → `unlatchd connect`/`serve` → a real directory. Runs by default under `cargo test`:
//!
//! * `unlatchd` comes from `$UNLATCHD_BIN`, or is built from this checkout into the test's own
//!   target dir (`unlatch_bench::fpsim::e2e::unlatchd_for_tests`); if neither works every test
//!   fails with instructions — never a silent pass;
//! * engine-side faults (`die_before_ipc_reply`) run the engine in `unlatch-bench fpsim
//!   engine-host`, which cargo builds for this test (`CARGO_BIN_EXE_unlatch-bench`), so the
//!   replay scenarios never skip here.
//!
//! ```text
//! cargo test -p unlatch-bench --test fpsim_e2e                          # debug unlatchd, built on demand
//! UNLATCHD_BIN=$PWD/target/release/unlatchd cargo test -p unlatch-bench --test fpsim_e2e
//! FPSIM_VERBOSE=1 cargo test -p unlatch-bench --test fpsim_e2e -- --exact mq016_newcomer_displaces_existing
//! FPSIM_SLOW_HOST_MS=20 cargo test -p unlatch-bench --test fpsim_e2e   # host slow to take events
//! ```

use std::path::PathBuf;
use std::sync::OnceLock;
use unlatch_bench::fpsim::e2e::{run_scenario, unlatchd_for_tests, E2eConfig};
use unlatch_bench::fpsim::scenarios::SCENARIOS;

/// Scenarios that cannot run against the real engine, with the reason. Anything else that
/// reports `skipped` fails.
const LEGIT_SKIPS: &[(&str, &str)] = &[(
    "mq005_failing_enumeration_throttled_until_error_resolved",
    "the real engine answers ChangesSince from its replica while offline, so no enumeration \
     fails and the MQ-005 throttle cannot be entered; the scripted world covers it",
)];

fn config() -> E2eConfig {
    static UNLATCHD: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    let unlatchd = UNLATCHD
        .get_or_init(unlatchd_for_tests)
        .clone()
        .unwrap_or_else(|e| panic!("fpsim e2e needs an unlatchd binary: {e}"));
    E2eConfig::discover(
        Some(unlatchd),
        Some(PathBuf::from(env!("CARGO_BIN_EXE_unlatch-bench"))),
        std::env::var_os("FPSIM_VERBOSE").is_some(),
    )
    .unwrap_or_else(|e| panic!("fpsim e2e config: {e:#}"))
}

fn check(name: &str) {
    let v = run_scenario(&config(), name).unwrap_or_else(|e| panic!("{name}: harness error {e:#}"));
    if let Some(why) = &v.skipped {
        match LEGIT_SKIPS.iter().find(|(n, _)| *n == name) {
            Some((_, reason)) => {
                eprintln!("{name}: skipped: {why} ({reason})");
                return;
            }
            None => panic!("{name}: skipped against the real engine: {why}"),
        }
    }
    assert!(v.problems.is_empty(), "{name}: {:#?}", v.problems);
}

macro_rules! e2e_tests {
    ($($test:ident => $name:literal,)*) => {
        $(
            #[test]
            fn $test() {
                check($name);
            }
        )*

        #[test]
        fn every_scenario_has_an_e2e_test() {
            let tested = [$($name),*];
            for s in SCENARIOS {
                assert!(tested.contains(&s.name), "scenario {} has no e2e test", s.name);
            }
            for (n, _) in LEGIT_SKIPS {
                assert!(tested.contains(n), "LEGIT_SKIPS names unknown scenario {n}");
            }
        }
    };
}

e2e_tests! {
    mq001_folder_enumerated_once => "mq001_folder_enumerated_once",
    mq004_empty_change_set_at_held_anchor => "mq004_empty_change_set_at_held_anchor",
    mq005_failing_enumeration_throttled => "mq005_failing_enumeration_throttled_until_error_resolved",
    mq006_expired_anchor_resumes_fresh => "mq006_expired_anchor_resumes_fresh_without_rescan",
    mq009_trash_enumerator_answer => "mq009_trash_enumerator_answer",
    mq011_item_not_found_deletes_local_file => "mq011_item_not_found_deletes_local_file",
    mq013_returned_version_is_believed => "mq013_returned_version_is_believed",
    mq013_rename_reply_with_newer_content => "mq013_rename_reply_with_newer_content",
    mq014_create_collision_retried_forever => "mq014_create_collision_retried_forever",
    mq016_case_collision_renamed_locally => "mq016_case_collision_renamed_locally",
    mq016_newcomer_displaces_existing => "mq016_newcomer_displaces_existing_display_name",
    mq035_write_retried_with_same_op => "mq035_write_retried_with_same_op",
    mq037_only_error_resolved_flushes_writes => "mq037_only_error_resolved_flushes_writes",
    mq049_atomic_save_is_one_modify => "mq049_atomic_save_is_one_modify",
    mq080_pending_edit_on_deleted_item_recreated => "mq080_pending_edit_on_deleted_item_recreated",
    replay_keeps_template_id => "replay_keeps_template_id",
    replay_reply_reflects_later_edit => "replay_reply_reflects_later_edit",
    replay_of_since_deleted_item => "replay_of_since_deleted_item",
    replayed_create_carries_newer_content => "replayed_create_carries_newer_content",
    second_save_after_lost_reply => "second_save_after_lost_reply",
    dir_stays_until_children_deleted => "dir_stays_until_children_deleted",
    tombstones_follow_reported_dir => "tombstones_follow_reported_dir",
    symlink_rule_follows_ancestor_moves => "symlink_rule_follows_ancestor_moves",
    evict_on_update_goes_dataless => "evict_on_update_goes_dataless",
    rule2_create_of_identical_file_merges => "rule2_create_of_identical_file_merges",
    rule4_concurrent_rename_answers_server_state => "rule4_concurrent_rename_answers_server_state",
    rule5_tags_stay_on_the_mac => "rule5_tags_stay_on_the_mac",
    rule6_delete_keeps_unseen_agent_file => "rule6_delete_keeps_unseen_agent_file",
    rule6_retried_delete_keeps_unseen_agent_file => "rule6_retried_delete_keeps_unseen_agent_file",
    rule10_exec_bit_hidden_by_default => "rule10_exec_bit_hidden_by_default",
    rule11_moved_twin_keeps_real_name => "rule11_moved_twin_keeps_real_name",
    rule8_mass_deletion_waits_for_the_user => "rule8_mass_deletion_waits_for_the_user",
}

/// Model checks (no failing-first engine flaw: they pin fpsim's own model) end to end.
#[test]
fn model_check_recreated_folder_takes_numbered_name() {
    check("rule2_recreated_folder_takes_numbered_name");
}

/// Review (c)7 end to end: unlatchd's index is lost mid-session with pending Mac edits.
#[test]
fn e2e_check_index_change_reimports() {
    check("rule7_index_change_reimports");
}

#[test]
fn every_model_and_e2e_check_has_an_e2e_test() {
    use unlatch_bench::fpsim::scenarios::{E2E_CHECKS, MODEL_CHECKS};
    assert_eq!(MODEL_CHECKS, ["rule2_recreated_folder_takes_numbered_name"]);
    assert_eq!(E2E_CHECKS, ["rule7_index_change_reimports"]);
}
