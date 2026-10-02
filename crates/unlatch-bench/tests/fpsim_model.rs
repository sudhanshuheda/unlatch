//! fpsim's own model of fileproviderd, exercised against the (correct) scripted engine: the
//! behaviours that need no engine flaw to demonstrate (what the *system* does, and what it never
//! sends).

use std::time::Duration;
use unlatch_bench::fpsim::scripted::Flaws;
use unlatch_bench::fpsim::sim::{write_retry_delay, ActionError, TrashAnswer};
use unlatch_bench::fpsim::world::QUIESCE_BUDGET;
use unlatch_bench::fpsim::{ScriptedWorld, World};
use unlatch_proto::ipc::fields;
use unlatch_proto::ErrorCode;

fn world(files: &[(&str, &[u8])]) -> ScriptedWorld {
    let mut w = ScriptedWorld::new(Flaws::default()).expect("world");
    for (p, b) in files {
        let mut acc = String::new();
        let comps: Vec<&str> = p.split('/').collect();
        for c in &comps[..comps.len() - 1] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(c);
            if w.vm().kind(&acc).is_none() {
                w.vm().mkdir(&acc).expect("mkdir");
            }
        }
        w.vm().write(p, b).expect("write");
    }
    w.settle().expect("settle");
    w.sim().add_domain().expect("add domain");
    w.sync().expect("sync");
    w
}

fn calls_since(w: &mut ScriptedWorld, from: usize, kind: &str) -> usize {
    w.sim().history[from..]
        .iter()
        .filter(|c| c.kind == kind)
        .count()
}

#[test]
fn mq015_finder_resolves_local_collisions_itself() {
    let mut w = world(&[("run.sh", b"#!/bin/sh")]);
    let p = w.sim().create_file("", "run.sh", b"other").expect("create");
    assert_eq!(p, "run copy.sh");
    w.sync().expect("sync");
    let tree = w.vm_tree().expect("tree");
    assert!(tree.nodes.contains_key("run copy.sh"));
    assert_eq!(
        tree.nodes["run.sh"].content.as_deref(),
        Some(&b"#!/bin/sh"[..])
    );
    assert!(w.converged().expect("converged").is_empty());
}

#[test]
fn mq046_ds_store_never_reaches_the_provider() {
    let mut w = world(&[("a.txt", b"a")]);
    let before = w.sim().history.len();
    w.sim()
        .create_file("", ".DS_Store", b"finder junk")
        .expect("create");
    w.sync().expect("sync");
    assert_eq!(calls_since(&mut w, before, "create"), 0);
    assert!(!w.vm_tree().expect("tree").nodes.contains_key(".DS_Store"));
    assert!(
        w.converged().expect("converged").is_empty(),
        "local-only items are not divergence"
    );
}

#[test]
fn mq047_chmod_carries_only_owner_exec_and_noops_are_silent() {
    let mut w = world(&[("tool", b"x")]);
    let before = w.sim().history.len();
    w.sim().chmod_exec("tool", false).expect("chmod");
    w.sync().expect("sync");
    assert_eq!(
        calls_since(&mut w, before, "modify"),
        0,
        "chmod to the current state sends nothing"
    );
    w.sim().chmod_exec("tool", true).expect("chmod");
    w.sync().expect("sync");
    let mods: Vec<u32> = w.sim().history[before..]
        .iter()
        .filter(|c| c.kind == "modify")
        .map(|c| c.fields)
        .collect();
    assert_eq!(mods, vec![fields::FILE_SYSTEM_FLAGS]);
    assert_eq!(w.vm_tree().expect("tree").nodes["tool"].mode & 0o100, 0o100);
}

#[test]
fn mq048_rename_is_one_filename_modify() {
    let mut w = world(&[("d/a.txt", b"a")]);
    w.sim().browse("d").expect("browse");
    let before = w.sim().history.len();
    w.sim().rename("d/a.txt", "b.txt").expect("rename");
    w.sync().expect("sync");
    let mods: Vec<u32> = w.sim().history[before..]
        .iter()
        .filter(|c| c.kind == "modify")
        .map(|c| c.fields)
        .collect();
    assert_eq!(mods, vec![fields::FILENAME]);
    assert!(w.vm_tree().expect("tree").nodes.contains_key("d/b.txt"));
    assert!(w.converged().expect("converged").is_empty());
}

#[test]
fn mq042_tags_are_mac_only_metadata() {
    let mut w = world(&[("a.txt", b"a")]);
    let tree_before = w.vm_tree().expect("tree");
    let before = w.sim().history.len();
    w.sim().set_tags("a.txt", b"bplist-tags").expect("tag");
    w.sync().expect("sync");
    let mods: Vec<u32> = w.sim().history[before..]
        .iter()
        .filter(|c| c.kind == "modify")
        .map(|c| c.fields)
        .collect();
    assert_eq!(
        mods,
        vec![fields::TAG_DATA],
        "one modifyItem with tagData only"
    );
    assert_eq!(
        w.vm_tree().expect("tree"),
        tree_before,
        "tags never reach the VM"
    );
    assert!(w.converged().expect("converged").is_empty());
}

#[test]
fn mq012_mq036_failed_fetch_leaves_item_and_is_not_retried() {
    let mut w = world(&[("big.bin", b"payload")]);
    w.set_online(false).expect("offline");
    let before = w.sim().history.len();
    assert_eq!(
        w.sim().open("big.bin"),
        Err(ActionError::Fetch(ErrorCode::Offline))
    );
    w.sim().pump(Duration::from_secs(600));
    assert_eq!(
        calls_since(&mut w, before, "fetch"),
        1,
        "the system never re-issues a failed fetch"
    );
    assert!(
        w.sim().disk().resolve("big.bin").is_some(),
        "the item stays after a failed fetch"
    );
    w.set_online(true).expect("online");
    assert_eq!(w.sim().open("big.bin").expect("second read"), b"payload");
}

#[test]
fn mq035_queued_write_backoff_doubles_without_ceiling() {
    let mut w = world(&[("f", b"v1")]);
    w.sim().open("f").expect("open");
    w.set_online(false).expect("offline");
    w.sim().write("f", b"offline edit").expect("write");
    w.sim().pump(Duration::from_secs(3600));
    let times: Vec<f64> = w
        .sim()
        .history
        .iter()
        .filter(|c| c.kind == "modify")
        .map(|c| c.at.as_secs_f64())
        .collect();
    assert!(times.len() >= 8, "{times:?}");
    for (i, pair) in times.windows(2).enumerate() {
        let gap = pair[1] - pair[0];
        let want = write_retry_delay(i as u32 + 1).as_secs_f64();
        assert!((gap - want).abs() < 1e-6, "gap {i}: {gap} vs {want}");
    }
    assert!(w.sim().pending() > 0, "never dropped");
}

#[test]
fn mq037_working_set_signal_does_not_flush_queued_writes() {
    let mut w = world(&[("f", b"v1")]);
    w.sim().open("f").expect("open");
    w.set_online(false).expect("offline");
    w.sim().write("f", b"queued").expect("write");
    w.sim().pump(Duration::from_secs(900));
    let before = w.sim().history.len();
    // Bring the VM back *without* ErrorResolved: only a working-set signal.
    w.engine.set_flaws(Flaws {
        no_error_resolved: true,
        ..Flaws::default()
    });
    w.set_online(true).expect("online");
    w.sim().signal_working_set();
    w.sim().pump(Duration::from_secs(30));
    assert_eq!(
        calls_since(&mut w, before, "modify"),
        0,
        "a working-set signal must not flush writes"
    );
}

#[test]
fn mq005_signals_do_not_bypass_the_throttle() {
    let mut w = ScriptedWorld::new(Flaws {
        changes_fail_offline: true,
        ..Flaws::default()
    })
    .expect("world");
    w.sim().add_domain().expect("add");
    w.set_online(false).expect("offline");
    w.sim().signal_working_set();
    w.sim().pump(Duration::ZERO);
    let first = w
        .sim()
        .history
        .iter()
        .filter(|c| c.kind == "changes")
        .count();
    for _ in 0..20 {
        w.sim().signal_working_set();
        w.sim().advance(Duration::from_secs(1));
        w.sim().pump(Duration::ZERO);
    }
    let after = w
        .sim()
        .history
        .iter()
        .filter(|c| c.kind == "changes")
        .count();
    assert_eq!(
        after, first,
        "20 signals within the first 30 s throttle window must not enumerate"
    );
}

#[test]
fn mq075_mq010_trash_is_asked_twice_then_abandoned() {
    let mut w = world(&[("a", b"a")]);
    w.sim().pump(Duration::from_secs(120));
    assert_eq!(w.sim().stats.trash_asks, 2);
    let mut looping = ScriptedWorld::with_config(Flaws::default(), |c| {
        c.trash_answer = TrashAnswer::NoSuchItem
    })
    .expect("world");
    looping.sim().add_domain().expect("add");
    looping.sim().pump(Duration::from_secs(120));
    assert!(
        looping.sim().stats.trash_asks > 100,
        "MQ-009: ~1 Hz for ever"
    );
}

#[test]
fn deletion_rejected_restores_the_folder_with_the_unseen_file() {
    let mut w = world(&[("d/a.txt", b"a")]);
    w.sim().browse("d").expect("browse");
    w.vm()
        .write("d/new.txt", b"agent wrote this")
        .expect("write");
    w.sim().delete("d").expect("delete");
    w.sync().expect("sync");
    assert!(w.sim().stats.deletion_rejected >= 1);
    assert_eq!(
        w.vm_tree().expect("tree").nodes["d/new.txt"]
            .content
            .as_deref(),
        Some(&b"agent wrote this"[..])
    );
    assert!(w.converged().expect("converged").is_empty());
}

#[test]
fn folder_contents_arrive_through_the_working_set_not_re_enumeration() {
    let mut w = world(&[("d/a.txt", b"a")]);
    w.sim().browse("d").expect("browse");
    for i in 0..20 {
        w.vm().write(&format!("d/f{i}"), b"x").expect("write");
    }
    w.settle().expect("settle");
    w.sim().pump(QUIESCE_BUDGET);
    assert_eq!(w.sim().stats.enumerations.values().copied().max(), Some(1));
    let disk = w.sim().disk();
    let d = disk.resolve("d").expect("d");
    assert_eq!(disk.children(d).len(), 21);
}

#[test]
fn moves_between_folders_keep_identifiers() {
    let mut w = world(&[("a/f.txt", b"f"), ("b/keep", b"k")]);
    w.sim().browse("a").expect("browse a");
    w.sim().browse("b").expect("browse b");
    let k = w.sim().disk().resolve("a/f.txt").expect("f");
    let id = w.sim().disk().get(k).and_then(|n| n.id);
    w.vm().rename("a/f.txt", "b/f.txt").expect("mv");
    w.sync().expect("sync");
    let k2 = w.sim().disk().resolve("b/f.txt").expect("moved");
    assert_eq!(w.sim().disk().get(k2).and_then(|n| n.id), id);
    assert!(w.converged().expect("converged").is_empty());
}

/// Fuzz seed 13: a pending folder the system bounced locally (MQ-016) to the very name the
/// provider then gave it (rule 2 `README 2`) is settled — when the agent's README goes, the
/// Mac keeps showing `README 2`, as the VM has it. Both orders of create vs working set.
#[test]
fn recreated_folder_keeps_the_numbered_name_the_provider_gave_it() {
    use unlatch_bench::fpsim::scenarios::{run, MODEL_CHECKS};
    for name in MODEL_CHECKS {
        let mut w = ScriptedWorld::new(Flaws::default()).expect("world");
        let v = run(name, &mut w).expect("run");
        assert!(
            v.pass(),
            "{name}: {:#?} (skipped: {:?})",
            v.problems,
            v.skipped
        );
    }
}
