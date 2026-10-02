//! Failing-first proof for every fileproviderd rule fpsim models: each scenario must pass against
//! the correct scripted engine AND be caught against a scripted engine that breaks exactly the
//! rule the scenario defends. If the second assertion fails, fpsim does not model the measured
//! behaviour (a naive engine would sail through), which is the whole point of the simulator.

use unlatch_bench::fpsim::scenarios::{run_scripted_pair, SCENARIOS};

fn check(name: &str) {
    let info = SCENARIOS
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no scenario {name}"));
    let r = run_scripted_pair(info).unwrap_or_else(|e| panic!("{name}: harness error {e:#}"));
    assert!(
        r.correct.pass(),
        "{name} ({}): the correct engine must pass; skipped={:?} problems={:#?}",
        info.rule,
        r.correct.skipped,
        r.correct.problems
    );
    assert!(
        !r.broken.problems.is_empty(),
        "{name} ({}): the engine that breaks the rule was NOT caught — fpsim does not model {}",
        info.rule,
        info.what
    );
}

macro_rules! scenario_tests {
    ($($test:ident => $name:literal,)*) => {
        $(
            #[test]
            fn $test() {
                check($name);
            }
        )*

        #[test]
        fn every_scenario_has_a_test() {
            let tested = [$($name),*];
            for s in SCENARIOS {
                assert!(tested.contains(&s.name), "scenario {} has no test", s.name);
            }
        }
    };
}

scenario_tests! {
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
