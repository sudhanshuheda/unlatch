//! `serve` / `connect` lifecycle (D18, review §2(d)10 and §2(f)5).

mod common;
use common::*;
use std::path::Path;
use std::time::{Duration, Instant};

/// pids of `unlatchd serve` processes for this state dir.
fn serve_pids(state: &Path) -> Vec<i32> {
    let needle = state.as_os_str().as_encoded_bytes().to_vec();
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let args: Vec<&[u8]> = cmd.split(|&b| b == 0).collect();
        if args.len() > 1 && args[1] == b"serve" && args.contains(&needle.as_slice()) {
            // skip zombies
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            if stat.split_whitespace().nth(2) == Some("Z") {
                continue;
            }
            out.push(pid);
        }
    }
    out
}

fn stop(state: &Path) {
    let _ = std::process::Command::new(BIN)
        .arg("stop")
        .arg("--state")
        .arg(state)
        .output();
}

struct StopOnDrop<'a>(&'a Path);
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        stop(self.0);
    }
}

fn connect(root: &Path, state: &Path, env: Vec<(String, String)>) -> Client {
    let mut c = Client::spawn(
        root,
        state,
        Opts {
            connect: true,
            env,
            ..Default::default()
        },
    );
    if c.welcome().mode == unlatch_proto::wire::WelcomeMode::Snapshot {
        c.wait_snapshot();
    }
    c
}

#[test]
fn connect_spawns_one_shared_server() {
    let (root, state) = (tmp(), tmp());
    let _g = StopOnDrop(state.path());
    std::fs::write(root.path().join("x"), b"x").unwrap();
    let mut a = connect(root.path(), state.path(), vec![]);
    let mut b = connect(root.path(), state.path(), vec![]);
    assert_eq!(a.welcome().index, b.welcome().index);
    assert_eq!(serve_pids(state.path()).len(), 1);
    std::fs::write(root.path().join("y"), b"y").unwrap();
    a.ping();
    b.ping();
    assert_eq!(
        a.find("y").unwrap().id,
        b.find("y").unwrap().id,
        "both clients see the change"
    );
    // A mutation by one client is an event for the other.
    let d = match a.call(unlatch_proto::wire::Request::Mkdir {
        op: op(1),
        parent: unlatch_proto::ItemId::ROOT,
        name: "from-a".into(),
        may_exist: false,
    }) {
        Ok(unlatch_proto::wire::Response::Entry(e)) => e,
        other => panic!("{other:?}"),
    };
    b.ping();
    assert_eq!(b.find("from-a").unwrap().id, d.id);
    a.close();
    b.close();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        serve_pids(state.path()).len(),
        1,
        "the server outlives its clients"
    );
    let st = std::process::Command::new(BIN)
        .arg("status")
        .arg("--state")
        .arg(state.path())
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&st.stdout);
    assert!(out.contains("running=yes"), "{out}");
    stop(state.path());
    let t0 = Instant::now();
    while !serve_pids(state.path()).is_empty() && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(serve_pids(state.path()).is_empty());
    let st = std::process::Command::new(BIN)
        .arg("status")
        .arg("--state")
        .arg(state.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&st.stdout).contains("running=no"));
}

#[test]
fn kill9_then_8_concurrent_connects_start_exactly_one_server() {
    let (root, state) = (tmp(), tmp());
    let _g = StopOnDrop(state.path());
    make_tree(root.path(), 5, 5);
    let c = connect(root.path(), state.path(), vec![]);
    let index = c.welcome().index;
    c.close();
    let pids = serve_pids(state.path());
    assert_eq!(pids.len(), 1);
    // SAFETY: killing the test's own child server.
    unsafe { libc::kill(pids[0], libc::SIGKILL) };
    let t0 = Instant::now();
    while !serve_pids(state.path()).is_empty() && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let root_p = root.path().to_path_buf();
    let state_p = state.path().to_path_buf();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (r, s, b) = (root_p.clone(), state_p.clone(), barrier.clone());
            std::thread::spawn(move || {
                b.wait();
                let log = s.join("connect.log").to_string_lossy().into_owned();
                let mut c = connect(&r, &s, vec![("UNLATCHD_LOG".into(), log)]);
                let idx = c.welcome().index;
                c.ping();
                (idx, c)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join()).collect();
    if results.iter().any(|r| r.is_err()) {
        panic!(
            "connect failed; serve.log:\n{}\nconnect.log:\n{}\nserve pids now: {:?}",
            std::fs::read_to_string(state.path().join("serve.log")).unwrap_or_default(),
            std::fs::read_to_string(state.path().join("connect.log")).unwrap_or_default(),
            serve_pids(state.path())
        );
    }
    let clients: Vec<_> = results.into_iter().map(|r| r.unwrap()).collect();
    assert_eq!(serve_pids(state.path()).len(), 1, "exactly one serve");
    for (idx, _) in &clients {
        assert_eq!(*idx, index, "same index after kill -9 (journal replay)");
    }
}

#[test]
fn connect_exits_promptly_when_the_client_disconnects() {
    let (root, state) = (tmp(), tmp());
    let _g = StopOnDrop(state.path());
    let mut c = connect(root.path(), state.path(), vec![]);
    c.ping();
    let t0 = Instant::now();
    let status = c.close();
    let dt = t0.elapsed();
    assert!(status.success());
    assert!(
        dt < Duration::from_secs(1),
        "connect lingered {dt:?} (ssh would hang)"
    );
    eprintln!("connect exited {dt:?} after stdin EOF");
}

#[test]
fn idle_server_exits() {
    let (root, state) = (tmp(), tmp());
    let _g = StopOnDrop(state.path());
    let env = vec![("UNLATCHD_IDLE_EXIT_SECS".to_string(), "1".to_string())];
    let c = connect(root.path(), state.path(), env);
    c.close();
    let t0 = Instant::now();
    while !serve_pids(state.path()).is_empty() && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(serve_pids(state.path()).is_empty(), "idle server must exit");
}

#[test]
fn stdio_refuses_a_state_dir_owned_by_a_server() {
    let (root, state) = (tmp(), tmp());
    let _g = StopOnDrop(state.path());
    let c = connect(root.path(), state.path(), vec![]);
    let out = std::process::Command::new(BIN)
        .arg("stdio")
        .arg("--root")
        .arg(root.path())
        .arg("--state")
        .arg(state.path())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    drop(c);
}

#[test]
fn version_and_usage() {
    let out = std::process::Command::new(BIN)
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("unlatchd "));
    let out = std::process::Command::new(BIN)
        .arg("bogus")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn unwatch_collapses_only_when_no_session_still_lists_the_dir() {
    use unlatch_proto::wire::{Request, Response};
    let (root, state) = (tmp(), tmp());
    let _g = StopOnDrop(state.path());
    std::fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
    let mut a = connect(root.path(), state.path(), vec![]);
    let mut b = connect(root.path(), state.path(), vec![]);
    let nm = a.find("node_modules").unwrap();
    assert!(nm.lazy);
    a.list(nm.id).unwrap();
    b.list(nm.id).unwrap();
    let unwatch = |c: &mut Client| match c.call(Request::Unwatch { dir: nm.id }) {
        Ok(Response::Entry(e)) => e,
        other => panic!("{other:?}"),
    };
    assert!(!unwatch(&mut a).lazy, "b still lists it");
    assert!(unwatch(&mut b).lazy, "last lister gone → collapsed");
}

/// A client that was away while an item moved *into* a directory that was then removed still
/// has the item at its old place: its Resume must carry a tombstone for the item itself, not
/// only for the removed directory (fuzz seed 72: a symlink moved into a dir, then `rm -rf`).
#[test]
fn resume_removes_an_item_moved_into_a_removed_dir() {
    let (root, state) = (tmp(), tmp());
    let _g = StopOnDrop(state.path());
    std::fs::create_dir(root.path().join("d")).unwrap();
    std::fs::write(root.path().join("f"), b"f").unwrap();
    let mut a = connect(root.path(), state.path(), vec![]);
    let f = a.find("f").unwrap();
    let index = a.welcome().index;
    let seq = a.ping();
    drop(a);
    // Another client keeps watching (and barriers) while the first one is away.
    let mut b = connect(root.path(), state.path(), vec![]);
    std::fs::rename(root.path().join("f"), root.path().join("d/moved")).unwrap();
    b.ping();
    std::fs::remove_dir_all(root.path().join("d")).unwrap();
    b.ping();
    let mut a = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            connect: true,
            resume: Some(unlatch_proto::wire::Resume { index, seq }),
            ..Default::default()
        },
    );
    assert_eq!(a.welcome().mode, unlatch_proto::wire::WelcomeMode::Resume);
    a.ping();
    assert!(
        a.events.iter().any(|c| matches!(
            c,
            unlatch_proto::wire::Change::Remove { id, .. } if *id == f.id
        )),
        "no tombstone for the moved-in item: {:?}",
        a.events
    );
}
