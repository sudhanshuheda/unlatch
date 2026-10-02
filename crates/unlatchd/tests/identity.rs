//! Identity rules (D4) and versions (D3, T17) observed through a live session.

mod common;
use common::*;
use std::io::Write;
use unlatch_proto::wire::Change;
use unlatch_proto::{ItemId, Kind};

fn session(root: &std::path::Path, state: &std::path::Path) -> Client {
    let mut c = Client::spawn(root, state, Opts::default());
    c.wait_snapshot();
    c
}

#[test]
fn mv_a_b_then_touch_a() {
    let (root, state) = (tmp(), tmp());
    std::fs::write(root.path().join("a"), b"A").unwrap();
    let mut c = session(root.path(), state.path());
    let a = c.find("a").unwrap();
    std::fs::rename(root.path().join("a"), root.path().join("b")).unwrap();
    std::fs::write(root.path().join("a"), b"new").unwrap();
    c.ping();
    assert_eq!(c.find("b").unwrap().id, a.id, "id follows the inode");
    let na = c.find("a").unwrap();
    assert_ne!(na.id, a.id, "the new a is a new item");
    assert_eq!(na.size, 3);
}

/// `mv_a_b_then_touch_a` failed on CI (2026-10-01): b came back as a new item. rename(2) queues
/// IN_MOVED_FROM and IN_MOVED_TO one after the other, and a batch taken between them held the
/// IN_MOVED_FROM alone. The window opens when the renaming thread is preempted there: the test
/// and the daemon share one CPU and the daemon reconciles at once (no debounce), which cut
/// ~1 rename in 5 before the fix (`core::tests::rename_cut_after_its_moved_from_keeps_the_id`
/// injects the cut deterministically).
#[test]
fn mv_a_b_then_touch_a_on_one_cpu() {
    // SAFETY: plain libc calls on a zeroed cpu_set_t; the spawned daemon inherits the mask.
    unsafe {
        let cpu = libc::sched_getcpu().max(0) as usize;
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
    let (root, state) = (tmp(), tmp());
    let (a, b) = (root.path().join("a"), root.path().join("b"));
    std::fs::write(&a, b"A").unwrap();
    let opts = Opts {
        env: vec![
            ("UNLATCHD_DEBOUNCE_MS".into(), "0".into()),
            ("UNLATCHD_SETTLE_US".into(), "0".into()),
        ],
        ..Opts::default()
    };
    let mut c = Client::spawn(root.path(), state.path(), opts);
    c.wait_snapshot();
    let mut split = Vec::new();
    for round in 0..300 {
        let old = c.find("a").unwrap().id;
        std::fs::rename(&a, &b).unwrap();
        std::fs::write(&a, b"new").unwrap();
        c.ping();
        if c.find("b").unwrap().id != old {
            split.push(round);
        }
        std::fs::remove_file(&b).unwrap();
        c.ping();
    }
    assert!(split.is_empty(), "b lost a's id in rounds {split:?}");
}

#[test]
fn rename_over_keeps_destination_id() {
    let (root, state) = (tmp(), tmp());
    std::fs::write(root.path().join("doc.txt"), b"v1").unwrap();
    let mut c = session(root.path(), state.path());
    let doc = c.find("doc.txt").unwrap();
    // Temp file observed by the watcher first (slow save)…
    std::fs::write(root.path().join(".doc.tmp"), b"version 2").unwrap();
    c.ping();
    let tmpe = c.find(".doc.tmp").unwrap();
    std::fs::rename(root.path().join(".doc.tmp"), root.path().join("doc.txt")).unwrap();
    c.ping();
    let now = c.find("doc.txt").unwrap();
    assert_eq!(now.id, doc.id, "destination id survives rename-over");
    assert!(now.version.content > doc.version.content);
    assert_eq!(now.size, 9);
    assert!(c.find(".doc.tmp").is_none());
    assert!(c
        .events
        .iter()
        .any(|ch| matches!(ch, Change::Remove { id, .. } if *id == tmpe.id)));
    // …and an atomic save whose temp was never observed (same batch).
    let before = c.find("doc.txt").unwrap();
    let mut f = std::fs::File::create(root.path().join("doc.txt.swp")).unwrap();
    f.write_all(b"version 3!").unwrap();
    drop(f);
    std::fs::rename(root.path().join("doc.txt.swp"), root.path().join("doc.txt")).unwrap();
    c.ping();
    let after = c.find("doc.txt").unwrap();
    assert_eq!(after.id, doc.id, "atomic editor save keeps the id");
    assert!(after.version.content > before.version.content);
    assert_eq!(after.size, 10);
}

#[test]
fn rm_then_mkdir_same_name_gets_new_id() {
    let (root, state) = (tmp(), tmp());
    std::fs::write(root.path().join("f"), b"file").unwrap();
    let mut c = session(root.path(), state.path());
    let f = c.find("f").unwrap();
    std::fs::remove_file(root.path().join("f")).unwrap();
    std::fs::create_dir(root.path().join("f")).unwrap();
    c.ping();
    let d = c.find("f").unwrap();
    assert_eq!(d.kind, Kind::Dir);
    assert_ne!(d.id, f.id, "a kind change never reuses an id");
    assert!(!c.replica.contains_key(&f.id));
}

#[test]
fn hardlinks_get_separate_ids_and_share_content_changes() {
    let (root, state) = (tmp(), tmp());
    std::fs::write(root.path().join("a"), b"one").unwrap();
    std::fs::hard_link(root.path().join("a"), root.path().join("b")).unwrap();
    let mut c = session(root.path(), state.path());
    let (a, b) = (c.find("a").unwrap(), c.find("b").unwrap());
    assert_ne!(a.id, b.id);
    // a write through one link dirties every link (inotify only reports one name)
    std::fs::OpenOptions::new()
        .write(true)
        .open(root.path().join("a"))
        .unwrap()
        .write_all(b"two")
        .unwrap();
    c.ping();
    let (a2, b2) = (c.find("a").unwrap(), c.find("b").unwrap());
    assert!(a2.version.content > a.version.content);
    assert!(
        b2.version.content > b.version.content,
        "the other link's content version bumps too"
    );
    // a new hard link created later is its own item; the original keeps its id
    std::fs::hard_link(root.path().join("a"), root.path().join("c")).unwrap();
    c.ping();
    let cc = c.find("c").unwrap();
    assert!(cc.id != a.id && cc.id != b.id);
    assert_eq!(c.find("a").unwrap().id, a.id);
}

#[test]
fn t17_same_size_rewrites_always_bump_content() {
    let (root, state) = (tmp(), tmp());
    let p = root.path().join("hot");
    std::fs::write(&p, format!("{:08}", 0)).unwrap();
    let mut c = session(root.path(), state.path());
    let id = c.find("hot").unwrap().id;
    let mut last = c.stat(id).unwrap().version.content;
    let mut same_mtime = 0;
    let mut last_mtime = 0;
    for i in 1..=1000 {
        std::fs::write(&p, format!("{i:08}")).unwrap();
        let e = c.stat(id).unwrap();
        assert!(
            e.version.content > last,
            "trial {i}: content version did not change"
        );
        assert_eq!(e.size, 8);
        if e.mtime_ns == last_mtime {
            same_mtime += 1;
        }
        last_mtime = e.mtime_ns;
        last = e.version.content;
    }
    eprintln!("T17: 1000/1000 bumped ({same_mtime} rewrites left mtime unchanged)");
}

#[test]
fn hot_file_publication_is_throttled_but_versions_are_live() {
    let (root, state) = (tmp(), tmp());
    let p = root.path().join("log.txt");
    std::fs::write(&p, b"").unwrap();
    let mut c = session(root.path(), state.path());
    let id = c.find("log.txt").unwrap().id;
    let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
    let n0 = c.events.len();
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_millis(1500) {
        f.write_all(b"line\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        c.pump(std::time::Duration::from_millis(0));
    }
    c.pump_until(std::time::Duration::from_millis(300), |_| false);
    let ups = c.events[n0..]
        .iter()
        .filter(|ch| matches!(ch, Change::Upsert(e) if e.id == id))
        .count();
    assert!(ups >= 2, "first change + periodic publication: {ups}");
    assert!(
        ups <= 6,
        "≤ 1/s while writes continue (+ first/final): {ups}"
    );
    // A barrier/stat always sees the live version.
    let live = c.stat(id).unwrap();
    let last_pub = c.replica.get(&id).unwrap().clone();
    assert!(live.version.content >= last_pub.version.content);
    drop(f); // IN_CLOSE_WRITE → final publication
    c.ping();
    assert_eq!(c.replica.get(&id).unwrap().size, live.size);
}

#[test]
fn directory_moved_during_snapshot_keeps_subtree() {
    let (root, state) = (tmp(), tmp());
    make_tree(root.path(), 300, 30);
    std::fs::create_dir_all(root.path().join("zzz/inner/deeper")).unwrap();
    std::fs::write(root.path().join("zzz/inner/deeper/leaf"), b"leaf").unwrap();
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    // Move a not-yet-listed dir into an early (already-sent) one while the snapshot streams.
    std::fs::rename(
        root.path().join("zzz/inner"),
        root.path().join("d0000/inner"),
    )
    .unwrap();
    c.wait_snapshot();
    c.ping();
    let leaf = c
        .find("d0000/inner/deeper/leaf")
        .expect("subtree of the moved dir arrived");
    assert_eq!(leaf.size, 4);
    assert!(c.find("zzz/inner").is_none());
}

#[test]
fn unobserved_dir_move_into_root_is_scanned() {
    let (root, state, outside) = (tmp(), tmp(), tmp());
    std::fs::create_dir_all(outside.path().join("pkg/sub")).unwrap();
    std::fs::write(outside.path().join("pkg/sub/x"), b"x").unwrap();
    let mut c = session(root.path(), state.path());
    if std::fs::rename(outside.path().join("pkg"), root.path().join("pkg")).is_err() {
        return; // different filesystems in this environment
    }
    c.ping();
    assert!(
        c.find("pkg/sub/x").is_some(),
        "an unpaired MOVED_TO gets a full scan + watches"
    );
    // and moved back out: subtree removed
    std::fs::rename(root.path().join("pkg"), outside.path().join("pkg")).unwrap();
    c.ping();
    assert!(c.find("pkg").is_none());
    assert!(!c.replica.values().any(|e| e.name == "x"));
    let _ = ItemId::ROOT;
}
