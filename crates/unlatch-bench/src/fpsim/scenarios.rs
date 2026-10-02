//! One scenario per measured fileproviderd rule (review §2(f)2). Each runs against any
//! [`World`]. Against [`ScriptedWorld`] every scenario is run twice: with a correct engine (must
//! pass) and with the one engine flaw the rule defends against (must fail) — the failing-first
//! proof that fpsim actually models the behaviour.

use super::check::compare_trees;
use super::names::{is_conflict_copy, numbered_base};
use super::scripted::{Flaws, ReplyFault};
use super::sim::TrashAnswer;
use super::vmfs::VmKind;
use super::world::{ScriptedWorld, World, QUIESCE_BUDGET};
use anyhow::{anyhow, Result};
use std::time::Duration;
use unlatch_proto::ipc::fields;
use unlatch_proto::ItemId;

/// Outcome of one scenario run: `problems` empty = the invariants held.
#[derive(Clone, Debug, Default)]
pub struct Verdict {
    pub problems: Vec<String>,
    /// Set when the world cannot run this scenario (with the reason).
    pub skipped: Option<String>,
}

impl Verdict {
    pub fn pass(&self) -> bool {
        self.problems.is_empty() && self.skipped.is_none()
    }

    fn skip(why: &str) -> Verdict {
        Verdict {
            problems: Vec::new(),
            skipped: Some(why.to_string()),
        }
    }
}

/// How the failing-first variant breaks the world.
#[derive(Clone, Copy, Debug)]
pub enum Breakage {
    /// Engine flaw (scripted engine).
    Flaw(fn(&mut Flaws)),
    /// The shim answers the trash enumerator `.noSuchItem` (MQ-009).
    TrashNoSuchItem,
}

pub struct ScenarioInfo {
    pub name: &'static str,
    pub rule: &'static str,
    pub what: &'static str,
    /// Flaws every run of this scenario needs in the scripted world (setup, not the defect).
    pub base: fn(&mut Flaws),
    pub breakage: Breakage,
}

fn nothing(_: &mut Flaws) {}

pub const SCENARIOS: &[ScenarioInfo] = &[
    ScenarioInfo {
        name: "mq001_folder_enumerated_once",
        rule: "MQ-001",
        what: "a folder is enumerated once; later VM changes inside it arrive only via the working set",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.ws_only_materialized_ids = true),
    },
    ScenarioInfo {
        name: "mq004_empty_change_set_at_held_anchor",
        rule: "MQ-004",
        what: "an empty change set at the held anchor drops the change until the next signal",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.signal_before_commit = true),
    },
    ScenarioInfo {
        name: "mq005_failing_enumeration_throttled_until_error_resolved",
        rule: "MQ-005",
        what: "failing change enumerations back off (47 min at 27 failures) until ErrorResolved",
        base: |f| f.changes_fail_offline = true,
        breakage: Breakage::Flaw(|f| f.no_error_resolved = true),
    },
    ScenarioInfo {
        name: "mq006_expired_anchor_resumes_fresh_without_rescan",
        rule: "MQ-006",
        what: "AnchorExpired makes the system continue from a fresh anchor with no re-enumeration",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.expire_on_restart = true),
    },
    ScenarioInfo {
        name: "mq009_trash_enumerator_answer",
        rule: "MQ-009/MQ-010",
        what: "noSuchItem on the trash container loops at 1 Hz for ever; NSFeatureUnsupported stops after 2",
        base: nothing,
        breakage: Breakage::TrashNoSuchItem,
    },
    ScenarioInfo {
        name: "mq011_item_not_found_deletes_local_file",
        rule: "MQ-011",
        what: "noSuchItem from item(for:) deletes the local file",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.item_not_found_offline = true),
    },
    ScenarioInfo {
        name: "mq013_returned_version_is_believed",
        rule: "MQ-013",
        what: "the version in a modify reply is believed with the local bytes; conflicts need should_fetch_content",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.conflict_without_fetch = true),
    },
    ScenarioInfo {
        name: "mq013_rename_reply_with_newer_content",
        rule: "MQ-013",
        what: "a rename whose reply carries a newer content version must make the Mac re-fetch",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.metadata_reply_hides_content_change = true),
    },
    ScenarioInfo {
        name: "mq014_create_collision_retried_forever",
        rule: "MQ-014",
        what: "a create answered filenameCollision is retried for ever",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.create_returns_exists = true),
    },
    ScenarioInfo {
        name: "mq016_case_collision_renamed_locally",
        rule: "MQ-016",
        what: "a case-only collision from the server is renamed on the Mac only, with no call",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.no_display_mapping = true),
    },
    ScenarioInfo {
        name: "mq016_newcomer_displaces_existing_display_name",
        rule: "MQ-016/D16",
        what: "a VM newcomer that sorts first renames an existing twin's display name; the twin must be reported",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.no_display_rename_report = true),
    },
    ScenarioInfo {
        name: "mq035_write_retried_with_same_op",
        rule: "MQ-035",
        what: "a write whose reply was lost is re-offered for ever with the same base and bytes",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.non_idempotent = true),
    },
    ScenarioInfo {
        name: "mq037_only_error_resolved_flushes_writes",
        rule: "MQ-037",
        what: "only signalErrorResolved flushes a queued write; a working-set signal does not",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.no_error_resolved = true),
    },
    ScenarioInfo {
        name: "mq049_atomic_save_is_one_modify",
        rule: "MQ-049",
        what: "an atomic save is one modifyItem on the original id, after an mtime-only modify of the parent",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.dir_modify_unsupported = true),
    },
    ScenarioInfo {
        name: "mq080_pending_edit_on_deleted_item_recreated",
        rule: "MQ-080",
        what: "a pending edit on an item deleted remotely comes back as createItem",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.modify_missing_ok = true),
    },
    ScenarioInfo {
        name: "replay_keeps_template_id",
        rule: "D1",
        what: "a create whose reply was lost is replayed with the same template id; no duplicate",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.non_idempotent = true),
    },
    ScenarioInfo {
        name: "replay_reply_reflects_later_edit",
        rule: "D1/MQ-013",
        what: "a replayed create whose item was edited since is answered with the current version",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.stale_replay_reply = true),
    },
    ScenarioInfo {
        name: "replay_of_since_deleted_item",
        rule: "D1/MQ-080",
        what: "a replayed create whose item was deleted since leaves no phantom and nothing stuck",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.stale_replay_reply = true),
    },
    ScenarioInfo {
        name: "replayed_create_carries_newer_content",
        rule: "D1/MQ-013",
        what: "a create replayed after the user saved the new file again uploads the newer bytes",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.create_replay_ignores_new_content = true),
    },
    ScenarioInfo {
        name: "second_save_after_lost_reply",
        rule: "D1",
        what: "a save queued behind a modify whose reply was lost is not a conflict with itself",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.no_self_fastforward = true),
    },
    ScenarioInfo {
        name: "dir_stays_until_children_deleted",
        rule: "(a)5",
        what: "a directory reported deleted stays on the Mac until each child is reported deleted",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.dir_tombstone_only = true),
    },
    ScenarioInfo {
        name: "tombstones_follow_reported_dir",
        rule: "(a)5",
        what: "rm -rf of a never-enumerated folder holding an item the Mac received by a move leaves no phantom",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.tombstone_filter_strict = true),
    },
    ScenarioInfo {
        name: "symlink_rule_follows_ancestor_moves",
        rule: "D12",
        what: "moving a directory re-evaluates the in-root rule for symlinks below it (depth changed)",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.symlink_depth_not_reevaluated = true),
    },
    ScenarioInfo {
        name: "rule2_create_of_identical_file_merges",
        rule: "(c)2",
        what: "a Mac create where the VM already has the same bytes under that name is that file, not `x 2`",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.non_idempotent = true),
    },
    ScenarioInfo {
        name: "rule4_concurrent_rename_answers_server_state",
        rule: "(c)4",
        what: "a Finder rename racing an agent rename never errors: the reply carries the VM's name",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.rename_conflict_errors = true),
    },
    ScenarioInfo {
        name: "rule5_tags_stay_on_the_mac",
        rule: "(c)5/MQ-043",
        what: "a Finder tag is Mac-only (no VM change) and survives the item's next VM update",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.local_meta_not_merged = true),
    },
    ScenarioInfo {
        name: "rule6_delete_keeps_unseen_agent_file",
        rule: "(c)6",
        what: "deleting a folder in Finder never deletes an agent file the Mac has not seen yet",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.delete_ignores_seen_seq = true),
    },
    ScenarioInfo {
        name: "rule6_retried_delete_keeps_unseen_agent_file",
        rule: "(c)6",
        what: "a folder delete retried after a lost reply keeps the first attempt's seen_seq: the agent file it kept survives",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.delete_retry_widens_seen = true),
    },
    ScenarioInfo {
        name: "rule10_exec_bit_hidden_by_default",
        rule: "(c)10/D12",
        what: "a VM file with the owner-exec bit (a `.command` too) is not executable on the Mac",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.exec_bit_exposed = true),
    },
    ScenarioInfo {
        name: "rule11_moved_twin_keeps_real_name",
        rule: "(c)11",
        what: "moving a case twin shown as `x (Unlatch 2).ext` moves the real name on the VM",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.display_name_leaks_to_vm = true),
    },
    ScenarioInfo {
        name: "rule8_mass_deletion_waits_for_the_user",
        rule: "(c)8/MQ-080",
        what: "an agent rm -rf of most downloaded files pauses sync until the user confirms",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.no_mass_delete_guard = true),
    },
    ScenarioInfo {
        name: "evict_on_update_goes_dataless",
        rule: "D9",
        what: "a materialized file whose content version changes goes dataless",
        base: nothing,
        breakage: Breakage::Flaw(|f| f.stale_content_version = true),
    },
];

/// Run scenario `name` in `w`.
pub fn run<W: World>(name: &str, w: &mut W) -> Result<Verdict> {
    match name {
        "mq001_folder_enumerated_once" => mq001(w),
        "mq004_empty_change_set_at_held_anchor" => mq004(w),
        "mq005_failing_enumeration_throttled_until_error_resolved" => mq005(w),
        "mq006_expired_anchor_resumes_fresh_without_rescan" => mq006(w),
        "mq009_trash_enumerator_answer" => mq009(w),
        "mq011_item_not_found_deletes_local_file" => mq011(w),
        "mq013_returned_version_is_believed" => mq013(w),
        "mq013_rename_reply_with_newer_content" => mq013b(w),
        "mq014_create_collision_retried_forever" => mq014(w),
        "mq016_case_collision_renamed_locally" => mq016(w),
        "mq016_newcomer_displaces_existing_display_name" => mq016b(w),
        "mq035_write_retried_with_same_op" => mq035(w),
        "mq037_only_error_resolved_flushes_writes" => mq037(w),
        "mq049_atomic_save_is_one_modify" => mq049(w),
        "mq080_pending_edit_on_deleted_item_recreated" => mq080(w),
        "replay_keeps_template_id" => replay_template(w),
        "replay_reply_reflects_later_edit" => replay_after_edit(w),
        "replay_of_since_deleted_item" => replay_after_delete(w),
        "replayed_create_carries_newer_content" => replayed_create_newer(w),
        "second_save_after_lost_reply" => second_save(w),
        "dir_stays_until_children_deleted" => dir_stays(w),
        "tombstones_follow_reported_dir" => tombstones_follow_dir(w),
        "symlink_rule_follows_ancestor_moves" => symlink_depth(w),
        "evict_on_update_goes_dataless" => evict_on_update(w),
        "rule2_create_of_identical_file_merges" => rule2_identical_create(w),
        "rule2_recreated_folder_takes_numbered_name" => rule2_recreated_folder(w),
        "rule7_index_change_reimports" => rule7_index_change(w),
        "rule4_concurrent_rename_answers_server_state" => rule4_rename_race(w),
        "rule5_tags_stay_on_the_mac" => rule5_tags(w),
        "rule6_delete_keeps_unseen_agent_file" => rule6_delete_unseen(w),
        "rule6_retried_delete_keeps_unseen_agent_file" => rule6_retried_delete(w),
        "rule10_exec_bit_hidden_by_default" => rule10_exec_hidden(w),
        "rule11_moved_twin_keeps_real_name" => rule11_move_twin(w),
        "rule8_mass_deletion_waits_for_the_user" => rule8_mass_delete(w),
        other => Err(anyhow!("unknown scenario {other:?}")),
    }
}

/// Result of the correct/flawed pair against the scripted engine.
#[derive(Clone, Debug)]
pub struct PairResult {
    pub name: &'static str,
    pub rule: &'static str,
    pub correct: Verdict,
    pub broken: Verdict,
}

impl PairResult {
    /// The rule is defended: correct engine passes, the broken one is caught.
    pub fn ok(&self) -> bool {
        self.correct.pass() && !self.broken.problems.is_empty()
    }
}

/// Run one scenario against a correct and a broken scripted engine.
pub fn run_scripted_pair(info: &ScenarioInfo) -> Result<PairResult> {
    let mut base = Flaws::default();
    (info.base)(&mut base);
    let mut w = ScriptedWorld::new(base.clone())?;
    let correct = run(info.name, &mut w)?;
    let broken = match info.breakage {
        Breakage::Flaw(f) => {
            let mut flaws = base;
            f(&mut flaws);
            let mut w = ScriptedWorld::new(flaws)?;
            run(info.name, &mut w)?
        }
        Breakage::TrashNoSuchItem => {
            let mut w =
                ScriptedWorld::with_config(base, |c| c.trash_answer = TrashAnswer::NoSuchItem)?;
            run(info.name, &mut w)?
        }
    };
    Ok(PairResult {
        name: info.name,
        rule: info.rule,
        correct,
        broken,
    })
}

// ---- helpers ------------------------------------------------------------------------------

/// Create files (and their directories) on the VM, then start the Mac side and browse `dirs`.
fn setup<W: World>(w: &mut W, files: &[(&str, &[u8])], dirs: &[&str]) -> Result<()> {
    for (path, body) in files {
        let mut acc = String::new();
        let comps: Vec<&str> = path.split('/').collect();
        for c in &comps[..comps.len() - 1] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(c);
            if w.vm().kind(&acc).is_none() {
                w.vm().mkdir(&acc)?;
            }
        }
        if path.ends_with('/') {
            continue;
        }
        w.vm().write(path, body)?;
    }
    w.settle()?;
    w.sim()
        .add_domain()
        .map_err(|e| anyhow!("add_domain: {e}"))?;
    for d in dirs {
        w.sim().browse(d).map_err(|e| anyhow!("browse {d}: {e}"))?;
    }
    w.sync()
}

fn mkdirs<W: World>(w: &mut W, path: &str) -> Result<()> {
    let mut acc = String::new();
    for c in path.split('/').filter(|c| !c.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(c);
        if w.vm().kind(&acc).is_none() {
            w.vm().mkdir(&acc)?;
        }
    }
    Ok(())
}

fn finish<W: World>(w: &mut W, mut problems: Vec<String>) -> Result<Verdict> {
    problems.extend(w.converged()?);
    Ok(Verdict {
        problems,
        skipped: None,
    })
}

fn conflict_copies<W: World>(w: &mut W) -> Result<Vec<String>> {
    Ok(w.vm_tree()?
        .nodes
        .keys()
        .filter(|p| is_conflict_copy(p.rsplit('/').next().unwrap_or(p)))
        .cloned()
        .collect())
}

fn id_of<W: World>(w: &mut W, path: &str) -> Option<unlatch_proto::ItemId> {
    let k = w.sim().disk().resolve(path)?;
    w.sim().disk().get(k)?.id
}

// ---- scenarios ----------------------------------------------------------------------------

fn mq001<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/a.txt", b"a")], &["d"])?;
    let d = id_of(w, "d").ok_or_else(|| anyhow!("d has no id"))?;
    // Remote change inside an already-enumerated, still-open folder.
    w.vm().write("d/new.txt", b"new")?;
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    // Revisiting the folder never re-enumerates it (the model's side of MQ-001).
    w.sim().browse("d").map_err(|e| anyhow!("{e}"))?;
    let mut problems = Vec::new();
    let n = w.sim().stats.enumerations.get(&d).copied().unwrap_or(0);
    if n != 1 {
        problems.push(format!(
            "folder d enumerated {n} times (MQ-001: exactly once)"
        ));
    }
    // Without re-enumeration, the new file is visible only if the working set carried it:
    // compare *before* converged() browses anything else.
    let tree = w.vm_tree()?;
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    finish(w, problems)
}

fn mq004<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/a.txt", b"a"), ("d/b.txt", b"b")], &["d"])?;
    w.vm().remove_file("d/a.txt")?;
    // Anything the provider signals before committing is enumerated as "no changes".
    w.sim().pump(QUIESCE_BUDGET);
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    let tree = w.vm_tree()?;
    let problems = compare_trees(&tree, &w.sim().visible());
    finish(w, problems)
}

fn mq005<W: World>(w: &mut W) -> Result<Verdict> {
    if !w.can_fail_enumerations() {
        return Ok(Verdict::skip(
            "change enumeration never fails in this world",
        ));
    }
    setup(w, &[("d/a.txt", b"a")], &["d"])?;
    w.set_online(false)?;
    w.vm().write("d/b.txt", b"while offline")?;
    w.settle()?;
    // Hours of failing enumerations: the throttle climbs to its ceiling.
    w.sim().signal_working_set();
    w.sim().pump(Duration::from_secs(6 * 3600));
    let mut problems = Vec::new();
    let failures = w.sim().stats.ws_failures;
    if failures < 20 {
        problems.push(format!("only {failures} failed enumerations accumulated"));
    }
    w.set_online(true)?;
    // The change must reach the Mac within a minute of reconnecting.
    w.sim().pump(Duration::from_secs(60));
    let tree = w.vm_tree()?;
    problems.extend(
        compare_trees(&tree, &w.sim().visible())
            .into_iter()
            .map(|p| format!("60 s after reconnect: {p}")),
    );
    finish(w, problems)
}

fn mq006<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/a.txt", b"a")], &["d"])?;
    let d = id_of(w, "d").ok_or_else(|| anyhow!("d has no id"))?;
    w.restart_engine()?;
    w.vm().write("d/b.txt", b"after restart")?;
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    if w.sim().stats.enumerations.get(&d).copied().unwrap_or(0) != 1 {
        problems.push("expiry caused a re-enumeration (MQ-006: never)".into());
    }
    let tree = w.vm_tree()?;
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    finish(w, problems)
}

fn mq009<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("a.txt", b"a")], &[])?;
    w.sim().pump(Duration::from_secs(60));
    let mut problems = Vec::new();
    let asks = w.sim().stats.trash_asks;
    if asks > 2 {
        problems.push(format!(
            "trash container asked {asks} times in 60 s (MQ-009 loop)"
        ));
    }
    finish(w, problems)
}

fn mq011<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/a.txt", b"hello")], &["d"])?;
    w.set_online(false)?;
    // Opening a dataless file offline: item(for:) must still answer from the replica; the fetch
    // fails and the file stays (MQ-012).
    let _ = w.sim().open("d/a.txt");
    let mut problems = Vec::new();
    if w.sim().disk().resolve("d/a.txt").is_none() {
        problems.push("d/a.txt vanished from the Mac after an offline open (MQ-011)".into());
    }
    w.set_online(true)?;
    finish(w, problems)
}

fn mq013<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/f.txt", b"v1")], &["d"])?;
    w.sim().open("d/f.txt").map_err(|e| anyhow!("{e}"))?;
    // The agent rewrites the file; before the Mac hears of it, the user saves an edit.
    w.vm().write("d/f.txt", b"agent v2")?;
    w.sim()
        .write("d/f.txt", b"mac edit")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    // Before anything else signals: whatever the Mac now holds is final (MQ-013).
    let tree = w.vm_tree()?;
    problems.extend(
        compare_trees(&tree, &w.sim().visible())
            .into_iter()
            .map(|p| format!("after reply: {p}")),
    );
    w.sync()?;
    let tree = w.vm_tree()?;
    if tree.nodes.get("d/f.txt").and_then(|n| n.content.as_deref()) != Some(&b"agent v2"[..]) {
        problems.push("agent bytes lost".into());
    }
    if !tree.contents().any(|(_, c)| c == b"mac edit") {
        problems.push("Mac bytes not kept in a conflict copy".into());
    }
    finish(w, problems)
}

fn mq013b<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/f.txt", b"v1")], &["d"])?;
    w.sim().open("d/f.txt").map_err(|e| anyhow!("{e}"))?;
    // The agent rewrites the file; the user renames it before the Mac hears of the rewrite.
    w.vm().write("d/f.txt", b"agent v2 bytes")?;
    w.sim()
        .rename("d/f.txt", "g.txt")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    let tree = w.vm_tree()?;
    problems.extend(
        compare_trees(&tree, &w.sim().visible())
            .into_iter()
            .map(|p| format!("after reply: {p}")),
    );
    finish(w, problems)
}

fn mq014<W: World>(w: &mut W) -> Result<Verdict> {
    // The colliding VM path must be one the system never sees (as in the measurement): a file
    // dragged onto the icon of a folder that was never opened.
    setup(
        w,
        &[("d/keep.txt", b"k"), ("d/sub/x.txt", b"agent")],
        &["d"],
    )?;
    w.sim()
        .create_file("d/sub", "x.txt", b"mac")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    let tree = w.vm_tree()?;
    if tree
        .nodes
        .get("d/sub/x.txt")
        .and_then(|n| n.content.as_deref())
        != Some(&b"agent"[..])
    {
        problems.push("agent's x.txt was replaced".into());
    }
    if !tree.contents().any(|(_, c)| c == b"mac") {
        problems.push("the Mac's create never landed (MQ-014 collision loop)".into());
    }
    finish(w, problems)
}

fn mq016<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("README.md", b"upper")], &[])?;
    let before = w.sim().history.len();
    w.vm().write("readme.md", b"lower")?;
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    let writes = w.sim().history[before..]
        .iter()
        .filter(|c| matches!(c.kind, "modify" | "create" | "delete"))
        .count();
    if writes != 0 {
        problems.push(format!(
            "{writes} mutations sent for a server-side case collision"
        ));
    }
    let tree = w.vm_tree()?;
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    finish(w, problems)
}

fn mq016b<W: World>(w: &mut W) -> Result<Verdict> {
    // The Mac already shows `readme.md`; the agent adds `README.md`, which sorts first in byte
    // order and therefore keeps the plain name (rule 11): `readme.md` must become
    // `readme (Unlatch 2).md` on the Mac through the working set, never by a local bounce.
    setup(w, &[("readme.md", b"lower")], &[])?;
    let before = w.sim().history.len();
    w.vm().write("README.md", b"upper")?;
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    if w.sim().stats.bounces != 0 {
        problems.push(format!(
            "{} local bounce renames (MQ-016)",
            w.sim().stats.bounces
        ));
    }
    let writes = w.sim().history[before..]
        .iter()
        .filter(|c| matches!(c.kind, "modify" | "create" | "delete"))
        .count();
    if writes != 0 {
        problems.push(format!(
            "{writes} mutations sent for a server-side collision"
        ));
    }
    let tree = w.vm_tree()?;
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    finish(w, problems)
}

fn mq035<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/f.txt", b"v1")], &["d"])?;
    w.sim().open("d/f.txt").map_err(|e| anyhow!("{e}"))?;
    if !w.arm_reply_fault(ReplyFault::Modify)? {
        return Ok(Verdict::skip(
            "cannot inject die_before_ipc_reply:modify here",
        ));
    }
    let f = id_of(w, "d/f.txt");
    let before = w.sim().history.len();
    w.sim()
        .write("d/f.txt", b"mac edit")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    let mods = w.sim().history[before..]
        .iter()
        .filter(|c| c.kind == "modify" && c.id == f && c.fields & fields::CONTENTS != 0)
        .count();
    if mods < 2 {
        problems.push(format!("lost reply was not retried ({mods} modify calls)"));
    }
    w.sync()?;
    let tree = w.vm_tree()?;
    if tree.nodes.get("d/f.txt").and_then(|n| n.content.as_deref()) != Some(&b"mac edit"[..]) {
        problems.push("the retried write did not land in place".into());
    }
    for c in conflict_copies(w)? {
        problems.push(format!("conflict copy of the user's own save: {c}"));
    }
    finish(w, problems)
}

fn mq037<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/f.txt", b"v1")], &["d"])?;
    w.sim().open("d/f.txt").map_err(|e| anyhow!("{e}"))?;
    w.set_online(false)?;
    w.sim()
        .write("d/f.txt", b"offline edit")
        .map_err(|e| anyhow!("{e}"))?;
    // Twenty minutes of failed attempts push the next retry many minutes out (MQ-035).
    w.sim().pump(Duration::from_secs(20 * 60));
    w.set_online(true)?;
    w.sim().pump(Duration::from_secs(60));
    let mut problems = Vec::new();
    let tree = w.vm_tree()?;
    if tree.nodes.get("d/f.txt").and_then(|n| n.content.as_deref()) != Some(&b"offline edit"[..]) {
        problems.push("queued write not flushed within 60 s of reconnect (MQ-037)".into());
    }
    finish(w, problems)
}

fn mq049<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/f.txt", b"v1")], &["d"])?;
    w.sim().open("d/f.txt").map_err(|e| anyhow!("{e}"))?;
    let (d, f) = (id_of(w, "d"), id_of(w, "d/f.txt"));
    let before = w.sim().history.len();
    w.sim()
        .save_atomic("d/f.txt", b"saved atomically")
        .map_err(|e| anyhow!("{e}"))?;
    w.sync()?;
    let mut problems = Vec::new();
    let calls: Vec<_> = w.sim().history[before..].to_vec();
    let content_mods = calls
        .iter()
        .filter(|c| c.kind == "modify" && c.id == f && c.fields & fields::CONTENTS != 0)
        .count();
    let creates_deletes = calls
        .iter()
        .filter(|c| matches!(c.kind, "create" | "delete"))
        .count();
    let parent_mtime = calls
        .iter()
        .any(|c| c.kind == "modify" && c.id == d && c.fields == fields::CONTENT_MODIFICATION_DATE);
    if content_mods != 1 || creates_deletes != 0 || !parent_mtime {
        problems.push(format!(
            "atomic save produced {content_mods} content modifies, {creates_deletes} creates/deletes, parent mtime modify: {parent_mtime}"
        ));
    }
    if id_of(w, "d/f.txt") != f {
        problems.push("atomic save changed the item identifier".into());
    }
    let tree = w.vm_tree()?;
    if tree.nodes.get("d/f.txt").and_then(|n| n.content.as_deref())
        != Some(&b"saved atomically"[..])
    {
        problems.push("saved bytes not on the VM".into());
    }
    finish(w, problems)
}

fn mq080<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/f.txt", b"v1"), ("d/other.txt", b"o")], &["d"])?;
    w.sim().open("d/f.txt").map_err(|e| anyhow!("{e}"))?;
    w.set_online(false)?;
    w.sim()
        .write("d/f.txt", b"edit made offline")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(Duration::from_secs(30));
    w.vm().remove_file("d/f.txt")?;
    w.set_online(true)?;
    w.sync()?;
    let mut problems = Vec::new();
    let tree = w.vm_tree()?;
    if !tree.contents().any(|(_, c)| c == b"edit made offline") {
        problems.push("the Mac's pending edit on a remotely deleted file was lost (MQ-080)".into());
    }
    finish(w, problems)
}

fn replay_template<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/keep.txt", b"k")], &["d"])?;
    if !w.arm_reply_fault(ReplyFault::Create)? {
        return Ok(Verdict::skip(
            "cannot inject die_before_ipc_reply:create here",
        ));
    }
    let before = w.sim().history.len();
    w.sim()
        .create_file("d", "new.txt", b"from the mac")
        .map_err(|e| anyhow!("{e}"))?;
    w.sync()?;
    let mut problems = Vec::new();
    let templates: Vec<Option<String>> = w.sim().history[before..]
        .iter()
        .filter(|c| c.kind == "create")
        .map(|c| c.template_id.clone())
        .collect();
    if templates.len() < 2 || templates.windows(2).any(|p| p[0] != p[1]) {
        problems.push(format!("replayed create template ids: {templates:?}"));
    }
    let tree = w.vm_tree()?;
    let dups: Vec<&String> = tree
        .nodes
        .keys()
        .filter(|p| p.starts_with("d/") && numbered_base(&p[2..]).is_some_and(|b| b == "new.txt"))
        .collect();
    if !dups.is_empty() {
        problems.push(format!("replay created duplicates: {dups:?}"));
    }
    finish(w, problems)
}

fn replay_after_edit<W: World>(w: &mut W) -> Result<Verdict> {
    // Found by the fuzzer: the create lands, its reply is lost, the working set delivers the item
    // (the system now holds it twice: the pending create, bounced, and the mirror), the user
    // saves the mirror, then the create is replayed.
    setup(w, &[("keep.txt", b"k")], &[])?;
    if !w.arm_reply_fault(ReplyFault::Create)? {
        return Ok(Verdict::skip(
            "cannot inject die_before_ipc_reply:create here",
        ));
    }
    w.sim()
        .create_file("", "README", b"first version")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(Duration::ZERO);
    w.settle()?;
    w.sim().pump(Duration::ZERO);
    let _ = w.sim().save_atomic("README", b"saved after the lost reply");
    w.sim().pump(QUIESCE_BUDGET);
    finish(w, Vec::new())
}

fn replay_after_delete<W: World>(w: &mut W) -> Result<Verdict> {
    // Same shape, but the user deletes the delivered mirror (and puts a folder inside the
    // bounced pending one) before the replay.
    setup(w, &[("keep.txt", b"k")], &[])?;
    if !w.arm_reply_fault(ReplyFault::Create)? {
        return Ok(Verdict::skip(
            "cannot inject die_before_ipc_reply:create here",
        ));
    }
    w.sim().mkdir("", "src").map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(Duration::ZERO);
    w.settle()?;
    w.sim().pump(Duration::ZERO);
    if let Some(bounced) = w
        .sim()
        .visible()
        .into_iter()
        .find(|v| v.path != "src" && v.path.starts_with("src "))
    {
        let _ = w.sim().mkdir(&bounced.path, "inner");
    }
    let _ = w.sim().delete("src");
    w.sim().pump(QUIESCE_BUDGET);
    finish(w, Vec::new())
}

fn replayed_create_newer<W: World>(w: &mut W) -> Result<Verdict> {
    // Found by the fuzzer (seed 176): create, reply lost, save the new file again, replay.
    setup(w, &[("keep.txt", b"k")], &[])?;
    if !w.arm_reply_fault(ReplyFault::Create)? {
        return Ok(Verdict::skip(
            "cannot inject die_before_ipc_reply:create here",
        ));
    }
    let p = w
        .sim()
        .create_file("", "new.txt", b"first bytes")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(Duration::ZERO);
    // The user saves the file they created — still a pending create (possibly bounced by the
    // mirror the working set delivered meanwhile).
    let pending = w
        .sim()
        .visible()
        .into_iter()
        .find(|v| v.pending && v.id.is_none())
        .map_or(p, |v| v.path);
    w.sim()
        .save_atomic(&pending, b"second, newer bytes")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    let tree = w.vm_tree()?;
    if !tree.contents().any(|(_, c)| c == b"second, newer bytes") {
        problems.push("the user's second save of the new file never reached the VM".into());
    }
    finish(w, problems)
}

fn second_save<W: World>(w: &mut W) -> Result<Verdict> {
    // Found by the fuzzer (seed 8): save, reply lost, save again before the retry.
    setup(w, &[("notes", b"v1")], &[])?;
    w.sim().open("notes").map_err(|e| anyhow!("{e}"))?;
    if !w.arm_reply_fault(ReplyFault::Modify)? {
        return Ok(Verdict::skip(
            "cannot inject die_before_ipc_reply:modify here",
        ));
    }
    w.sim()
        .write("notes", b"first save")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(Duration::ZERO);
    w.sim()
        .write("notes", b"second save")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    for c in conflict_copies(w)? {
        problems.push(format!(
            "conflict copy of the user's own consecutive saves: {c}"
        ));
    }
    let tree = w.vm_tree()?;
    if tree.nodes.get("notes").and_then(|n| n.content.as_deref()) != Some(&b"second save"[..]) {
        problems.push("the second save did not land".into());
    }
    finish(w, problems)
}

fn dir_stays<W: World>(w: &mut W) -> Result<Verdict> {
    setup(
        w,
        &[
            ("d/a.txt", b"a"),
            ("d/b.txt", b"b"),
            ("d/sub/c.txt", b"c"),
            ("keep.txt", b"k"),
        ],
        &["d", "d/sub"],
    )?;
    w.sim().open("d/a.txt").map_err(|e| anyhow!("{e}"))?;
    w.vm().remove_dir_all("d")?;
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    let tree = w.vm_tree()?;
    let problems = compare_trees(&tree, &w.sim().visible());
    finish(w, problems)
}

fn tombstones_follow_dir<W: World>(w: &mut W) -> Result<Verdict> {
    // The Mac lists the root but never opens `d`. The agent moves a root file into `d` (the Mac
    // follows the move, the old parent being materialized), then deletes `d` recursively.
    setup(w, &[("f.txt", b"f"), ("d/inner.txt", b"i")], &[])?;
    w.vm().rename("f.txt", "d/f.txt")?;
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    w.vm().remove_dir_all("d")?;
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    let tree = w.vm_tree()?;
    let problems = compare_trees(&tree, &w.sim().visible());
    finish(w, problems)
}

fn symlink_depth<W: World>(w: &mut W) -> Result<Verdict> {
    // `a/b/link -> ../../x` resolves to `x` inside the root. After `mv a/b b` the same link sits
    // at depth 1 and would resolve one level above the root.
    setup(w, &[("x", b"x"), ("a/b/keep", b"k")], &["a", "a/b"])?;
    w.vm().symlink("../../x", "a/b/link")?;
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    w.vm().rename("a/b", "b")?;
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    w.sim().browse("b").map_err(|e| anyhow!("{e}"))?;
    let tree = w.vm_tree()?;
    let mut problems = compare_trees(&tree, &w.sim().visible());
    let dir = tempfile::Builder::new().prefix("fpsim-mat-").tempdir()?;
    problems.extend(super::check::materialize_and_check_realpaths(
        &w.sim().visible(),
        &dir.path().join("m"),
    )?);
    finish(w, problems)
}

fn evict_on_update<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/f.txt", b"aaaa")], &["d"])?;
    w.sim().open("d/f.txt").map_err(|e| anyhow!("{e}"))?;
    w.vm().write("d/f.txt", b"bbbb")?;
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    let k = w.sim().disk().resolve("d/f.txt");
    let materialized = k
        .and_then(|k| w.sim().disk().get(k))
        .is_some_and(|n| n.content.is_some());
    if materialized {
        problems.push("file still materialized after its content version changed".into());
    }
    let tree = w.vm_tree()?;
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    match w.sim().open("d/f.txt") {
        Ok(b) if b == b"bbbb" => {}
        Ok(b) => problems.push(format!("reopen returned {:?}", String::from_utf8_lossy(&b))),
        Err(e) => problems.push(format!("reopen failed: {e}")),
    }
    if tree.nodes.get("d/f.txt").map(|n| n.kind) != Some(VmKind::File) {
        problems.push("d/f.txt missing on the VM".into());
    }
    finish(w, problems)
}

fn no_sync_errors<W: World>(w: &mut W, problems: &mut Vec<String>) {
    for (p, code) in w.sim().sync_errors() {
        problems.push(format!("{p}: sync error {code:?}"));
    }
}

fn rule2_identical_create<W: World>(w: &mut W) -> Result<Verdict> {
    // As in MQ-014, the VM path is one the system never saw (a never-opened folder), but the
    // agent already wrote exactly the bytes the user now drops there.
    setup(
        w,
        &[("d/keep.txt", b"k"), ("d/sub/x.txt", b"same bytes")],
        &["d"],
    )?;
    w.sim()
        .create_file("d/sub", "x.txt", b"same bytes")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    no_sync_errors(w, &mut problems);
    let tree = w.vm_tree()?;
    let copies: Vec<&String> = tree
        .nodes
        .iter()
        .filter(|(_, n)| n.content.as_deref() == Some(&b"same bytes"[..]))
        .map(|(p, _)| p)
        .collect();
    if copies != ["d/sub/x.txt"] {
        problems.push(format!("identical create was not merged: {copies:?}"));
    }
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    finish(w, problems)
}

fn rule4_rename_race<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/f.txt", b"f")], &["d"])?;
    // The agent renames the file; before the Mac hears of it the user renames it too.
    w.vm().rename("d/f.txt", "d/g.txt")?;
    w.sim()
        .rename("d/f.txt", "h.txt")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    no_sync_errors(w, &mut problems);
    let tree = w.vm_tree()?;
    if tree.nodes.get("d/g.txt").and_then(|n| n.content.as_deref()) != Some(&b"f"[..]) {
        problems.push("the agent's rename was undone (d/g.txt missing on the VM)".into());
    }
    if tree.nodes.contains_key("d/h.txt") {
        problems.push("a rename based on a stale name was applied over the agent's".into());
    }
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    finish(w, problems)
}

fn rule5_tags<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/f.txt", b"v1")], &["d"])?;
    let before_tree = w.vm_tree()?;
    let version_of = |w: &mut W| {
        let k = w.sim().disk().resolve("d/f.txt")?;
        w.sim().disk().get(k)?.version
    };
    let before = version_of(w);
    w.sim()
        .set_tags("d/f.txt", b"red")
        .map_err(|e| anyhow!("{e}"))?;
    w.sync()?;
    let mut problems = Vec::new();
    no_sync_errors(w, &mut problems);
    if version_of(w) != before {
        problems.push(format!(
            "tagging changed the item's version {before:?} -> {:?} (a VM round trip?)",
            version_of(w)
        ));
    }
    let after_tree = w.vm_tree()?;
    if after_tree.nodes.keys().ne(before_tree.nodes.keys())
        || after_tree.contents().ne(before_tree.contents())
    {
        problems.push("tagging changed the VM tree".into());
    }
    // The agent rewrites the file: the update the Mac receives must still carry the tag.
    w.vm().write("d/f.txt", b"agent v2")?;
    w.sync()?;
    let tag = w
        .sim()
        .disk()
        .resolve("d/f.txt")
        .and_then(|k| w.sim().disk().get(k))
        .and_then(|n| n.local.tag_data.clone());
    if tag.as_deref() != Some(&b"red"[..]) {
        problems.push(format!(
            "the tag did not survive a VM update (MQ-043): {:?}",
            tag.map(|t| String::from_utf8_lossy(&t).into_owned())
        ));
    }
    let tree = w.vm_tree()?;
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    finish(w, problems)
}

fn rule6_delete_unseen<W: World>(w: &mut W) -> Result<Verdict> {
    setup(w, &[("d/a.txt", b"a"), ("keep.txt", b"k")], &["d"])?;
    // The agent adds a file; before the Mac hears of it the user deletes the folder.
    w.vm().write("d/new.txt", b"agent work")?;
    w.sim().delete("d").map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    let tree = w.vm_tree()?;
    if tree
        .nodes
        .get("d/new.txt")
        .and_then(|n| n.content.as_deref())
        != Some(&b"agent work"[..])
    {
        problems.push("the agent's unseen d/new.txt was deleted".into());
    }
    if tree.nodes.contains_key("d/a.txt") {
        problems.push("d/a.txt, which the Mac saw and deleted, is still on the VM".into());
    }
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    finish(w, problems)
}

fn rule6_retried_delete<W: World>(w: &mut W) -> Result<Verdict> {
    // Fuzz seed 186. The agent writes into a folder; the engine has it, the system has not
    // consumed it yet when the user deletes the folder. The delete keeps the unseen file but
    // its reply is lost (the engine dies). Before the system retries it enumerates the working
    // set — consuming the anchor that carries the file, under a folder it already deleted
    // locally, so nothing is shown. The retry is the same call and must not delete the file.
    setup(w, &[("d/a.txt", b"a"), ("keep.txt", b"k")], &["d"])?;
    if !w.arm_reply_fault(ReplyFault::Delete)? {
        return Ok(Verdict::skip(
            "cannot inject die_before_ipc_reply:delete here",
        ));
    }
    w.vm().write("d/new.txt", b"agent work")?;
    w.settle()?;
    w.sim().delete("d").map_err(|e| anyhow!("{e}"))?;
    let mut problems = Vec::new();
    // The first attempt, and nothing else: its reply is lost.
    if !w.sim().run_one_queued_op() {
        problems.push("set-up: the delete was not offered".into());
    }
    // The system enumerates the working set before it retries (explicit: on its own the order
    // races with the restarted engine's `ErrorResolved`, which flushes the delete, MQ-037).
    w.settle()?;
    let sets = w.sim().change_sets_applied();
    w.sim().enumerate_working_set_now();
    if w.sim().change_sets_applied() == sets {
        problems.push("set-up: no change set was consumed before the retry".into());
    }
    w.sim().pump(QUIESCE_BUDGET);
    let tree = w.vm_tree()?;
    if tree
        .nodes
        .get("d/new.txt")
        .and_then(|n| n.content.as_deref())
        != Some(&b"agent work"[..])
    {
        problems.push("the retried delete removed the agent's unseen d/new.txt".into());
    }
    if tree.nodes.contains_key("d/a.txt") {
        problems.push("d/a.txt, which the Mac saw and deleted, is still on the VM".into());
    }
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    finish(w, problems)
}

/// Model checks: scenarios about fpsim's own model of fileproviderd rather than one engine
/// rule, so they have no failing-first engine flaw. They run against the scripted world
/// (`tests/fpsim_model.rs`) and end to end (`tests/fpsim_e2e.rs`).
pub const MODEL_CHECKS: &[&str] = &["rule2_recreated_folder_takes_numbered_name"];

/// End-to-end checks: need the real daemon (its persisted index), so the scripted world skips
/// them. Run by `tests/fpsim_e2e.rs`.
pub const E2E_CHECKS: &[&str] = &["rule7_index_change_reimports"];

fn rule2_recreated_folder<W: World>(w: &mut W) -> Result<Verdict> {
    // Fuzz seed 13 (an fpsim bug: a local bounce that the provider's reply confirmed was later
    // "undone", so the Mac showed README for the VM's `README 2`). A new Mac folder is still queued (the link is down) when the agent creates
    // a file of that name. Whichever the system learns first, the folder must land as `name 2`
    // on the VM (rule 2: a create never surfaces Exists) and the Mac must show both, also after
    // the agent's file goes.
    setup(w, &[("keep.txt", b"k")], &[])?;
    let mut problems = Vec::new();
    // (a) The create goes first (queued writes before change enumerations): the engine already
    //     has the agent's `notes`, so the folder lands as `notes 2`.
    // (b) The create fails (link down) and the working set comes first: the system bounces its
    //     pending folder to `README 2` locally (MQ-016) and the create lands under that name.
    //     Provider and Mac then agree: nothing may rename it back to README when the agent's
    //     README goes.
    for (name, ws_first) in [("notes", false), ("README", true)] {
        if ws_first {
            w.set_online(false)?;
            w.sim().mkdir("", name).map_err(|e| anyhow!("{e}"))?;
            w.sim().pump(Duration::ZERO);
            w.vm().write(name, b"agent file")?;
            w.set_online(true)?;
            w.settle()?;
            w.sim().enumerate_working_set_now();
        } else {
            w.vm().write(name, b"agent file")?;
            w.settle()?;
            w.sim().mkdir("", name).map_err(|e| anyhow!("{e}"))?;
        }
        w.sim().pump(QUIESCE_BUDGET);
        w.settle()?;
        w.sim().pump(QUIESCE_BUDGET);
        no_sync_errors(w, &mut problems);
        let tree = w.vm_tree()?;
        if tree.nodes.get(name).map(|n| n.kind) != Some(VmKind::File) {
            problems.push(format!("{name}: the agent's file is not on the VM"));
        }
        if tree.nodes.get(&format!("{name} 2")).map(|n| n.kind) != Some(VmKind::Dir) {
            problems.push(format!(
                "{name}: the Mac's folder did not land as `{name} 2`"
            ));
        }
        problems.extend(compare_trees(&tree, &w.sim().visible()));
        w.vm().remove_file(name)?;
        w.settle()?;
        w.sim().pump(QUIESCE_BUDGET);
        let tree = w.vm_tree()?;
        problems.extend(compare_trees(&tree, &w.sim().visible()));
    }
    finish(w, problems)
}

fn rule7_index_change<W: World>(w: &mut W) -> Result<Verdict> {
    // Review (c)7. The Mac holds edits it could not send (the link is down) when the VM loses
    // unlatchd's index: the next daemon builds a new one (new IndexId, ids renumbered). The
    // engine must emit Reimport{below: ROOT}; no op addressed by an id of the old index may
    // execute against the new one (an old id names nothing — or, worse, another item — there);
    // pending edits land by path (creates that may already exist) or as conflict copies, never
    // silently dropped; and the trees converge.
    let files: &[(&str, &[u8])] = &[
        ("a.txt", b"A"),
        ("b.txt", b"B"),
        ("c.txt", b"C"),
        ("d/x.txt", b"X"),
        ("d/y.txt", b"Y"),
    ];
    setup(w, files, &["d"])?;
    for (p, _) in files {
        w.sim().open(p).map_err(|e| anyhow!("{e}"))?;
    }
    let old_ids: std::collections::HashSet<ItemId> =
        w.sim().visible().into_iter().filter_map(|v| v.id).collect();
    let reimports = w.sim().stats.reimports;
    w.set_online(false)?;
    let edits: &[(&str, &[u8])] = &[("a.txt", b"mac edit of a"), ("d/x.txt", b"mac edit of x")];
    for (p, b) in edits {
        w.sim().write(p, b).map_err(|e| anyhow!("{e}"))?;
    }
    w.sim()
        .create_file("d", "new.txt", b"mac new file")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().delete("c.txt").map_err(|e| anyhow!("{e}"))?;
    // (Not offered yet: each attempt would only wait out the engine's offline timeout.)
    if !w.wipe_daemon_index(false)? {
        return Ok(Verdict::skip(
            "cannot wipe the daemon's index in this world",
        ));
    }
    w.set_online(true)?;
    let mut problems = w.converged()?;
    if w.sim().stats.reimports == reimports {
        problems.push("no Reimport{below: ROOT} after the index changed".into());
    }
    let tree = w.vm_tree()?;
    let content = |p: &str| tree.nodes.get(p).and_then(|n| n.content.clone());
    // Items the Mac never touched are exactly as they were, and the delete addressed by an id
    // of the old index did not execute (the system re-learns c.txt instead).
    for (p, b) in [("b.txt", &b"B"[..]), ("d/y.txt", b"Y"), ("c.txt", b"C")] {
        if content(p).as_deref() != Some(b) {
            problems.push(format!(
                "{p}: {:?} on the VM, expected {:?} (an op of the old index executed?)",
                content(p).map(|c| String::from_utf8_lossy(&c).into_owned()),
                String::from_utf8_lossy(b)
            ));
        }
    }
    // Every pending edit's bytes are on the VM: in place or in a conflict copy.
    for b in [&b"mac edit of a"[..], b"mac edit of x", b"mac new file"] {
        if !tree.contents().any(|(_, c)| c.as_slice() == b) {
            problems.push(format!(
                "pending edit {:?} silently lost",
                String::from_utf8_lossy(b)
            ));
        }
    }
    // The Mac shows no item under an identifier of the previous index.
    for v in w.sim().visible() {
        if v.id
            .is_some_and(|id| id != ItemId::ROOT && old_ids.contains(&id))
        {
            problems.push(format!(
                "{}: shown under id {:?} of the previous index",
                v.path, v.id
            ));
        }
    }
    finish(w, problems)
}

fn rule10_exec_hidden<W: World>(w: &mut W) -> Result<Verdict> {
    setup(
        w,
        &[("bin/run.sh", b"#!/bin/sh\n"), ("keep.txt", b"k")],
        &[],
    )?;
    w.vm().set_mode("bin/run.sh", 0o755)?;
    w.vm().write("bin/evil.command", b"#!/bin/sh\n")?;
    w.vm().set_mode("bin/evil.command", 0o755)?;
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    w.sim().browse("bin").map_err(|e| anyhow!("{e}"))?;
    w.sync()?;
    let mut problems = Vec::new();
    for p in ["bin/run.sh", "bin/evil.command"] {
        match w
            .sim()
            .disk()
            .resolve(p)
            .and_then(|k| w.sim().disk().get(k))
        {
            Some(n) if n.user_exec => {
                problems.push(format!("{p}: executable on the Mac (exec bit exposed)"))
            }
            Some(_) => {}
            None => problems.push(format!("{p}: not shown on the Mac")),
        }
    }
    let tree = w.vm_tree()?;
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    finish(w, problems)
}

fn rule11_move_twin<W: World>(w: &mut W) -> Result<Verdict> {
    // `README.md` sorts first and keeps its name; `readme.md` shows as `readme (Unlatch 2).md`.
    setup(
        w,
        &[
            ("README.md", b"upper"),
            ("readme.md", b"lower"),
            ("sub/keep.txt", b"k"),
        ],
        &["sub"],
    )?;
    let mapped = w
        .sim()
        .visible()
        .into_iter()
        .find(|v| !v.path.contains('/') && v.path != "README.md" && v.path.ends_with(".md"))
        .map(|v| v.path)
        .ok_or_else(|| anyhow!("the twin is not shown on the Mac"))?;
    w.sim()
        .move_to(&mapped, "sub")
        .map_err(|e| anyhow!("{e}"))?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    no_sync_errors(w, &mut problems);
    let tree = w.vm_tree()?;
    if tree
        .nodes
        .get("sub/readme.md")
        .and_then(|n| n.content.as_deref())
        != Some(&b"lower"[..])
    {
        problems.push(format!(
            "moving {mapped:?} did not move readme.md to sub/readme.md on the VM"
        ));
    }
    for p in tree.nodes.keys().filter(|p| p.contains("(Unlatch ")) {
        problems.push(format!("generated display name leaked to the VM: {p}"));
    }
    if tree
        .nodes
        .get("README.md")
        .and_then(|n| n.content.as_deref())
        != Some(&b"upper"[..])
    {
        problems.push("README.md changed".into());
    }
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    finish(w, problems)
}

fn rule8_mass_delete<W: World>(w: &mut W) -> Result<Verdict> {
    // 40 downloaded files (above the guard's floor of 32, and far above 20% of M).
    const N: usize = 40;
    let names: Vec<String> = (0..N).map(|i| format!("d/f{i:02}.txt")).collect();
    let mut files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
    files.push(("keep.txt", b"k"));
    setup(w, &files, &["d"])?;
    for n in &names {
        w.sim().open(n).map_err(|e| anyhow!("open {n}: {e}"))?;
    }
    w.sync()?;
    if !w.set_auto_confirm(false)? {
        return Ok(Verdict::skip(
            "this world confirms mass deletions by itself",
        ));
    }
    w.vm().remove_dir_all("d")?;
    w.settle()?;
    w.sim().pump(QUIESCE_BUDGET);
    let mut problems = Vec::new();
    let downloaded = |w: &mut W| {
        w.sim()
            .visible()
            .iter()
            .filter(|v| v.path.starts_with("d/") && v.content.is_some())
            .count()
    };
    let left = downloaded(w);
    if left != N {
        problems.push(format!(
            "{} of {N} downloaded files removed from the Mac without the user's confirmation",
            N - left
        ));
    }
    match w.sim().engine_status().map(|s| s.state) {
        Some(unlatch_proto::ipc::ConnState::Paused { .. }) => {}
        other => problems.push(format!("engine not paused: {other:?}")),
    }
    // The user confirms in the menu bar: now the deletion lands on the Mac.
    w.sim()
        .confirm_paused(true)
        .map_err(|e| anyhow!("confirm: {e}"))?;
    w.set_auto_confirm(true)?;
    w.sync()?;
    if w.sim().disk().resolve("d").is_some() {
        problems.push("d still on the Mac after the user confirmed".into());
    }
    let tree = w.vm_tree()?;
    problems.extend(compare_trees(&tree, &w.sim().visible()));
    finish(w, problems)
}

/// Make `mkdirs` reachable for callers building trees by hand.
pub fn ensure_dirs<W: World>(w: &mut W, path: &str) -> Result<()> {
    mkdirs(w, path)
}
