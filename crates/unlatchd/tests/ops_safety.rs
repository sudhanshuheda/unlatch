//! Data-safety rules of the mutating ops (review §2(d)1.4, 3, 4) against the real daemon:
//! base checks see the live file in polled directories, a recursive delete keeps a subtree
//! moved into it after the client's seen point and never crosses st_dev, and a move that the
//! kernel refuses across filesystems is never reported as "the item is gone".
//!
//! The mount cases need a private mount namespace: each re-runs itself under `unshare -rm`
//! (unprivileged user + mount namespace). Where the host forbids that (some CI kernels), they
//! say so on stderr and pass without running.

mod common;
use common::*;
use std::path::Path;
use std::time::Duration;
use unlatch_proto::wire::{Request, Response};
use unlatch_proto::{ErrorCode, ItemId};

fn polled() -> Opts {
    Opts {
        env: vec![("UNLATCHD_POLL".into(), "1".into())],
        ..Default::default()
    }
}

/// Every regular file directly in `dir`, with its bytes.
fn files_in(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| {
            (
                e.file_name().to_string_lossy().to_string(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

fn contains(b: &[u8], needle: &[u8]) -> bool {
    b.windows(needle.len()).any(|w| w == needle)
}

const NS_ENV: &str = "UNLATCHD_TEST_IN_USERNS";

/// Run `test` (this binary's test of that name) inside `unshare -rm`. Returns true when the
/// caller is already inside the namespace and should run its body.
fn in_mount_ns(test: &str) -> bool {
    if std::env::var_os(NS_ENV).is_some() {
        return true;
    }
    let probe = std::process::Command::new("unshare")
        .args(["-rm", "true"])
        .stderr(std::process::Stdio::null())
        .status();
    if !matches!(probe, Ok(s) if s.success()) {
        eprintln!("{test}: SKIPPED — unprivileged `unshare -rm` is not available on this host");
        return false;
    }
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new("unshare")
        .arg("-rm")
        .arg(&exe)
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(NS_ENV, "1")
        .output()
        .unwrap();
    eprintln!("{}", String::from_utf8_lossy(&out.stdout));
    eprintln!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success(),
        "{test} failed inside the mount namespace"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 passed"),
        "{test} did not run inside the mount namespace"
    );
    false
}

fn mount(args: &[&str], at: &Path) {
    let ok = std::process::Command::new("mount")
        .args(args)
        .arg(at)
        .status()
        .unwrap();
    assert!(ok.success(), "mount {args:?} {at:?}");
}

// ---- polled directories: base checks against the live file -----------------------------------

/// The agent appends to f.txt in a polled directory (not polled yet); the Mac then saves f.txt
/// based on the version it saw. The agent's bytes must survive (conflict copy), never be
/// unlinked with the old inode.
#[test]
fn polled_replace_keeps_an_unpolled_agent_edit() {
    let (root, state) = (tmp(), tmp());
    let p = root.path().join("f.txt");
    std::fs::write(&p, b"v1\n").unwrap();
    let mut c = Client::spawn(root.path(), state.path(), polled());
    c.wait_snapshot();
    let f = c.find("f.txt").unwrap();
    {
        use std::io::Write;
        let mut h = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        h.write_all(b"AGENT-WRITE\n").unwrap();
    }
    let r = c.write(
        op(1),
        ItemId::ROOT,
        "f.txt",
        Some(f.id),
        Some(f.version.content),
        b"mac save\n",
        false,
    );
    let files = files_in(root.path());
    match r {
        Ok(Response::Written { conflict_copy, .. }) => {
            assert!(conflict_copy.is_some(), "no conflict copy: {files:?}")
        }
        o => panic!("{o:?}"),
    }
    assert!(
        files.iter().any(|(_, b)| contains(b, b"AGENT-WRITE")),
        "agent write lost: {files:?}"
    );
    assert!(files.iter().any(|(_, b)| b == b"mac save\n"), "{files:?}");
}

/// Remove with the version the Mac saw, of a file the agent rewrote in a polled directory
/// (in place, or by a whole rewrite), and a recursive remove of its directory: rejected, the
/// agent's bytes stay.
#[test]
fn polled_remove_keeps_an_unpolled_agent_edit() {
    for (append, recursive) in [(true, false), (false, false), (false, true)] {
        let (root, state) = (tmp(), tmp());
        std::fs::create_dir(root.path().join("d")).unwrap();
        let p = root.path().join("d/f.txt");
        std::fs::write(&p, b"v1\n").unwrap();
        let mut c = Client::spawn(root.path(), state.path(), polled());
        c.wait_snapshot();
        let target = c.find(if recursive { "d" } else { "d/f.txt" }).unwrap();
        let seen = c.welcome().seq;
        std::thread::sleep(Duration::from_millis(30));
        if append {
            use std::io::Write;
            let mut h = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            h.write_all(b"AGENT-WRITE\n").unwrap();
        } else {
            std::fs::write(&p, b"AGENT-WRITE-LONGER\n").unwrap();
        }
        let r = c.call(Request::Remove {
            op: op(2),
            id: target.id,
            base: target.version,
            recursive,
            seen_seq: seen,
        });
        let case = format!("append {append} recursive {recursive}");
        match &r {
            Err(e) if e.code == ErrorCode::VersionMismatch => assert!(!recursive, "{case}"),
            Ok(Response::Removed { kept }) => assert!(!kept.is_empty(), "{case}: {kept:?}"),
            o => panic!("{case}: {o:?}"),
        }
        assert!(
            std::fs::read(&p).is_ok_and(|b| contains(&b, b"AGENT-WRITE")),
            "{case}: the agent's newer bytes were deleted ({r:?})"
        );
    }
}

// ---- recursive remove: a subtree moved in after the seen point --------------------------------

/// The agent moves an indexed folder into `src` after the Mac's seen point; the Mac deletes
/// `src`. Nothing in the moved folder was shown to the Mac under `src`: all of it stays.
#[test]
fn recursive_remove_keeps_a_folder_moved_in_after_the_seen_point() {
    for into in ["src", "src/node_modules"] {
        let (root, state) = (tmp(), tmp());
        std::fs::create_dir_all(root.path().join("src/node_modules/pkg")).unwrap();
        std::fs::write(root.path().join("src/x.txt"), b"x").unwrap();
        std::fs::create_dir_all(root.path().join("archive/deep")).unwrap();
        std::fs::write(root.path().join("archive/thesis.txt"), b"years of work").unwrap();
        std::fs::write(root.path().join("archive/deep/notes.txt"), b"notes").unwrap();
        let mut c = Client::spawn(root.path(), state.path(), Opts::default());
        c.wait_snapshot();
        // Clearly after every file's ctime: the seq→time sample of `seen` is later than them.
        std::thread::sleep(Duration::from_millis(1200));
        c.ping();
        let seen = c.ping();
        let src = c.find("src").unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        let moved = root.path().join(into).join("archive");
        std::fs::rename(root.path().join("archive"), &moved).unwrap();
        c.ping();
        c.pump(Duration::from_millis(100));
        let r = c.call(Request::Remove {
            op: op(31),
            id: src.id,
            base: src.version,
            recursive: true,
            seen_seq: seen,
        });
        match &r {
            Ok(Response::Removed { kept }) => assert!(kept.contains(&src.id), "{into}: {kept:?}"),
            o => panic!("{into}: {o:?}"),
        }
        assert_eq!(
            std::fs::read(moved.join("thesis.txt")).ok().as_deref(),
            Some(&b"years of work"[..]),
            "{into}: a file the Mac never saw inside src was deleted"
        );
        assert!(moved.join("deep/notes.txt").exists(), "{into}");
        assert!(
            !root.path().join("src/x.txt").exists(),
            "{into}: seen files go"
        );
    }
}

// ---- mount points: never cross st_dev, EXDEV is not "gone" -----------------------------------

fn remove_mount_point(bind: bool, test: &str) {
    if !in_mount_ns(test) {
        return;
    }
    let (root, state, outside) = (tmp(), tmp(), tmp());
    let data = root.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::write(root.path().join("top.txt"), b"t").unwrap();
    let src = if bind {
        outside.path().to_path_buf()
    } else {
        mount(&["-t", "tmpfs", "none"], &data);
        data.clone()
    };
    std::fs::create_dir(src.join("photos")).unwrap();
    std::fs::write(src.join("photos/a.jpg"), b"jpeg").unwrap();
    std::fs::write(src.join("notes.txt"), b"notes").unwrap();
    if bind {
        mount(&["--bind", outside.path().to_str().unwrap()], &data);
    }
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let d = c.find("data").expect("mount point listed");
    let _ = c.list(d.id); // expanded or not: the walk must not enter it either way
    std::thread::sleep(Duration::from_millis(1200));
    c.ping();
    let seen = c.ping();
    let d = c.find("data").unwrap();
    let r = c.call(Request::Remove {
        op: op(9),
        id: d.id,
        base: d.version,
        recursive: true,
        seen_seq: seen,
    });
    match &r {
        Ok(Response::Removed { kept }) => assert_eq!(kept, &vec![d.id]),
        o => panic!("{o:?}"),
    }
    assert_eq!(std::fs::read(src.join("photos/a.jpg")).unwrap(), b"jpeg");
    assert_eq!(std::fs::read(src.join("notes.txt")).unwrap(), b"notes");
    // Non-recursive (an "empty" folder to the Mac) is kept as well.
    let r = c.call(Request::Remove {
        op: op(10),
        id: d.id,
        base: d.version,
        recursive: false,
        seen_seq: seen,
    });
    assert!(
        matches!(&r, Ok(Response::Removed { kept }) if kept == &vec![d.id]),
        "{r:?}"
    );
    assert!(src.join("notes.txt").exists());
}

#[test]
fn recursive_remove_of_a_mount_point_keeps_the_mounted_filesystem() {
    remove_mount_point(
        false,
        "recursive_remove_of_a_mount_point_keeps_the_mounted_filesystem",
    );
}

#[test]
fn recursive_remove_of_a_bind_mount_keeps_its_source() {
    remove_mount_point(true, "recursive_remove_of_a_bind_mount_keeps_its_source");
}

/// A move onto another filesystem (a mount point below the root) fails with EXDEV: the item
/// is where it was, so the reply must say so (`Renamed { applied: false }`), never NotFound —
/// the engine deletes an item it is told is gone. A Write with `move_to` there saves in place.
#[test]
fn move_into_a_mount_point_reports_the_item_where_it_is() {
    let test = "move_into_a_mount_point_reports_the_item_where_it_is";
    if !in_mount_ns(test) {
        return;
    }
    let (root, state) = (tmp(), tmp());
    let mnt = root.path().join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    std::fs::create_dir(root.path().join("sub")).unwrap();
    std::fs::write(root.path().join("sub/a.txt"), b"a").unwrap();
    std::fs::write(root.path().join("b.txt"), b"b").unwrap();
    mount(&["-t", "tmpfs", "none"], &mnt);
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let m = c.find("mnt").unwrap();
    let _ = c.list(m.id);
    let sub = c.find("sub").unwrap();
    let a = c.find("sub/a.txt").unwrap();
    for (n, id, parent, name) in [(1, a.id, sub.id, "a.txt"), (2, sub.id, ItemId::ROOT, "sub")] {
        let r = c.call(Request::Rename {
            op: op(n),
            id,
            base_parent: parent,
            base_name: name.into(),
            new_parent: m.id,
            new_name: name.into(),
        });
        match r {
            Ok(Response::Renamed { entry, applied }) => {
                assert!(!applied);
                assert_eq!(
                    (entry.id, entry.parent, entry.name.as_str()),
                    (id, parent, name)
                );
            }
            o => panic!("rename {name}: {o:?}"),
        }
    }
    assert!(root.path().join("sub/a.txt").exists());
    let b = c.find("b.txt").unwrap();
    let id = c.start_write(
        op(3),
        ItemId::ROOT,
        "b.txt",
        Some(b.id),
        Some(b.version.content),
        b"new b",
        false,
        None,
        Some((m.id, "b.txt".into())),
    );
    match c.response(id) {
        Ok(Response::Written { entry, .. }) => {
            assert_eq!((entry.parent, entry.name.as_str()), (ItemId::ROOT, "b.txt"))
        }
        o => panic!("write+move: {o:?}"),
    }
    assert_eq!(std::fs::read(root.path().join("b.txt")).unwrap(), b"new b");
}
