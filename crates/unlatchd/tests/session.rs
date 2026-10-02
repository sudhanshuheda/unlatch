//! Snapshot / resume / events / listing behaviour over a real `unlatchd stdio` session.

mod common;
use common::*;
use std::time::{Duration, Instant};
use unlatch_proto::wire::{Change, Request, Response, Resume, WelcomeMode};
use unlatch_proto::{ErrorCode, ItemId, Kind};

#[test]
fn snapshot_lists_whole_tree_breadth_first() {
    let root = tmp();
    let state = tmp();
    make_tree(root.path(), 5, 20);
    std::fs::create_dir_all(root.path().join("a/b/c")).unwrap();
    std::fs::write(root.path().join("a/b/c/deep.txt"), b"deep").unwrap();
    std::os::unix::fs::symlink("a/b", root.path().join("link")).unwrap();
    std::fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    assert_eq!(c.welcome().mode, WelcomeMode::Snapshot);
    c.wait_snapshot();
    assert_eq!(c.find("d0003/f0019.txt").unwrap().size, 4);
    let deep = c.find("a/b/c/deep.txt").unwrap();
    assert_eq!(deep.kind, Kind::File);
    let l = c.find("link").unwrap();
    assert_eq!(l.kind, Kind::Symlink);
    assert_eq!(l.symlink_target.as_deref(), Some("a/b"));
    let nm = c.find("node_modules").unwrap();
    assert!(nm.lazy, "lazy by name");
    assert!(
        c.find("node_modules/pkg").is_none(),
        "lazy dirs are not scanned"
    );
    // complete_dirs covers every scanned dir, and root first (BFS).
    assert_eq!(c.complete_dirs.first(), Some(&ItemId::ROOT));
    assert!(c.complete_dirs.contains(&c.find("a/b/c").unwrap().id));
    // every entry carries seq/access
    assert!(c.replica.values().all(|e| e.seq > 0));
    assert!(deep.access & unlatch_proto::ACCESS_R != 0);
}

#[test]
fn events_for_vm_changes_and_barrier() {
    let root = tmp();
    let state = tmp();
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    std::fs::write(root.path().join("new.txt"), b"hello").unwrap();
    std::fs::create_dir(root.path().join("dir")).unwrap();
    std::fs::write(root.path().join("dir/inner"), b"x").unwrap();
    c.ping();
    let f = c.find("new.txt").expect("new file visible after barrier");
    assert_eq!(f.size, 5);
    assert!(c.find("dir/inner").is_some());
    let seqs = c.event_seqs.clone();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "Events.seq strictly increasing: {seqs:?}"
    );
    // rename keeps the id
    std::fs::rename(
        root.path().join("new.txt"),
        root.path().join("dir/moved.txt"),
    )
    .unwrap();
    c.ping();
    assert_eq!(c.find("dir/moved.txt").unwrap().id, f.id);
    assert!(c.find("new.txt").is_none());
    // delete → Remove
    std::fs::remove_dir_all(root.path().join("dir")).unwrap();
    c.ping();
    assert!(c.find("dir").is_none());
    assert!(!c.replica.contains_key(&f.id));
    // latency: a VM write shows up without a barrier within the debounce
    let t0 = Instant::now();
    std::fs::write(root.path().join("fast.txt"), b"1").unwrap();
    assert!(c.pump_until(Duration::from_secs(2), |c| c.find("fast.txt").is_some()));
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "event latency {:?}",
        t0.elapsed()
    );
}

#[test]
fn resume_after_restart_sends_only_changes() {
    let root = tmp();
    let state = tmp();
    make_tree(root.path(), 3, 10);
    // Let the tree age past the racy-timestamp window (D3: entries changed within 20 ms of the
    // last persisted observation get a version bump on restart). On a fast machine the whole
    // first session can fit inside that window, and every file is then — correctly — resent.
    std::thread::sleep(Duration::from_millis(100));
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let index = c.welcome().index;
    let seq = c.ping();
    let a = c.find("d0000/f0000.txt").unwrap();
    assert!(c.close().success());
    // offline changes
    std::fs::write(root.path().join("d0001/added.txt"), b"added").unwrap();
    std::fs::remove_file(root.path().join("d0002/f0003.txt")).unwrap();
    std::fs::rename(
        root.path().join("d0000/f0000.txt"),
        root.path().join("d0001/renamed.txt"),
    )
    .unwrap();
    let mut c = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            resume: Some(Resume { index, seq }),
            ..Default::default()
        },
    );
    assert_eq!(c.welcome().mode, WelcomeMode::Resume);
    assert_eq!(c.welcome().index, index);
    c.ping();
    let ups: Vec<String> = c
        .events
        .iter()
        .filter_map(|ch| match ch {
            Change::Upsert(e) => Some(e.name.clone()),
            _ => None,
        })
        .collect();
    assert!(ups.contains(&"added.txt".to_string()), "{ups:?}");
    assert!(ups.contains(&"renamed.txt".to_string()), "{ups:?}");
    let renamed = c
        .events
        .iter()
        .find_map(|ch| match ch {
            Change::Upsert(e) if e.name == "renamed.txt" => Some(e.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        renamed.id, a.id,
        "moved while the daemon was down: same id (btime identity)"
    );
    assert!(c
        .events
        .iter()
        .any(|ch| matches!(ch, Change::Remove { .. })));
    assert!(ups.len() < 10, "only changed entries are resent: {ups:?}");
    assert_eq!(
        c.stats
            .snapshot_bytes
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[test]
fn stale_or_foreign_resume_gets_snapshot() {
    let root = tmp();
    let state = tmp();
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let index = c.welcome().index;
    c.close();
    let c2 = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            resume: Some(Resume {
                index: unlatch_proto::IndexId(index.0 ^ 1),
                seq: 1,
            }),
            ..Default::default()
        },
    );
    assert_eq!(c2.welcome().mode, WelcomeMode::Snapshot);
    c2.close();
    // client ahead of the server (bogus seq) → snapshot
    let c3 = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            resume: Some(Resume {
                index,
                seq: u64::MAX / 2,
            }),
            ..Default::default()
        },
    );
    assert_eq!(c3.welcome().mode, WelcomeMode::Snapshot);
}

#[test]
fn t16_restart_without_changes_is_resume_fast_zero_snapshot() {
    let root = tmp();
    let state = tmp();
    make_tree(root.path(), 100, 100); // 10k files
                                      // Age the tree past the racy-timestamp window (D3) — see resume_after_restart_sends_only_changes.
    std::thread::sleep(Duration::from_millis(100));
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let index = c.welcome().index;
    let seq = c.ping();
    assert!(c.close().success());
    let log = state.path().join("t16.log");
    let env = vec![(
        "UNLATCHD_LOG".to_string(),
        log.to_string_lossy().into_owned(),
    )];
    let mut c = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            resume: Some(Resume { index, seq }),
            env,
            ..Default::default()
        },
    );
    let w = c.welcome();
    eprintln!("{}", std::fs::read_to_string(&log).unwrap_or_default());
    assert_eq!(w.mode, WelcomeMode::Resume);
    assert!(
        w.elapsed <= Duration::from_millis(200),
        "Welcome took {:?}",
        w.elapsed
    );
    eprintln!("T16: Welcome in {:?} (10k entries)", w.elapsed);
    // let the background verify walk finish, then no changes may have been sent
    std::thread::sleep(Duration::from_millis(300));
    c.ping();
    assert_eq!(
        c.stats
            .snapshot_bytes
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    let ups = c
        .events
        .iter()
        .filter(|ch| matches!(ch, Change::Upsert(e) if e.kind != Kind::Dir))
        .count();
    assert_eq!(
        ups,
        0,
        "no file changes expected: {:?}",
        &c.events[..c.events.len().min(5)]
    );
}

#[test]
fn listdir_of_lazy_dir_is_one_level() {
    let root = tmp();
    let state = tmp();
    let nm = root.path().join("node_modules");
    // T15 shape: 20k files in nested subdirs.
    for p in 0..200 {
        let pd = nm.join(format!("pkg{p:03}"));
        std::fs::create_dir_all(pd.join("lib/deep")).unwrap();
        for f in 0..50 {
            std::fs::write(pd.join(format!("lib/f{f}.js")), b"x").unwrap();
        }
        for f in 0..50 {
            std::fs::write(pd.join(format!("lib/deep/g{f}.js")), b"x").unwrap();
        }
    }
    std::fs::write(nm.join(".package-lock.json"), b"{}").unwrap();
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let nme = c.find("node_modules").unwrap();
    assert!(nme.lazy);
    let watches_before = c.welcome().info.watches;
    let bytes0 = c
        .stats
        .total_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    let (dir, entries) = c.list(nme.id).unwrap();
    let bytes = c
        .stats
        .total_bytes
        .load(std::sync::atomic::Ordering::Relaxed)
        - bytes0;
    assert!(!dir.lazy);
    assert_eq!(entries.len(), 201, "exactly one level");
    assert!(
        entries
            .iter()
            .filter(|e| e.kind == Kind::Dir)
            .all(|e| e.lazy),
        "child dirs come back lazy"
    );
    assert!(bytes < 64 * 1024, "one level only: {bytes} bytes");
    c.ping();
    // no deeper entries were sent at all
    assert!(c
        .replica
        .values()
        .all(|e| !e.name.starts_with('f') || e.parent != nme.id));
    assert!(c.find("node_modules/pkg000/lib").is_none());
    // T15: watches ≤ 1 (+ child dirs are lazy, so no more)
    let (_, _) = c.list(ItemId::ROOT).unwrap();
    let st = std::process::Command::new(BIN)
        .arg("status")
        .arg("--state")
        .arg(state.path())
        .output()
        .unwrap();
    let _ = st;
    let watches_after = count_watches(&c);
    assert!(
        watches_after <= watches_before + 1,
        "watches {watches_before} → {watches_after}"
    );
    // expanding a child continues one level at a time
    let pkg = c.find("node_modules/pkg007").unwrap();
    let (_, kids) = c.list(pkg.id).unwrap();
    assert_eq!(kids.len(), 1);
    assert!(kids[0].lazy);
    // Unwatch collapses back to lazy; relisting keeps ids
    let before: Vec<(String, ItemId)> = entries.iter().map(|e| (e.name.clone(), e.id)).collect();
    match c.call(Request::Unwatch { dir: nme.id }).unwrap() {
        Response::Entry(e) => assert!(e.lazy),
        other => panic!("{other:?}"),
    }
    let (_, again) = c.list(nme.id).unwrap();
    let mut after: Vec<(String, ItemId)> = again.iter().map(|e| (e.name.clone(), e.id)).collect();
    let mut b2 = before.clone();
    b2.sort();
    after.sort();
    assert_eq!(b2, after, "collapse + relist keeps ids");
}

/// Count inotify watches of the child process from /proc.
fn count_watches(c: &Client) -> u64 {
    let pid = c.child.id();
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fdinfo")) {
        for e in rd.flatten() {
            if let Ok(s) = std::fs::read_to_string(e.path()) {
                n += s.lines().filter(|l| l.starts_with("inotify wd:")).count() as u64;
            }
        }
    }
    n
}

#[test]
fn stat_and_errors() {
    let root = tmp();
    let state = tmp();
    std::fs::write(root.path().join("f"), b"abc").unwrap();
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let f = c.find("f").unwrap();
    assert_eq!(c.stat(f.id).unwrap().id, f.id);
    assert_eq!(
        c.stat(ItemId(999_999)).unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(c.list(f.id).unwrap_err().code, ErrorCode::NotDir);
    let root_e = c.stat(ItemId::ROOT).unwrap();
    assert_eq!(root_e.parent, ItemId::ROOT);
}

#[test]
fn wrong_root_is_rejected() {
    let root = tmp();
    let other = tmp();
    let state = tmp();
    // Server serves `root`, client asks for `other`.
    let mut cmd = std::process::Command::new(BIN);
    let _ = &mut cmd;
    let err = {
        use std::io::Write;
        use unlatch_proto::frame::{self, Preamble};
        use unlatch_proto::wire::{ClientMsg, ServerMsg};
        let mut child = std::process::Command::new(BIN)
            .arg("stdio")
            .arg("--root")
            .arg(root.path())
            .arg("--state")
            .arg(state.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut i = child.stdin.take().unwrap();
        let mut o = child.stdout.take().unwrap();
        i.write_all(
            &Preamble {
                proto_min: 1,
                proto_max: 1,
                build_id: [0; 16],
            }
            .to_bytes(),
        )
        .unwrap();
        let hello = ClientMsg::Hello {
            proto: 1,
            root: other.path().to_string_lossy().into_owned(),
            resume: None,
            expect_index: None,
            default_lazy_names: vec![],
            client_name: "x".into(),
        };
        i.write_all(&frame::encode(&hello, false).unwrap()).unwrap();
        frame::read_preamble_blocking(&mut o).unwrap();
        let m: ServerMsg = frame::read_blocking(&mut o).unwrap().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        match m {
            ServerMsg::Error { err, req_id: None } => err,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(err.code, ErrorCode::Protocol);
}

#[test]
fn shell_junk_before_preamble_is_tolerated_by_version_check() {
    // A client that speaks no common version gets a Protocol error, not a hang.
    use std::io::Write;
    use unlatch_proto::frame::{self, Preamble};
    use unlatch_proto::wire::ServerMsg;
    let root = tmp();
    let state = tmp();
    let mut child = std::process::Command::new(BIN)
        .arg("stdio")
        .arg("--root")
        .arg(root.path())
        .arg("--state")
        .arg(state.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut i = child.stdin.take().unwrap();
    let mut o = child.stdout.take().unwrap();
    i.write_all(b"garbage from bashrc\n").unwrap();
    i.write_all(
        &Preamble {
            proto_min: 7,
            proto_max: 9,
            build_id: [0; 16],
        }
        .to_bytes(),
    )
    .unwrap();
    i.flush().unwrap();
    frame::read_preamble_blocking(&mut o).unwrap();
    let m: ServerMsg = frame::read_blocking(&mut o).unwrap().unwrap();
    assert!(matches!(m, ServerMsg::Error { req_id: None, err } if err.code == ErrorCode::Protocol));
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn watch_budget_exhaustion_falls_back_to_polling() {
    let root = tmp();
    let state = tmp();
    make_tree(root.path(), 6, 2);
    let env = vec![("UNLATCHD_MAX_WATCHES".to_string(), "2".to_string())];
    // A fresh index is scanned before Welcome, so its warnings are current.
    let mut c = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            env,
            ..Default::default()
        },
    );
    c.wait_snapshot();
    let warnings = c.welcome().info.warnings.clone();
    assert!(
        warnings.iter().any(|w| w.contains("polled")),
        "{warnings:?}"
    );
    assert!(c.welcome().info.watches <= 2);
    // Every directory still converges (watched or polled), and the Ping barrier covers polled
    // dirs too (Pong contract): no waiting on the adaptive 1–30 s poll schedule, whatever the
    // load on the box.
    for d in 0..6 {
        std::fs::write(root.path().join(format!("d{d:04}/new.txt")), b"n").unwrap();
    }
    c.ping();
    for d in 0..6 {
        assert!(
            c.find(&format!("d{d:04}/new.txt")).is_some(),
            "d{d:04}/new.txt not visible after the barrier"
        );
    }
    // Content changes and removals in polled dirs as well.
    for d in 0..6 {
        std::fs::write(root.path().join(format!("d{d:04}/new.txt")), b"longer").unwrap();
        std::fs::remove_file(root.path().join(format!("d{d:04}/f0000.txt"))).unwrap();
    }
    c.ping();
    for d in 0..6 {
        assert_eq!(c.find(&format!("d{d:04}/new.txt")).map(|e| e.size), Some(6));
        assert!(c.find(&format!("d{d:04}/f0000.txt")).is_none());
    }
    // Without a barrier the adaptive poll still gets there on its own.
    std::fs::write(root.path().join("d0005/late.txt"), b"l").unwrap();
    assert!(
        c.pump_until(Duration::from_secs(120), |c| c
            .find("d0005/late.txt")
            .is_some()),
        "polled dir never re-listed"
    );
}

#[test]
fn root_replaced_ends_the_session_and_gets_a_new_index() {
    let base = tmp();
    let state = tmp();
    let root = base.path().join("proj");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a"), b"a").unwrap();
    let mut c = Client::spawn(&root, state.path(), Opts::default());
    c.wait_snapshot();
    let index = c.welcome().index;
    let seq = c.ping();
    // `rm -rf proj && git clone … proj`
    std::fs::rename(&root, base.path().join("proj.old")).unwrap();
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("b"), b"b").unwrap();
    assert!(
        c.pump_until(Duration::from_secs(5), |c| c.session_error.is_some()),
        "RootReplaced not sent"
    );
    assert_eq!(
        c.session_error.as_ref().unwrap().code,
        ErrorCode::RootReplaced
    );
    drop(c);
    let mut c = Client::spawn(
        &root,
        state.path(),
        Opts {
            resume: Some(Resume { index, seq }),
            ..Default::default()
        },
    );
    assert_eq!(c.welcome().mode, WelcomeMode::Snapshot);
    assert_ne!(
        c.welcome().index,
        index,
        "a replaced root gets a new index id"
    );
    c.wait_snapshot();
    assert!(c.find("b").is_some());
    assert!(c.find("a").is_none());
}

/// `UNLATCH_FAULT=overflow_after_events:<n>` (D14): the reader drops everything after the n-th
/// event and reports IN_Q_OVERFLOW; the overflow reconcile re-lists every watched directory,
/// the tree converges and ids survive (a rename whose events were lost keeps its id).
#[test]
fn injected_queue_overflow_reconciles_and_keeps_ids() {
    let root = tmp();
    let state = tmp();
    std::fs::create_dir(root.path().join("d")).unwrap();
    for n in ["a", "b", "c"] {
        std::fs::write(root.path().join("d").join(n), n.as_bytes()).unwrap();
    }
    let env = vec![(
        "UNLATCH_FAULT".to_string(),
        "overflow_after_events:1".to_string(),
    )];
    let mut c = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            env,
            ..Default::default()
        },
    );
    c.wait_snapshot();
    let a = c.find("d/a").unwrap();
    let cc = c.find("d/c").unwrap();
    let d = root.path().join("d");
    std::fs::write(d.join("new"), b"new").unwrap();
    std::fs::rename(d.join("a"), d.join("a2")).unwrap();
    std::fs::remove_file(d.join("b")).unwrap();
    std::fs::write(d.join("c"), b"c grew").unwrap();
    std::fs::create_dir(d.join("sub")).unwrap();
    std::fs::write(d.join("sub/x"), b"x").unwrap();
    c.ping();
    assert!(c.find("d/new").is_some());
    assert!(c.find("d/a").is_none());
    assert_eq!(
        c.find("d/a2").map(|e| e.id),
        Some(a.id),
        "rename kept its id"
    );
    assert!(c.find("d/b").is_none());
    let c2 = c.find("d/c").unwrap();
    assert_eq!(c2.id, cc.id);
    assert_eq!(c2.size, 6);
    assert!(c2.version.content > cc.version.content);
    assert!(
        c.find("d/sub/x").is_some(),
        "new dir scanned after the overflow"
    );
    // Watching continues normally after the overflow.
    std::fs::write(d.join("later"), b"l").unwrap();
    c.ping();
    assert!(c.find("d/later").is_some());
}

/// A directory moved while no daemon watched it (crash, VM reboot) is found moved by the startup
/// verify walk; it must be re-listed and re-watched there like one that stayed in place, or
/// nothing written into it later is ever seen (fuzz seed 58: `docs.old/notes`).
#[test]
fn dir_moved_while_down_is_watched_after_restart() {
    let root = tmp();
    let state = tmp();
    std::fs::create_dir_all(root.path().join("docs/sub")).unwrap();
    std::fs::write(root.path().join("docs/a"), b"a").unwrap();
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let docs = c.find("docs").unwrap();
    assert!(c.close().success());
    std::fs::rename(root.path().join("docs"), root.path().join("docs.old")).unwrap();
    std::os::unix::fs::symlink("/nonexistent", root.path().join("docs")).unwrap();
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    c.ping();
    assert_eq!(
        c.find("docs.old").map(|e| e.id),
        Some(docs.id),
        "moved, same id"
    );
    std::fs::write(root.path().join("docs.old/notes"), b"n").unwrap();
    std::fs::write(root.path().join("docs.old/sub/deep"), b"d").unwrap();
    c.ping();
    assert!(c.find("docs.old/notes").is_some(), "moved dir not watched");
    assert!(
        c.find("docs.old/sub/deep").is_some(),
        "its subdir not watched"
    );
}
