//! A foreign-uid squatter that binds the per-root socket name and lets its accept queue fill must
//! not hang `unlatchd connect`: a blocking connect(2) to such a name parks in `unix_wait_for_peer`
//! forever, which would let any other user on the VM deny service.
mod common;
use common::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Binds the name, then fills its own accept queue (listen(0) plus non-blocking self-connects)
/// and never accepts.
const SQUAT_FULL: &str = r#"
import socket, sys, time
name = b"\0" + sys.argv[1].encode("latin-1")
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.bind(name)
s.listen(0)
held = []
while True:
    c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    c.setblocking(False)
    try:
        c.connect(name)
    except BlockingIOError:
        break
    held.append(c)
print("bound", len(held), flush=True)
time.sleep(120)
"#;

const FOREIGN_UID: [&str; 6] = [
    "--map-auto",
    "--map-root-user",
    "setpriv",
    "--reuid=1",
    "--regid=1",
    "--clear-groups",
];

/// The exact chain the squatter uses works here (some hosts map the namespace but cannot switch
/// to a second uid).
fn foreign_uid_available() -> bool {
    Command::new("unshare")
        .args(FOREIGN_UID)
        .args([
            "python3",
            "-c",
            "import os, sys; sys.exit(0 if os.getuid() == 1 else 1)",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn squatter_with_a_full_backlog_cannot_block_connect() {
    if !foreign_uid_available() {
        eprintln!("SKIP: no subordinate uid mapping (unshare --map-auto + setpriv)");
        return;
    }
    let (root, state) = (tmp(), tmp());
    let rootc = std::fs::canonicalize(root.path()).unwrap();
    let statec = std::fs::canonicalize(state.path()).unwrap();
    let predictable = unlatchd::lifecycle::socket_name(&rootc, &statec, None);
    use std::os::unix::ffi::OsStrExt;
    let mut squat = Command::new("unshare")
        .args(FOREIGN_UID)
        .args(["python3", "-c", SQUAT_FULL])
        .arg(std::ffi::OsStr::from_bytes(&predictable))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    {
        use std::io::BufRead;
        std::io::BufReader::new(squat.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
    }
    assert!(line.starts_with("bound"), "squatter did not bind: {line:?}");

    // `Client::spawn` with `connect` panics with "no Welcome" if `unlatchd connect` hangs.
    let t0 = Instant::now();
    let mut c = Client::spawn(
        root.path(),
        state.path(),
        Opts {
            connect: true,
            ..Default::default()
        },
    );
    c.wait_snapshot();
    let took = t0.elapsed();
    drop(c);
    let _ = squat.kill();
    let _ = squat.wait();
    let _ = Command::new(BIN)
        .args(["stop", "--state"])
        .arg(state.path())
        .output();
    assert!(
        took < Duration::from_secs(30),
        "connect took {took:?} next to a squatter with a full accept queue"
    );
}
