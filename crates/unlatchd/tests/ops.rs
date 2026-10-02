//! Mutations over a real `unlatchd stdio` session: idempotent ops, conflict rules, base checks,
//! kept-on-remove semantics, fault injection + replay, path-escape safety.

mod common;
use common::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use unlatch_proto::wire::{Request, Response};
use unlatch_proto::{ErrorCode, ItemId, Kind, Version};

fn written(
    r: Result<Response, unlatch_proto::ProtoError>,
) -> (unlatch_proto::Entry, Option<unlatch_proto::Entry>) {
    match r {
        Ok(Response::Written {
            entry,
            conflict_copy,
        }) => (entry, conflict_copy),
        other => panic!("expected Written, got {other:?}"),
    }
}

fn entry(r: Result<Response, unlatch_proto::ProtoError>) -> unlatch_proto::Entry {
    match r {
        Ok(Response::Entry(e)) => e,
        other => panic!("expected Entry, got {other:?}"),
    }
}

fn session(root: &std::path::Path, state: &std::path::Path) -> Client {
    let mut c = Client::spawn(root, state, Opts::default());
    c.wait_snapshot();
    c
}

#[test]
fn create_then_replay_returns_stored_result() {
    let (root, state) = (tmp(), tmp());
    let mut c = session(root.path(), state.path());
    let (e, cc) = written(c.write(op(1), ItemId::ROOT, "a.txt", None, None, b"hello", false));
    assert!(cc.is_none());
    assert_eq!(std::fs::read(root.path().join("a.txt")).unwrap(), b"hello");
    assert_eq!(e.size, 5);
    assert_eq!(e.name, "a.txt");
    // Replay (lost reply): the stored response comes back, nothing is created twice.
    let (e2, _) = written(c.write(op(1), ItemId::ROOT, "a.txt", None, None, b"hello", false));
    assert_eq!(e2, e);
    let names: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .flatten()
        .map(|d| d.file_name())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    // A different op for the same name → Exists (never overwrites)…
    assert_eq!(
        c.write(op(2), ItemId::ROOT, "a.txt", None, None, b"other", false)
            .unwrap_err()
            .code,
        ErrorCode::Exists
    );
    assert_eq!(std::fs::read(root.path().join("a.txt")).unwrap(), b"hello");
    // …unless may_exist and the content is identical.
    let (e3, _) = written(c.write(op(3), ItemId::ROOT, "a.txt", None, None, b"hello", true));
    assert_eq!(e3.id, e.id);
    assert_eq!(
        c.write(op(4), ItemId::ROOT, "a.txt", None, None, b"diff", true)
            .unwrap_err()
            .code,
        ErrorCode::Exists
    );
    // Hash mismatch is rejected and nothing is published.
    let id = c.request(Request::Write {
        op: op(5),
        parent: ItemId::ROOT,
        name: "bad.txt".into(),
        target: None,
        base: None,
        size: 3,
        content_hash: [0; 32],
        mtime_ns: None,
        exec: None,
        move_to: None,
        may_exist: false,
    });
    c.send(&unlatch_proto::wire::ClientMsg::WriteChunk {
        req_id: id,
        data: b"abc".to_vec(),
        last: true,
    });
    assert_eq!(c.response(id).unwrap_err().code, ErrorCode::Io);
    assert!(!root.path().join("bad.txt").exists());
    // Empty file.
    let (z, _) = written(c.write(op(6), ItemId::ROOT, "empty", None, None, b"", false));
    assert_eq!(z.size, 0);
    // Uploads get credit back.
    c.ping();
    assert!(c.credits >= 768 * 1024);
}

#[test]
fn replace_keeps_id_mode_and_detects_conflicts() {
    let (root, state) = (tmp(), tmp());
    let p = root.path().join("run.sh");
    std::fs::write(&p, b"echo 1\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut c = session(root.path(), state.path());
    let f = c.find("run.sh").unwrap();
    // Correct base → replaced in place, id and mode kept.
    let (e, cc) = written(c.write(
        op(10),
        ItemId::ROOT,
        "run.sh",
        Some(f.id),
        Some(f.version.content),
        b"echo 2\n",
        false,
    ));
    assert!(cc.is_none());
    assert_eq!(e.id, f.id);
    assert!(e.version.content > f.version.content);
    assert_eq!(std::fs::read(&p).unwrap(), b"echo 2\n");
    assert_eq!(
        std::fs::metadata(&p).unwrap().mode() & 0o7777,
        0o755,
        "exec bit kept"
    );
    assert!(!std::fs::read_dir(root.path())
        .unwrap()
        .flatten()
        .any(|d| d.file_name().to_string_lossy().starts_with(".unlatch-")));
    c.ping();
    assert_eq!(
        c.find("run.sh").unwrap().id,
        f.id,
        "the watcher agrees: replace keeps the id"
    );
    // Stale base + different bytes → conflict copy, target untouched.
    let stale = f.version.content;
    let (e2, cc2) = written(c.write(
        op(11),
        ItemId::ROOT,
        "run.sh",
        Some(f.id),
        Some(stale),
        b"mac edit\n",
        false,
    ));
    let copy = cc2.expect("conflict copy");
    assert_eq!(e2.id, f.id);
    assert_eq!(
        std::fs::read(&p).unwrap(),
        b"echo 2\n",
        "agent version survives"
    );
    assert!(
        copy.name.starts_with("run (conflict from testmac "),
        "{}",
        copy.name
    );
    assert!(copy.name.ends_with(".sh"));
    assert_eq!(
        std::fs::read(root.path().join(&copy.name)).unwrap(),
        b"mac edit\n"
    );
    // Stale base but identical bytes → success, no conflict copy.
    let (e3, cc3) = written(c.write(
        op(12),
        ItemId::ROOT,
        "run.sh",
        Some(f.id),
        Some(stale),
        b"echo 2\n",
        false,
    ));
    assert!(cc3.is_none());
    assert_eq!(e3.id, f.id);
    // exec: Some(false) clears x bits.
    let cur = c.stat(f.id).unwrap();
    let id = c.start_write(
        op(13),
        ItemId::ROOT,
        "run.sh",
        Some(f.id),
        Some(cur.version.content),
        b"x",
        false,
        Some(false),
        None,
    );
    written(c.response(id));
    assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o111, 0);
}

#[test]
fn agent_write_racing_a_mac_save_is_never_lost() {
    let (root, state) = (tmp(), tmp());
    let p = root.path().join("notes.md");
    std::fs::write(&p, b"v1").unwrap();
    let mut c = session(root.path(), state.path());
    let f = c.find("notes.md").unwrap();
    // The agent writes; the Mac saves against the old base before seeing the event.
    std::fs::write(&p, b"agent v2").unwrap();
    let (_, cc) = written(c.write(
        op(20),
        ItemId::ROOT,
        "notes.md",
        Some(f.id),
        Some(f.version.content),
        b"mac v2",
        false,
    ));
    assert!(
        cc.is_some(),
        "base checked against the live file, not the debounced index"
    );
    assert_eq!(std::fs::read(&p).unwrap(), b"agent v2");
}

#[test]
fn mkdir_symlink_rename_setattr() {
    let (root, state) = (tmp(), tmp());
    let mut c = session(root.path(), state.path());
    let d = entry(c.call(Request::Mkdir {
        op: op(30),
        parent: ItemId::ROOT,
        name: "dir".into(),
        may_exist: false,
    }));
    assert_eq!(d.kind, Kind::Dir);
    assert!(!d.lazy);
    assert_eq!(
        c.call(Request::Mkdir {
            op: op(31),
            parent: ItemId::ROOT,
            name: "dir".into(),
            may_exist: false
        })
        .unwrap_err()
        .code,
        ErrorCode::Exists
    );
    let d2 = entry(c.call(Request::Mkdir {
        op: op(32),
        parent: ItemId::ROOT,
        name: "dir".into(),
        may_exist: true,
    }));
    assert_eq!(d2.id, d.id);
    let l = entry(c.call(Request::Symlink {
        op: op(33),
        parent: d.id,
        name: "l".into(),
        target: "../x".into(),
    }));
    assert_eq!(l.kind, Kind::Symlink);
    assert_eq!(
        std::fs::read_link(root.path().join("dir/l"))
            .unwrap()
            .to_string_lossy(),
        "../x"
    );
    let (f, _) = written(c.write(op(34), ItemId::ROOT, "f", None, None, b"x", false));
    // Rename with a stale base location → not applied, server state returned.
    match c.call(Request::Rename {
        op: op(35),
        id: f.id,
        base_parent: d.id,
        base_name: "f".into(),
        new_parent: ItemId::ROOT,
        new_name: "g".into(),
    }) {
        Ok(Response::Renamed {
            entry,
            applied: false,
        }) => assert_eq!(entry.name, "f"),
        other => panic!("{other:?}"),
    }
    // Real rename; id kept.
    match c.call(Request::Rename {
        op: op(36),
        id: f.id,
        base_parent: ItemId::ROOT,
        base_name: "f".into(),
        new_parent: d.id,
        new_name: "g".into(),
    }) {
        Ok(Response::Renamed {
            entry,
            applied: true,
        }) => {
            assert_eq!(entry.id, f.id);
            assert_eq!(entry.parent, d.id);
            assert!(entry.version.meta > f.version.meta);
            assert_eq!(
                entry.version.content, f.version.content,
                "rename is not a content change"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(root.path().join("dir/g").exists());
    // Replay of the rename → stored success (not a base mismatch).
    match c.call(Request::Rename {
        op: op(36),
        id: f.id,
        base_parent: ItemId::ROOT,
        base_name: "f".into(),
        new_parent: d.id,
        new_name: "g".into(),
    }) {
        Ok(Response::Renamed { applied: true, .. }) => {}
        other => panic!("{other:?}"),
    }
    // RENAME_NOREPLACE: never overwrites.
    std::fs::write(root.path().join("taken"), b"t").unwrap();
    c.ping();
    let taken = c.find("taken").unwrap();
    let _ = taken;
    assert_eq!(
        c.call(Request::Rename {
            op: op(37),
            id: f.id,
            base_parent: d.id,
            base_name: "g".into(),
            new_parent: ItemId::ROOT,
            new_name: "taken".into(),
        })
        .unwrap_err()
        .code,
        ErrorCode::Exists
    );
    assert_eq!(std::fs::read(root.path().join("taken")).unwrap(), b"t");
    // SetAttr exec + mtime
    let e = entry(c.call(Request::SetAttr {
        op: op(38),
        id: f.id,
        exec: Some(true),
        mtime_ns: Some(1_600_000_000_000_000_000),
    }));
    assert_eq!(e.mode & 0o100, 0o100);
    assert_eq!(e.mtime_ns, 1_600_000_000_000_000_000);
    let md = std::fs::metadata(root.path().join("dir/g")).unwrap();
    assert_eq!(md.mode() & 0o100, 0o100);
    // invalid names
    assert_eq!(
        c.call(Request::Mkdir {
            op: op(39),
            parent: ItemId::ROOT,
            name: "a/b".into(),
            may_exist: false
        })
        .unwrap_err()
        .code,
        ErrorCode::InvalidName
    );
    assert_eq!(
        c.call(Request::Mkdir {
            op: op(40),
            parent: ItemId::ROOT,
            name: "..".into(),
            may_exist: false
        })
        .unwrap_err()
        .code,
        ErrorCode::InvalidName
    );
}

#[test]
fn remove_checks_base_and_keeps_newer_entries() {
    let (root, state) = (tmp(), tmp());
    std::fs::create_dir_all(root.path().join("proj/sub")).unwrap();
    std::fs::write(root.path().join("proj/old.txt"), b"o").unwrap();
    std::fs::write(root.path().join("proj/sub/old2.txt"), b"o").unwrap();
    std::fs::write(root.path().join("lone.txt"), b"l").unwrap();
    let mut c = session(root.path(), state.path());
    let seen = c.ping();
    let proj = c.find("proj").unwrap();
    let lone = c.find("lone.txt").unwrap();
    // File: base mismatch → VersionMismatch; nothing deleted.
    let bad = Version {
        content: lone.version.content + 1000,
        meta: lone.version.meta,
    };
    assert_eq!(
        c.call(Request::Remove {
            op: op(50),
            id: lone.id,
            base: bad,
            recursive: false,
            seen_seq: seen
        })
        .unwrap_err()
        .code,
        ErrorCode::VersionMismatch
    );
    assert!(root.path().join("lone.txt").exists());
    match c.call(Request::Remove {
        op: op(51),
        id: lone.id,
        base: lone.version,
        recursive: false,
        seen_seq: seen,
    }) {
        Ok(Response::Removed { kept }) => assert!(kept.is_empty()),
        other => panic!("{other:?}"),
    }
    assert!(!root.path().join("lone.txt").exists());
    // Dir without recursive → NotEmpty.
    assert_eq!(
        c.call(Request::Remove {
            op: op(52),
            id: proj.id,
            base: proj.version,
            recursive: false,
            seen_seq: seen
        })
        .unwrap_err()
        .code,
        ErrorCode::NotEmpty
    );
    // The agent writes a new file after the client's seen point.
    std::fs::write(root.path().join("proj/sub/agent-new.txt"), b"precious").unwrap();
    let proj_now = c.stat(proj.id).unwrap();
    let r = c.call(Request::Remove {
        op: op(53),
        id: proj.id,
        base: proj_now.version,
        recursive: true,
        seen_seq: seen,
    });
    let kept = match r {
        Ok(Response::Removed { kept }) => kept,
        other => panic!("{other:?}"),
    };
    assert!(
        root.path().join("proj/sub/agent-new.txt").exists(),
        "agent file survives rm -rf"
    );
    assert!(!root.path().join("proj/old.txt").exists());
    assert!(!root.path().join("proj/sub/old2.txt").exists());
    assert!(!kept.is_empty());
    assert!(
        kept.contains(&proj.id),
        "ancestors of kept entries are reported: {kept:?}"
    );
    c.ping();
    let sub = c.find("proj/sub").unwrap();
    assert!(kept.contains(&sub.id));
    // Replay returns the stored result.
    match c.call(Request::Remove {
        op: op(53),
        id: proj.id,
        base: proj_now.version,
        recursive: true,
        seen_seq: seen,
    }) {
        Ok(Response::Removed { kept: k2 }) => assert_eq!(k2, kept),
        other => panic!("{other:?}"),
    }
    // A full recursive delete of an old tree.
    std::fs::create_dir_all(root.path().join("old/a/b")).unwrap();
    std::fs::write(root.path().join("old/a/b/f"), b"x").unwrap();
    c.ping();
    let old = c.find("old").unwrap();
    let seen2 = c.ping();
    match c.call(Request::Remove {
        op: op(54),
        id: old.id,
        base: old.version,
        recursive: true,
        seen_seq: seen2,
    }) {
        Ok(Response::Removed { kept }) => assert!(kept.is_empty(), "{kept:?}"),
        other => panic!("{other:?}"),
    }
    assert!(!root.path().join("old").exists());
    c.ping();
    assert!(c.find("old").is_none());
}

#[test]
fn recursive_remove_never_follows_symlinks() {
    let (root, state, outside) = (tmp(), tmp(), tmp());
    std::fs::write(outside.path().join("keep.txt"), b"outside").unwrap();
    std::fs::create_dir_all(root.path().join("d")).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("d/escape")).unwrap();
    let mut c = session(root.path(), state.path());
    let seen = c.ping();
    let d = c.find("d").unwrap();
    match c.call(Request::Remove {
        op: op(60),
        id: d.id,
        base: d.version,
        recursive: true,
        seen_seq: seen,
    }) {
        Ok(Response::Removed { kept }) => assert!(kept.is_empty()),
        other => panic!("{other:?}"),
    }
    assert!(outside.path().join("keep.txt").exists());
    assert!(!root.path().join("d").exists());
}

#[test]
fn intermediate_dir_swapped_for_symlink_never_escapes() {
    let (root, state, outside) = (tmp(), tmp(), tmp());
    std::fs::create_dir_all(root.path().join("a/b")).unwrap();
    std::fs::write(root.path().join("a/b/file"), b"inside").unwrap();
    std::fs::create_dir_all(outside.path().join("b")).unwrap();
    std::fs::write(outside.path().join("b/file"), b"outside").unwrap();
    // Freeze the watcher's view: debounce long enough that the swap is not yet indexed when
    // the ops run… the flush would see it, so instead test the stale-id path explicitly: the
    // op addresses ids whose indexed path now crosses a symlink.
    let mut c = session(root.path(), state.path());
    let file = c.find("a/b/file").unwrap();
    let b = c.find("a/b").unwrap();
    std::fs::rename(root.path().join("a"), root.path().join("a.old")).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("a")).unwrap();
    // All of these must act inside the root (the flush follows the rename) or fail.
    let r = c.write(op(70), b.id, "new", None, None, b"n", false);
    let _ = r;
    let rr = c.write(
        op(71),
        ItemId::ROOT,
        "x",
        Some(file.id),
        Some(file.version.content),
        b"clobber",
        false,
    );
    let _ = rr;
    let seen = c.ping();
    let cur_b = c.stat(b.id);
    if let Ok(cb) = cur_b {
        let _ = c.call(Request::Remove {
            op: op(72),
            id: b.id,
            base: cb.version,
            recursive: true,
            seen_seq: seen,
        });
    }
    assert_eq!(
        std::fs::read(outside.path().join("b/file")).unwrap(),
        b"outside",
        "nothing outside the root touched"
    );
    assert!(!outside.path().join("b/new").exists());
    assert_eq!(
        std::fs::read_dir(outside.path().join("b")).unwrap().count(),
        1
    );
    // the moved dir kept its id
    c.ping();
    assert_eq!(
        c.find("a.old/b").map(|e| e.id),
        c.replica.get(&b.id).map(|e| e.id)
    );
}

#[test]
fn die_after_commit_then_replay_gives_same_result() {
    for (name, kind) in [
        ("write", 0),
        ("mkdir", 1),
        ("rename", 2),
        ("remove", 3),
        ("setattr", 4),
        ("symlink", 5),
    ] {
        let (root, state) = (tmp(), tmp());
        std::fs::write(root.path().join("existing"), b"e").unwrap();
        let c = session(root.path(), state.path());
        let ex = c.find("existing").unwrap();
        c.close();
        let env = vec![
            (
                "UNLATCH_FAULT".to_string(),
                format!("die_after_commit:{name}"),
            ),
            (
                "UNLATCHD_LOG".to_string(),
                state
                    .path()
                    .join("fault.log")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ];
        let mut c = Client::spawn(
            root.path(),
            state.path(),
            Opts {
                env,
                ..Default::default()
            },
        );
        c.wait_snapshot();
        let seen = c.ping();
        // The restart may bump versions (racy-git rule for files written just before the
        // first scan): use the live entry.
        let ex = c.stat(ex.id).unwrap();
        let req = |o| match kind {
            0 => Request::Write {
                op: o,
                parent: ItemId::ROOT,
                name: "new.txt".into(),
                target: None,
                base: None,
                size: 3,
                content_hash: *blake3::hash(b"abc").as_bytes(),
                mtime_ns: None,
                exec: None,
                move_to: None,
                may_exist: false,
            },
            1 => Request::Mkdir {
                op: o,
                parent: ItemId::ROOT,
                name: "nd".into(),
                may_exist: false,
            },
            2 => Request::Rename {
                op: o,
                id: ex.id,
                base_parent: ItemId::ROOT,
                base_name: "existing".into(),
                new_parent: ItemId::ROOT,
                new_name: "renamed".into(),
            },
            3 => Request::Remove {
                op: o,
                id: ex.id,
                base: ex.version,
                recursive: false,
                seen_seq: seen,
            },
            4 => Request::SetAttr {
                op: o,
                id: ex.id,
                exec: Some(true),
                mtime_ns: None,
            },
            _ => Request::Symlink {
                op: o,
                parent: ItemId::ROOT,
                name: "sl".into(),
                target: "existing".into(),
            },
        };
        let rid = c.request(req(op(80)));
        if kind == 0 {
            c.send(&unlatch_proto::wire::ClientMsg::WriteChunk {
                req_id: rid,
                data: b"abc".to_vec(),
                last: true,
            });
        }
        let Some(status) = c.wait_exit(T) else {
            panic!(
                "{name}: unlatchd must die; log:\n{}",
                std::fs::read_to_string(state.path().join("fault.log")).unwrap_or_default()
            );
        };
        assert_eq!(status.code(), Some(99), "{name}");
        assert!(
            c.next_for(rid, std::time::Duration::from_millis(100))
                .is_none(),
            "{name}: no reply before death"
        );
        drop(c);
        // Restart without the fault; replay the same op.
        let mut c = session(root.path(), state.path());
        let rid = c.request(req(op(80)));
        if kind == 0 {
            c.send(&unlatch_proto::wire::ClientMsg::WriteChunk {
                req_id: rid,
                data: b"abc".to_vec(),
                last: true,
            });
        }
        let resp = c
            .response(rid)
            .unwrap_or_else(|e| panic!("{name}: replay failed: {e:?}"));
        c.ping();
        match (kind, resp) {
            (
                0,
                Response::Written {
                    entry,
                    conflict_copy: None,
                },
            ) => {
                assert_eq!(
                    c.find("new.txt").unwrap().id,
                    entry.id,
                    "same id after restart"
                );
                assert_eq!(std::fs::read(root.path().join("new.txt")).unwrap(), b"abc");
            }
            (1, Response::Entry(e)) => assert_eq!(c.find("nd").unwrap().id, e.id),
            (
                2,
                Response::Renamed {
                    entry,
                    applied: true,
                },
            ) => {
                assert_eq!(entry.id, ex.id);
                assert_eq!(c.find("renamed").unwrap().id, ex.id);
            }
            (3, Response::Removed { kept }) => {
                assert!(kept.is_empty());
                assert!(!root.path().join("existing").exists());
            }
            (4, Response::Entry(e)) => assert_eq!(e.id, ex.id),
            (5, Response::Entry(e)) => assert_eq!(c.find("sl").unwrap().id, e.id),
            (k, r) => panic!("{name} ({k}): unexpected {r:?}"),
        }
        let n = std::fs::read_dir(root.path()).unwrap().count();
        assert!(n <= 2, "{name}: no duplicates ({n} entries)");
    }
}

/// Review (a)3: files with nlink > 1 get one id per link, and a change to the inode dirties
/// every link. A write through one link must update the other link's entry too.
#[test]
fn write_through_one_hardlink_updates_every_link() {
    let (root, state) = (tmp(), tmp());
    std::fs::create_dir(root.path().join("d")).unwrap();
    std::fs::write(root.path().join("d/f.txt"), b"v1").unwrap();
    let mut c = session(root.path(), state.path());
    std::fs::hard_link(root.path().join("d/f.txt"), root.path().join("d/h.txt")).unwrap();
    c.ping();
    let f = c.find("d/f.txt").unwrap();
    let h = c.find("d/h.txt").unwrap();
    assert_ne!(f.id, h.id, "one id per link");
    let (e, cc) = written(c.write(
        op(40),
        h.parent,
        "h.txt",
        Some(h.id),
        Some(h.version.content),
        b"mac edit via link",
        false,
    ));
    assert!(cc.is_none());
    assert_eq!(e.size, 17);
    assert_eq!(
        std::fs::read(root.path().join("d/f.txt")).unwrap(),
        b"mac edit via link",
        "links stay linked"
    );
    c.ping();
    let f2 = c.find("d/f.txt").unwrap();
    assert_eq!(f2.id, f.id);
    assert_eq!(f2.size, 17, "the other link's entry follows the write");
    assert!(f2.version.content > f.version.content);
    // An agent write through the other link updates both as well.
    std::fs::write(root.path().join("d/f.txt"), b"agent").unwrap();
    c.ping();
    assert_eq!(c.find("d/h.txt").unwrap().size, 5);
    assert_eq!(c.find("d/f.txt").unwrap().size, 5);
    // Unlinking one link makes the other a single-link file again (no event names it): a later
    // rename then keeps its id (identity by inode again).
    std::fs::remove_file(root.path().join("d/h.txt")).unwrap();
    c.ping();
    std::fs::rename(root.path().join("d/f.txt"), root.path().join("d/g.txt")).unwrap();
    c.ping();
    assert_eq!(c.find("d/g.txt").map(|e| e.id), Some(f.id));
}

/// A folder the client creates is one the client lists from then on (fileproviderd never
/// enumerates a folder it made itself): even under a lazy name it must be scanned and watched,
/// or files the agent writes into it never reach the Mac (fuzz seed 1).
#[test]
fn mkdir_of_a_lazy_name_is_watched() {
    let (root, state) = (tmp(), tmp());
    let mut c = session(root.path(), state.path());
    let nm = entry(c.call(Request::Mkdir {
        op: op(50),
        parent: ItemId::ROOT,
        name: "node_modules".into(),
        may_exist: false,
    }));
    assert!(!nm.lazy, "the creator holds its (empty) listing: not lazy");
    c.ping();
    assert!(
        !c.events.iter().any(|ch| matches!(
            ch,
            unlatch_proto::wire::Change::Upsert(e) if e.id == nm.id && e.lazy
        )),
        "a client-made dir is never published as lazy (the engine would never list it)"
    );
    std::fs::write(root.path().join("node_modules/x"), b"agent").unwrap();
    std::fs::create_dir(root.path().join("node_modules/pkg")).unwrap();
    c.ping();
    assert_eq!(c.find("node_modules/x").map(|e| e.size), Some(5));
    assert!(
        c.find("node_modules/pkg").unwrap().lazy,
        "D13: one level only"
    );
    std::os::unix::fs::symlink("docs/", root.path().join("node_modules/l")).unwrap();
    c.ping();
    assert!(
        c.find("node_modules/l").is_some(),
        "dangling in-root symlink"
    );
    // A folder the client creates inside an expanded lazy dir is watched too.
    let pkg2 = entry(c.call(Request::Mkdir {
        op: op(51),
        parent: nm.id,
        name: "pkg2".into(),
        may_exist: false,
    }));
    assert!(!pkg2.lazy);
    std::fs::write(root.path().join("node_modules/pkg2/index.js"), b"js").unwrap();
    c.ping();
    assert!(c.find("node_modules/pkg2/index.js").is_some());
}

/// Moving an item into a folder that is gone on the VM (fuzz seed 13: Finder moved a file into a
/// folder the agent had just `rm -rf`ed) is not an error about the *item*: NotFound would make
/// the engine delete it on the Mac although it still exists. The reply is the item's current
/// state, `applied: false` (metadata never errors); a content write with `move_to` there lands
/// in place.
#[test]
fn move_into_a_vanished_folder_reports_the_item_where_it_is() {
    let (root, state) = (tmp(), tmp());
    std::fs::create_dir(root.path().join("a")).unwrap();
    std::fs::create_dir(root.path().join("gone")).unwrap();
    std::fs::write(root.path().join("a/f"), b"f").unwrap();
    let mut c = session(root.path(), state.path());
    let a = c.find("a").unwrap();
    let f = c.find("a/f").unwrap();
    let gone = c.find("gone").unwrap();
    std::fs::remove_dir(root.path().join("gone")).unwrap();
    match c.call(Request::Rename {
        op: op(60),
        id: f.id,
        base_parent: a.id,
        base_name: "f".into(),
        new_parent: gone.id,
        new_name: "f".into(),
    }) {
        Ok(Response::Renamed {
            entry,
            applied: false,
        }) => {
            assert_eq!(entry.id, f.id);
            assert_eq!(entry.parent, a.id);
            assert_eq!(entry.name, "f");
        }
        other => panic!("expected Renamed{{applied: false}}, got {other:?}"),
    }
    assert_eq!(std::fs::read(root.path().join("a/f")).unwrap(), b"f");
    // Content + move to the vanished folder: the content lands in place.
    let id = c.start_write(
        op(61),
        a.id,
        "f",
        Some(f.id),
        Some(f.version.content),
        b"new bytes",
        false,
        None,
        Some((gone.id, "f".into())),
    );
    let (e, cc) = written(c.response(id));
    assert!(cc.is_none());
    assert_eq!(e.id, f.id);
    assert_eq!(e.parent, a.id);
    assert_eq!(
        std::fs::read(root.path().join("a/f")).unwrap(),
        b"new bytes"
    );
}

/// A Write whose target's directory was just swapped for a symlink (`mv d d.old; ln -s … d`)
/// must resolve against the live tree: the upload lands in d.old, it is not NotFound (which the
/// engine takes for "the item is gone" and re-creates as `x 2`, fuzz seed 152).
#[test]
fn write_right_after_its_dir_moved_resolves_live() {
    let (root, state) = (tmp(), tmp());
    std::fs::create_dir_all(root.path().join("d/src")).unwrap();
    std::fs::write(root.path().join("d/src/x"), b"old").unwrap();
    let mut c = session(root.path(), state.path());
    for i in 0..20u64 {
        let x = c.find(&format!("{}/src/x", if i == 0 { "d" } else { "d.old" }));
        let x = x.unwrap_or_else(|| panic!("round {i}: x not found"));
        if i > 0 {
            // Put the tree back for the next round.
            std::fs::remove_file(root.path().join("d")).unwrap();
            std::fs::rename(root.path().join("d.old"), root.path().join("d")).unwrap();
            c.ping();
        }
        let x = c.find("d/src/x").unwrap_or(x);
        std::fs::rename(root.path().join("d"), root.path().join("d.old")).unwrap();
        std::os::unix::fs::symlink("/nonexistent", root.path().join("d")).unwrap();
        let body = format!("mac round {i}");
        let (e, cc) = written(c.write(
            op(70 + i),
            x.parent,
            "x",
            Some(x.id),
            Some(x.version.content),
            body.as_bytes(),
            false,
        ));
        assert!(cc.is_none(), "round {i}");
        assert_eq!(e.id, x.id);
        assert_eq!(
            std::fs::read(root.path().join("d.old/src/x")).unwrap(),
            body.as_bytes()
        );
        c.ping();
    }
}

/// Where unlatchd has not seen a move (a polled directory, no inotify), a request by id resolves
/// the stale path, re-lists the parent, and retries where the id is now instead of answering
/// NotFound ("the item is gone").
#[test]
fn request_by_id_follows_an_unseen_move() {
    let (root, state) = (tmp(), tmp());
    std::fs::create_dir(root.path().join("d")).unwrap();
    std::fs::write(root.path().join("d/f"), b"old").unwrap();
    let mut c = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            env: vec![("UNLATCHD_POLL".into(), "1".into())],
            ..Default::default()
        },
    );
    c.wait_snapshot();
    let f = c.find("d/f").unwrap();
    std::fs::rename(root.path().join("d/f"), root.path().join("d/g")).unwrap();
    let (e, cc) = written(c.write(
        op(90),
        f.parent,
        "f",
        Some(f.id),
        Some(f.version.content),
        b"new",
        false,
    ));
    assert!(cc.is_none());
    assert_eq!(e.id, f.id, "same item");
    assert_eq!(e.name, "g");
    assert_eq!(std::fs::read(root.path().join("d/g")).unwrap(), b"new");
}
