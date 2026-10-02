//! Integration tests of the kernel-shaped network lab (needs unprivileged user namespaces).

use std::io::{Read, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use unlatch_bench::netlab::{self, Netlab, Profile, Service, ServiceHost};

fn bench_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_unlatch-bench"))
}

/// `unshare -rn` may be forbidden (e.g. some CI sandboxes): skip rather than fail there.
fn netns_available() -> bool {
    Command::new("unshare")
        .args(["-rn", "true"])
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn short_dir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("hnlt-{tag}-"))
        .tempdir_in("/tmp")
        .expect("tempdir")
}

#[test]
fn calibration_matches_profile() {
    if !netns_available() {
        eprintln!("skipping: unprivileged netns unavailable");
        return;
    }
    let dir = short_dir("cal");
    let p = Profile::parse("rtt20-bw50").unwrap();
    let lab = Netlab::start(&p, dir.path(), &bench_exe()).unwrap();
    let c = netlab::calibrate(&lab, 1.0, Duration::from_millis(1500)).unwrap();
    // RTT on lo = 2 × one-way netem delay.
    assert!((19.5..24.0).contains(&c.rtt_ms_p50), "rtt {c:?}");
    // Goodput is below the line rate (headers, slow start) but in the right range.
    assert!((25.0..52.0).contains(&c.down_mbit), "down {c:?}");
    assert!((25.0..52.0).contains(&c.up_mbit), "up {c:?}");
    // tcp_slow_start_after_idle=1: an idle connection restarts from IW10.
    assert!(
        c.fetch_256k_idle_ms > c.fetch_256k_warm_ms + 10.0,
        "idle {c:?}"
    );
}

#[test]
fn unshaped_profile_is_fast() {
    if !netns_available() {
        return;
    }
    let dir = short_dir("fast");
    let lab = Netlab::start(&Profile::parse("rtt0").unwrap(), dir.path(), &bench_exe()).unwrap();
    let _e = ServiceHost::start(dir.path(), "echo", Service::Echo).unwrap();
    let mut s = lab.connect("echo").unwrap();
    let rtts = netlab::echo_rtt(&mut s, 20).unwrap();
    let mut ms: Vec<f64> = rtts.iter().map(|d| d.as_secs_f64() * 1e3).collect();
    ms.sort_by(f64::total_cmp);
    assert!(ms[10] < 2.0, "{ms:?}");
}

#[test]
fn command_service_through_bridge_binary_and_stats() {
    if !netns_available() {
        return;
    }
    let dir = short_dir("cmd");
    let lab = Netlab::start(&Profile::parse("rtt10").unwrap(), dir.path(), &bench_exe()).unwrap();
    let _cat = ServiceHost::start(dir.path(), "cat", Service::command(vec!["cat".into()])).unwrap();
    // The stdio bridge binary, with trailing args ignored (sshfs ssh_command style).
    let mut argv = lab.connect_argv("cat");
    argv.extend(["-x", "-a", "host", "-s", "sftp"].map(String::from));
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = vec![7u8; 300_000];
    let mut stdin = child.stdin.take().unwrap();
    let p2 = payload.clone();
    let w = std::thread::spawn(move || {
        stdin.write_all(&p2).unwrap();
    });
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    w.join().unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(out, payload);
    let st = lab.stats().unwrap();
    let c = st.get("cat").expect("cat stats");
    assert_eq!((c.up, c.down, c.conns), (300_000, 300_000, 1));
}

#[test]
fn unknown_service_closes_connection() {
    if !netns_available() {
        return;
    }
    let dir = short_dir("unk");
    let lab = Netlab::start(&Profile::parse("rtt0").unwrap(), dir.path(), &bench_exe()).unwrap();
    let mut s = lab.connect("nobody-home").unwrap();
    s.shutdown(Shutdown::Write).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    assert!(buf.is_empty());
}

#[test]
fn dropping_the_lab_removes_the_namespace() {
    if !netns_available() {
        return;
    }
    let dir = short_dir("drop");
    let lab = Netlab::start(&Profile::parse("rtt0").unwrap(), dir.path(), &bench_exe()).unwrap();
    let pid = lab.holder_pid();
    let sock = lab.client_sock();
    assert!(sock.exists());
    drop(lab);
    assert!(!Path::new(&format!("/proc/{pid}")).exists() || proc_is_zombie(pid));
    assert!(!sock.exists());
}

fn proc_is_zombie(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| s.split(' ').nth(2) == Some("Z"))
        .unwrap_or(true)
}
