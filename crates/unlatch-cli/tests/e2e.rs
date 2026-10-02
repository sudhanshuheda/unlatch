//! End-to-end: the real `unlatch` binary, the real engine and a real `unlatchd stdio` (no ssh).
//!
//! Needs a built `unlatchd` (set `UNLATCHD_BIN`, or build it into the workspace target dir) and
//! FUSE (`/dev/fuse` + `fusermount3`). Run with `cargo test -p unlatch-cli -- --ignored`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn unlatchd_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("UNLATCHD_BIN") {
        return Some(PathBuf::from(p));
    }
    // The unlatch binary lives in <target>/<profile>/unlatch; look next to it first, then in the
    // workspace's default and per-agent target dirs.
    let unlatch = PathBuf::from(env!("CARGO_BIN_EXE_unlatch"));
    let mut candidates = vec![unlatch.with_file_name("unlatchd")];
    let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for profile in ["debug", "release"] {
        candidates.push(ws.join("target").join(profile).join("unlatchd"));
    }
    if let Ok(rd) = std::fs::read_dir(ws.join("target")) {
        for e in rd.flatten() {
            candidates.push(e.path().join("debug/unlatchd"));
            candidates.push(e.path().join("release/unlatchd"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn is_mounted(p: &Path) -> bool {
    let canon = match p.canonicalize() {
        Ok(c) => c,
        Err(_) => return false,
    };
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|m| {
            m.lines()
                .any(|l| l.split(' ').nth(4) == Some(canon.to_string_lossy().as_ref()))
        })
        .unwrap_or(false)
}

fn eventually(limit: Duration, what: &str, mut f: impl FnMut() -> bool) -> Duration {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
    t0.elapsed()
}

struct Mount {
    child: Child,
    mnt: PathBuf,
}

impl Drop for Mount {
    fn drop(&mut self) {
        // SAFETY: plain kill(2) on our own child.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if is_mounted(&self.mnt) {
            let _ = Command::new("fusermount3")
                .arg("-u")
                .arg("-z")
                .arg(&self.mnt)
                .status();
        }
    }
}

fn start_mount(tmp: &Path, root: &Path, unlatchd: &Path) -> Mount {
    let mnt = tmp.join("mnt");
    std::fs::create_dir_all(&mnt).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_unlatch"))
        .arg("mount")
        .arg(&mnt)
        .arg("--root")
        .arg(root)
        .arg("--state")
        .arg(tmp.join("client-state"))
        .arg("--foreground")
        .arg("--ttl-secs")
        .arg("3600")
        .arg("--wait-secs")
        .arg("20")
        .arg("--command")
        .arg(unlatchd)
        .arg("stdio")
        .arg("--root")
        .arg(root)
        .arg("--state")
        .arg(tmp.join("unlatchd-state"))
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn unlatch mount");
    let mut m = Mount { child, mnt };
    let t0 = Instant::now();
    while !is_mounted(&m.mnt) {
        if let Ok(Some(st)) = m.child.try_wait() {
            panic!("unlatch mount exited early: {st}");
        }
        assert!(
            t0.elapsed() < Duration::from_secs(40),
            "mount did not come up"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    m
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

#[test]
#[ignore = "needs engine+unlatchd"]
fn mount_unlatchd_stdio_ls_cat_write_rename_rm() {
    let Some(unlatchd) = unlatchd_bin() else {
        panic!(
            "unlatchd binary not found: build it (`cargo build -p unlatchd`) or set UNLATCHD_BIN"
        );
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("hello.txt"), "hello from the VM\n").unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let m = start_mount(tmp.path(), &root, &unlatchd);
    let mnt = m.mnt.clone();

    // ls + cat
    assert_eq!(names(&mnt), vec!["hello.txt", "src"]);
    assert_eq!(names(&mnt.join("src")), vec!["main.rs"]);
    assert_eq!(
        std::fs::read_to_string(mnt.join("hello.txt")).unwrap(),
        "hello from the VM\n"
    );

    // write (new file) → on the VM once close() returns
    std::fs::write(mnt.join("new.txt"), "written through FUSE").unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("new.txt")).unwrap(),
        "written through FUSE"
    );

    // overwrite an existing file
    std::fs::write(mnt.join("hello.txt"), "edited locally\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("hello.txt")).unwrap(),
        "edited locally\n"
    );

    // rename across directories
    std::fs::rename(mnt.join("new.txt"), mnt.join("src/moved.txt")).unwrap();
    assert!(!root.join("new.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("src/moved.txt")).unwrap(),
        "written through FUSE"
    );

    // mkdir / rm / rmdir
    std::fs::create_dir(mnt.join("dir")).unwrap();
    assert!(root.join("dir").is_dir());
    std::fs::remove_file(mnt.join("src/moved.txt")).unwrap();
    assert!(!root.join("src/moved.txt").exists());
    std::fs::remove_dir(mnt.join("dir")).unwrap();
    assert!(!root.join("dir").exists());

    // VM-side changes arrive by push despite the hour-long kernel TTLs.
    std::fs::write(root.join("from-agent.txt"), "agent output").unwrap();
    let t = eventually(Duration::from_secs(5), "VM create visible", || {
        mnt.join("from-agent.txt").exists()
    });
    eprintln!("VM create visible after {t:?}");
    std::fs::write(root.join("hello.txt"), "agent rewrote this\n").unwrap();
    eventually(Duration::from_secs(5), "VM edit visible", || {
        std::fs::read_to_string(mnt.join("hello.txt"))
            .map(|s| s == "agent rewrote this\n")
            .unwrap_or(false)
    });
    std::fs::remove_file(root.join("from-agent.txt")).unwrap();
    eventually(Duration::from_secs(5), "VM delete visible", || {
        !mnt.join("from-agent.txt").exists()
    });

    drop(m);
    assert!(!is_mounted(&mnt), "SIGTERM must unmount");
}

#[test]
#[ignore = "needs engine+unlatchd"]
fn agent_serves_ipc_for_ls_stat_cat_status() {
    let Some(unlatchd) = unlatchd_bin() else {
        panic!(
            "unlatchd binary not found: build it (`cargo build -p unlatchd`) or set UNLATCHD_BIN"
        );
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/a.txt"), "alpha").unwrap();
    let sock = tmp.path().join("e.sock");
    let cfg = serde_json::json!({
        "client_name": "e2e",
        "domains": [{
            "name": "e2e",
            "transport": {"command": {"argv": [unlatchd, "stdio", "--root", root, "--state", tmp.path().join("hs")]}},
            "remote_root": root,
            "state_dir": tmp.path().join("cs"),
            "socket": sock,
        }]
    });
    let cfg_path = tmp.path().join("agent.json");
    std::fs::write(&cfg_path, cfg.to_string()).unwrap();
    let mut agent = Command::new(env!("CARGO_BIN_EXE_unlatch"))
        .arg("agent")
        .arg("--config")
        .arg(&cfg_path)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let unlatch = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_unlatch"))
            .args(args)
            .arg("--socket")
            .arg(&sock)
            .arg("--domain")
            .arg("e2e")
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    eventually(Duration::from_secs(30), "agent socket", || sock.exists());
    eventually(Duration::from_secs(30), "listing", || {
        unlatch(&["ls", "sub"]).1.contains("a.txt")
    });
    let (ok, out, err) = unlatch(&["cat", "sub/a.txt"]);
    assert!(ok, "{err}");
    assert_eq!(out, "alpha");
    let (ok, out, err) = unlatch(&["stat", "sub/a.txt"]);
    assert!(ok, "{err}");
    assert!(out.contains("size:         5"), "{out}");
    let (ok, out, err) = unlatch(&["status"]);
    assert!(ok, "{err}");
    assert!(out.starts_with("e2e: "), "{out}");
    let (ok, _, err) = unlatch(&["cat", "sub/missing"]);
    assert!(!ok);
    assert!(err.contains("not found"), "{err}");

    // SAFETY: plain kill(2) on our own child.
    unsafe {
        libc::kill(agent.id() as i32, libc::SIGTERM);
    }
    let t0 = Instant::now();
    loop {
        if let Ok(Some(st)) = agent.try_wait() {
            assert!(st.success(), "agent exit status {st}");
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "agent did not stop on SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
