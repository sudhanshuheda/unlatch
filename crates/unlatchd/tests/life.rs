//! Lifecycle / persistence robustness: unlatchd's own
//! install and state dirs inside the served root, `unlatchd stop` with a stale pid file, a
//! squatted abstract socket name, an unwritable state dir (id/seq reservation), the tombstone
//! count cap, and relative paths beyond PATH_MAX.

mod common;
use common::*;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use unlatch_proto::wire::{Change, Request, Response, Resume, WelcomeMode};
use unlatch_proto::{ErrorCode, ItemId};

fn stop(state: &Path) -> std::process::Output {
    Command::new(BIN)
        .arg("stop")
        .arg("--state")
        .arg(state)
        .output()
        .unwrap()
}

struct StopOnDrop<'a>(&'a Path);
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        let _ = stop(self.0);
    }
}

/// Restores 0700 on drop (a failing assertion must not leave an undeletable temp dir).
struct Writable<'a>(&'a Path);
impl Drop for Writable<'_> {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o700));
    }
}

fn idle_events(c: &mut Client, d: Duration) -> usize {
    let n0 = c.event_seqs.len();
    let t0 = Instant::now();
    while t0.elapsed() < d {
        c.pump(Duration::from_millis(50));
    }
    c.event_seqs.len() - n0
}

/// Default layout on a VM whose served root is `~`: UNLATCH_HOME=~/.unlatch, state under it. Neither
/// the install dir nor the state dir may be indexed or watched (no self-feeding event loop, nothing
/// of unlatchd in Finder) — matched by identity, so the same holds wherever they live in the root.
#[test]
fn own_install_and_state_dirs_are_never_indexed() {
    for (install, state) in [
        (".unlatch", ".unlatch/state/abc"),
        // Not under a ".unlatch" name at all: identity, not name.
        ("tools/hx", "work/st"),
    ] {
        let home = tmp();
        let install = home.path().join(install);
        let state = home.path().join(state);
        std::fs::create_dir_all(&install).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(install.join("unlatchd-0.1.0"), b"binary").unwrap();
        std::fs::write(home.path().join("a.txt"), b"x").unwrap();
        let mut c = Client::spawn(
            home.path(),
            &state,
            Opts {
                env: vec![("UNLATCH_HOME".into(), install.display().to_string())],
                ..Default::default()
            },
        );
        c.wait_snapshot();
        c.write(op(1), ItemId::ROOT, "f1", None, None, b"hi", false)
            .unwrap();
        std::fs::write(home.path().join("agent.txt"), b"agent").unwrap();
        let s1 = c.ping();
        assert!(c.find("agent.txt").is_some());
        let names: Vec<String> = c.replica.values().map(|e| e.name.clone()).collect();
        for own in [
            "unlatchd-0.1.0",
            "index.bin",
            "alloc.bin",
            "serve.lock",
            "journal.0",
            "journal.1",
            "abc",
            "hx",
            "st",
        ] {
            assert!(!names.iter().any(|n| n == own), "{own} exposed: {names:?}");
        }
        // Idle: nothing changes in the root, so nothing may be published.
        let n = idle_events(&mut c, Duration::from_millis(1500));
        let s2 = c.ping();
        assert_eq!(n, 0, "events while idle (self-feeding loop)");
        assert_eq!(s1, s2, "seq grew while idle");
        // A listing of the parent of the install dir does not show it either.
        if let Some(parent) = install.parent().filter(|p| *p != home.path()) {
            let rel = parent.strip_prefix(home.path()).unwrap();
            let e = c.find(&rel.to_string_lossy()).unwrap();
            let (_, kids) = c.list(e.id).unwrap();
            assert!(kids.iter().all(|k| k.name != "hx"), "{kids:?}");
        }
    }
}

/// `UNLATCH_HOME` unset: the install dir falls back to the folder holding the binary. When that
/// is a folder of the user's (`<root>/bin`, as `~/bin`, `~/.local/bin` or `~/.cargo/bin` under
/// the root `~`), only the binary itself is unlatchd's: the user's other files there stay
/// visible. When it is one of the bootstrap's probed dirs (`~/.unlatch` here), it is excluded
/// whole, as with `UNLATCH_HOME`. The default state container next to the binary
/// (`<dir>/state/<hash>`, the layout without `UNLATCH_HOME`) is unlatchd's too.
#[test]
fn binary_in_a_user_folder_hides_only_itself() {
    use std::os::unix::fs::PermissionsExt;
    let home = tmp();
    let state = home.path().join("bin/state/0123abcd");
    std::fs::create_dir_all(&state).unwrap();
    for dir in ["bin", ".unlatch"] {
        let d = home.path().join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::copy(BIN, d.join("unlatchd")).unwrap();
        std::fs::set_permissions(d.join("unlatchd"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(d.join("tool.sh"), b"#!/bin/sh\necho mine\n").unwrap();
    }
    for (dir, dedicated) in [("bin", false), (".unlatch", true)] {
        let mut c = Client::spawn(
            home.path(),
            &state,
            Opts {
                bin: Some(home.path().join(dir).join("unlatchd")),
                env: vec![("HOME".into(), home.path().display().to_string())],
                env_remove: vec!["UNLATCH_HOME".into(), "XDG_DATA_HOME".into()],
                ..Default::default()
            },
        );
        c.wait_snapshot();
        // Both rounds share one state dir, so the second starts from the first's index, where
        // `.unlatch` was an ordinary folder. Welcome comes straight from that index (D17); the
        // startup verify walk then drops the newly excluded dir. Ping is a barrier over that walk.
        c.ping();
        let d = c.find(dir);
        if dedicated {
            assert!(d.is_none(), "{dir}: the bootstrap's install dir is shown");
            c.close();
            continue;
        }
        let d = d.unwrap_or_else(|| panic!("{dir}: the user's folder is hidden"));
        let (_, kids) = c.list(d.id).unwrap();
        let names: Vec<&str> = kids.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(
            names,
            ["tool.sh"],
            "{dir}: the user's script shown, unlatchd not"
        );
        // The other bin/unlatchd copy is a different inode: only the running binary is hidden.
        let other = c.find(".unlatch");
        assert!(
            other.is_some(),
            "only the running binary's own folder rules apply"
        );
        c.close();
    }
}

/// An index persisted while the install dir was still indexed (an older unlatchd, or UNLATCH_HOME
/// moved into the root) drops it at the startup verify walk: a resuming client is told to
/// remove it.
#[test]
fn persisted_own_dir_is_removed_on_adoption() {
    let (home, state) = (tmp(), tmp());
    let install = home.path().join("hx");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join("unlatchd-0.1.0"), b"binary").unwrap();
    let mut a = Client::spawn(home.path(), state.path(), Opts::default());
    a.wait_snapshot();
    let hx = a.find("hx").expect("indexed while not unlatchd's").id;
    let (index, seq) = (a.welcome().index, a.ping());
    a.close();
    let mut b = Client::spawn(
        home.path(),
        state.path(),
        Opts {
            env: vec![("UNLATCH_HOME".into(), install.display().to_string())],
            resume: Some(Resume { index, seq }),
            ..Default::default()
        },
    );
    assert_eq!(b.welcome().mode, WelcomeMode::Resume);
    b.ping();
    assert!(
        b.events
            .iter()
            .any(|e| matches!(e, Change::Remove { id, .. } if *id == hx)),
        "{:?}",
        b.events
    );
}

/// A recursive delete of a folder that contains unlatchd's install dir keeps it (it is not the
/// Mac's to delete: the Mac never saw it).
#[test]
fn recursive_remove_keeps_the_install_dir() {
    let home = tmp();
    let install = home.path().join("tools/hx");
    let state = install.join("state/abc");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(install.join("unlatchd-0.1.0"), b"binary").unwrap();
    std::fs::write(home.path().join("tools/note.txt"), b"n").unwrap();
    // Old enough to be judged "not newer" by the unindexed-entry time rule.
    std::thread::sleep(Duration::from_millis(1100));
    let mut c = Client::spawn(
        home.path(),
        &state,
        Opts {
            env: vec![("UNLATCH_HOME".into(), install.display().to_string())],
            ..Default::default()
        },
    );
    c.wait_snapshot();
    let seen = c.ping();
    std::thread::sleep(Duration::from_millis(1100));
    let tools = c.find("tools").unwrap();
    let r = c.call(Request::Remove {
        op: op(9),
        id: tools.id,
        base: v(&tools),
        recursive: true,
        seen_seq: seen,
    });
    eprintln!("remove: {r:?}");
    assert!(
        install.join("unlatchd-0.1.0").exists(),
        "unlatchd's install dir was deleted"
    );
    assert!(state.join("serve.lock").exists());
    assert!(!home.path().join("tools/note.txt").exists());
}

// ---- unlatchd stop ------------------------------------------------------------------------

/// serve.pid is stale (an earlier serve died uncleanly) and its pid now names an unrelated
/// process, while an `unlatchd stdio` session holds serve.lock: `stop` must not signal anything
/// that is not the lock-holding `unlatchd serve` of this state dir.
#[test]
fn stop_never_signals_a_process_that_is_not_the_serve() {
    let (root, state) = (tmp(), tmp());
    let mut victim = Command::new("sleep").arg("300").spawn().unwrap();
    std::fs::write(state.path().join("serve.pid"), format!("{}\n", victim.id())).unwrap();
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let out = stop(state.path());
    eprintln!(
        "stop: {:?} {} {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    std::thread::sleep(Duration::from_millis(200));
    let st = victim.try_wait().unwrap();
    let _ = victim.kill();
    let _ = victim.wait();
    assert!(
        st.is_none(),
        "unlatchd stop killed an unrelated process: {st:?}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("stopped"),
        "claims to have stopped something"
    );
    // The stdio session is untouched.
    c.ping();
}

/// The real serve is found (and stopped) even when serve.pid names another process.
#[test]
fn stop_stops_the_lock_holding_serve_despite_a_stale_pid_file() {
    let (root, state) = (tmp(), tmp());
    let _g = StopOnDrop(state.path());
    let mut c = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            connect: true,
            ..Default::default()
        },
    );
    c.wait_snapshot();
    let real: i32 = std::fs::read_to_string(state.path().join("serve.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut victim = Command::new("sleep").arg("300").spawn().unwrap();
    std::fs::write(state.path().join("serve.pid"), format!("{}\n", victim.id())).unwrap();
    let out = stop(state.path());
    std::thread::sleep(Duration::from_millis(200));
    let st = victim.try_wait().unwrap();
    let _ = victim.kill();
    let _ = victim.wait();
    assert!(st.is_none(), "unlatchd stop killed an unrelated process");
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&format!("pid {real}")),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let t0 = Instant::now();
    while Path::new(&format!("/proc/{real}")).exists() && t0.elapsed() < Duration::from_secs(5) {
        let stat = std::fs::read_to_string(format!("/proc/{real}/stat")).unwrap_or_default();
        if stat.split_whitespace().nth(2) == Some("Z") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let stat = std::fs::read_to_string(format!("/proc/{real}/stat")).unwrap_or_default();
    assert!(
        stat.is_empty() || stat.split_whitespace().nth(2) == Some("Z"),
        "serve still running"
    );
}

// ---- abstract socket squatting -------------------------------------------------------------

/// Runs `script` (python3) as another uid: uid 1 of a user namespace whose ids 1.. map to this
/// user's subordinate ids, so SO_PEERCRED reports a foreign uid. None when unavailable.
fn as_other_uid(script: &str, arg: &[u8]) -> Option<std::process::Child> {
    // Probe the exact chain the real command uses (subordinate uids, setpriv to uid 1, python3):
    // some hosts (e.g. CI runners) map the namespace but cannot switch to the second uid.
    let ok = Command::new("unshare")
        .args([
            "--map-auto",
            "--map-root-user",
            "setpriv",
            "--reuid=1",
            "--regid=1",
            "--clear-groups",
            "python3",
            "-c",
            "import os, sys; sys.exit(0 if os.getuid() == 1 else 1)",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    use std::os::unix::ffi::OsStrExt;
    Command::new("unshare")
        .args([
            "--map-auto",
            "--map-root-user",
            "setpriv",
            "--reuid=1",
            "--regid=1",
            "--clear-groups",
            "python3",
            "-c",
            script,
        ])
        .arg(std::ffi::OsStr::from_bytes(arg))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()
}

const SQUAT: &str = r#"
import socket, sys, time
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.bind(b"\0" + sys.argv[1].encode("latin-1"))
s.listen(16)
print("bound", flush=True)
time.sleep(60)
"#;

const PEEK: &str = r#"
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(5)
s.connect(b"\0" + sys.argv[1].encode("latin-1"))
try:
    d = s.recv(64)
except Exception as e:
    d = b""
print("got", len(d), flush=True)
"#;

fn wait_line(c: &mut std::process::Child) -> String {
    use std::io::BufRead;
    let out = c.stdout.as_mut().unwrap();
    let mut line = String::new();
    std::io::BufReader::new(out).read_line(&mut line).unwrap();
    line
}

/// Another uid binds the per-root socket name (the abstract namespace has no permissions, and
/// live names are listed in /proc/net/unix). SO_PEERCRED rejects it on both sides, and the name
/// carries a random per-state-dir nonce that a new server re-rolls when its name is taken: the
/// squatter can neither intercept nor deny service.
#[test]
fn squatted_socket_name_neither_hijacks_nor_denies_service() {
    let (root, state) = (tmp(), tmp());
    let _g = StopOnDrop(state.path());
    let rootc = std::fs::canonicalize(root.path()).unwrap();
    let statec = std::fs::canonicalize(state.path()).unwrap();
    let mut a = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            connect: true,
            ..Default::default()
        },
    );
    a.wait_snapshot();
    let name = unlatchd::lifecycle::current_socket_name(&rootc, &statec);
    // The server rejects a foreign-uid peer: no preamble, just EOF.
    let Some(mut peek) = as_other_uid(PEEK, &name) else {
        eprintln!("SKIP: no subordinate uid mapping (unshare --map-auto)");
        return;
    };
    let line = wait_line(&mut peek);
    let _ = peek.wait();
    assert_eq!(line.trim(), "got 0", "server talked to a foreign uid");
    drop(a);
    assert!(stop(state.path()).status.success());
    // The squatter takes the live name while no server runs (e.g. after the idle exit).
    let mut squat = as_other_uid(SQUAT, &name).unwrap();
    assert_eq!(wait_line(&mut squat).trim(), "bound");
    let t0 = Instant::now();
    let mut b = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            connect: true,
            ..Default::default()
        },
    );
    b.wait_snapshot();
    b.ping();
    assert!(t0.elapsed() < Duration::from_secs(10));
    let fresh = unlatchd::lifecycle::current_socket_name(&rootc, &statec);
    assert_ne!(fresh, name, "the server must move off a squatted name");
    let _ = squat.kill();
    let _ = squat.wait();
    // The nonce file is private.
    let mode = std::fs::metadata(statec.join("sock.nonce"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "sock.nonce mode {mode:o}");
}

/// The default name is not computable from public facts (uid, root, state path): binding the
/// name derived from them blocks nothing.
#[test]
fn predictable_socket_name_squat_is_harmless() {
    let (root, state) = (tmp(), tmp());
    let _g = StopOnDrop(state.path());
    let rootc = std::fs::canonicalize(root.path()).unwrap();
    let statec = std::fs::canonicalize(state.path()).unwrap();
    let predictable = unlatchd::lifecycle::socket_name(&rootc, &statec, None);
    let Some(mut squat) = as_other_uid(SQUAT, &predictable) else {
        eprintln!("SKIP: no subordinate uid mapping (unshare --map-auto)");
        return;
    };
    assert_eq!(wait_line(&mut squat).trim(), "bound");
    let mut c = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            connect: true,
            ..Default::default()
        },
    );
    c.wait_snapshot();
    c.ping();
    let _ = squat.kill();
    let _ = squat.wait();
}

// ---- id/seq reservation (alloc.bin) ---------------------------------------------------------

fn expect_err<T: std::fmt::Debug>(r: Result<T, unlatch_proto::ProtoError>) -> ErrorCode {
    match r {
        Err(e) => e.code,
        Ok(x) => panic!("expected an error, got {x:?}"),
    }
}

/// alloc.bin cannot be rewritten (state dir unwritable; disk full behaves the same) once the
/// reserved id block is used up. No id/seq above the durable reservation may reach a client:
/// changes are held back (Ping fails, a mutation fails before touching the VM) and published —
/// all of them — once the reservation succeeds again; a live client then has every change and a
/// resuming one too.
#[test]
fn unreservable_ids_are_never_handed_out_and_held_changes_publish_on_recovery() {
    let (root, state) = (tmp(), tmp());
    let mut a = Client::spawn(root.path(), state.path(), Opts::default());
    a.wait_snapshot();
    let index = a.welcome().index;
    let _w = Writable(state.path());
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let bulk = root.path().join("bulk");
    std::fs::create_dir(&bulk).unwrap();
    for i in 0..70_000 {
        std::fs::write(bulk.join(format!("f{i}")), b"").unwrap();
    }
    std::fs::write(root.path().join("after.txt"), b"x").unwrap();
    // The barrier must not claim success while changes are held back.
    let code = expect_err(a.call(Request::Ping { nonce: 1 }));
    eprintln!("ping during the outage: {code:?}");
    let max_id = a.replica.keys().map(|i| i.0).max().unwrap();
    let alloc = std::fs::read(state.path().join("alloc.bin")).unwrap();
    let id_hwm = u64::from_le_bytes(alloc[0..8].try_into().unwrap());
    let seq_hwm = u64::from_le_bytes(alloc[8..16].try_into().unwrap());
    assert!(
        max_id < id_hwm,
        "id {max_id} handed out above the reserved {id_hwm}"
    );
    assert!(
        a.event_seqs.iter().all(|&s| s <= seq_hwm),
        "seq above reservation"
    );
    // A mutation fails cleanly: nothing on the VM.
    let code = expect_err(a.call(Request::Mkdir {
        op: op(5),
        parent: ItemId::ROOT,
        name: "mac-dir".into(),
        may_exist: false,
    }));
    assert!(
        matches!(code, ErrorCode::NoSpace | ErrorCode::Io),
        "{code:?}"
    );
    assert!(
        !root.path().join("mac-dir").exists(),
        "mutation applied despite the error"
    );
    // Space returns.
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(root.path().join("later.txt"), b"y").unwrap();
    let t0 = Instant::now();
    let s2 = loop {
        match a.call(Request::Ping { nonce: 2 }) {
            Ok(Response::Pong { seq, .. }) => break seq,
            other => {
                assert!(t0.elapsed() < Duration::from_secs(10), "{other:?}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    let n = a.find("bulk").map(|b| a.children(b.id).len()).unwrap_or(0);
    assert_eq!(n, 70_000, "live client is missing held changes");
    assert!(a.find("after.txt").is_some() && a.find("later.txt").is_some());
    // The retried mutation now succeeds.
    a.call(Request::Mkdir {
        op: op(5),
        parent: ItemId::ROOT,
        name: "mac-dir".into(),
        may_exist: false,
    })
    .unwrap();
    a.close();
    // A client that resumes from before the outage gets everything too.
    let b = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            resume: Some(Resume { index, seq: 1 }),
            ..Default::default()
        },
    );
    drop(b);
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    assert_eq!(c.children(c.find("bulk").unwrap().id).len(), 70_000);
    assert!(c.ping() >= s2);
}

// ---- tombstones ------------------------------------------------------------------------

/// Tombstones are capped by count (UNLATCHD_TOMB_MAX): beyond the cap the oldest are dropped and a
/// client resuming from before them gets a Snapshot, never a Resume missing removals.
#[test]
fn tombstone_count_cap_falls_back_to_snapshot() {
    let (root, state) = (tmp(), tmp());
    let env = vec![("UNLATCHD_TOMB_MAX".to_string(), "100".to_string())];
    let mut a = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            env: env.clone(),
            ..Default::default()
        },
    );
    a.wait_snapshot();
    let index = a.welcome().index;
    std::fs::write(root.path().join("keep.txt"), b"k").unwrap();
    let s0 = a.ping();
    for round in 0..3 {
        let d = root.path().join(format!("out{round}"));
        std::fs::create_dir(&d).unwrap();
        for i in 0..200 {
            std::fs::write(d.join(format!("f{i}")), b"x").unwrap();
        }
        a.ping();
        std::fs::remove_dir_all(&d).unwrap();
        a.ping();
    }
    let s1 = a.ping();
    a.close();
    // Count on disk after the clean-stop checkpoint.
    let b = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            env: env.clone(),
            resume: Some(Resume { index, seq: s0 }),
            ..Default::default()
        },
    );
    assert_eq!(
        b.welcome().mode,
        WelcomeMode::Snapshot,
        "resume past dropped tombstones"
    );
    drop(b);
    let mut c = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            env,
            resume: Some(Resume { index, seq: s1 }),
            ..Default::default()
        },
    );
    assert_eq!(c.welcome().mode, WelcomeMode::Resume);
    c.ping();
    assert!(
        !c.events.iter().any(|e| matches!(e, Change::Remove { .. })),
        "nothing to remove after s1"
    );
}

// ---- PATH_MAX --------------------------------------------------------------------------

/// A tree whose relative paths exceed PATH_MAX (4096 bytes) is indexed, watched, listed, read
/// and written like any other.
#[test]
fn paths_beyond_path_max_work() {
    let (root, state) = (tmp(), tmp());
    let name = "d".repeat(200);
    let chain = |tail: &str| {
        let mut s = String::from("set -e; cd \"$1\"; ");
        for _ in 0..30 {
            s.push_str(&format!("mkdir -p {name}; cd {name}; "));
        }
        s.push_str(tail);
        let st = Command::new("bash")
            .arg("-c")
            .arg(&s)
            .arg("x")
            .arg(root.path())
            .status()
            .unwrap();
        assert!(st.success());
    };
    chain("echo bottom > deep.txt");
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let deep = c
        .replica
        .values()
        .find(|e| e.name == "deep.txt")
        .cloned()
        .expect("deep.txt (path > 4096 bytes) missing from the snapshot");
    chain("echo new > new.txt");
    c.ping();
    assert!(
        c.replica.values().any(|e| e.name == "new.txt"),
        "agent write below PATH_MAX depth not synced"
    );
    let (bytes, _) = c.read(deep.id, None).unwrap();
    assert_eq!(bytes, b"bottom\n");
    let (_, kids) = c.list(deep.parent).unwrap();
    assert!(kids.iter().any(|k| k.name == "deep.txt"));
    c.write(op(3), deep.parent, "mac.txt", None, None, b"mac", false)
        .unwrap();
    let mut s = String::from("cd \"$1\"; ");
    for _ in 0..30 {
        s.push_str(&format!("cd {name}; "));
    }
    s.push_str("cat mac.txt");
    let out = Command::new("bash")
        .arg("-c")
        .arg(&s)
        .arg("x")
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"mac");
}
