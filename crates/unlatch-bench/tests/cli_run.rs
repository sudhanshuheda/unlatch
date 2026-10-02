//! End-to-end CLI tests: `run` on a tiny tree, `compare` exit codes.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use unlatch_bench::measure::{Better, Measurement, Status, System};
use unlatch_bench::report::RunReport;

fn exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_unlatch-bench"))
}

fn netns_available() -> bool {
    Command::new("unshare")
        .args(["-rn", "true"])
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn write_report(dir: &std::path::Path, name: &str, v: f64) -> PathBuf {
    let r = RunReport {
        version: 1,
        measurements: vec![Measurement::new(
            "T1",
            "list_1000_p50_ms",
            System::Unlatch,
            "rtt40-bw50",
            "ms",
            Better::Lower,
        )
        .value(v, 10)],
        ..Default::default()
    };
    let p = dir.join(name);
    r.save(&p).unwrap();
    p
}

#[test]
fn compare_exit_codes() {
    let d = tempfile::tempdir().unwrap();
    let base = write_report(d.path(), "base.json", 1.0);
    let same = write_report(d.path(), "same.json", 1.05);
    let worse = write_report(d.path(), "worse.json", 2.0);
    let ok = Command::new(exe())
        .args(["compare"])
        .arg(&base)
        .arg(&same)
        .status()
        .unwrap();
    assert_eq!(ok.code(), Some(0));
    let bad = Command::new(exe())
        .args(["compare"])
        .arg(&base)
        .arg(&worse)
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stdout).contains("REGRESSION"));
    let usage = Command::new(exe())
        .args(["compare", "only-one.json"])
        .output()
        .unwrap();
    assert_ne!(usage.status.code(), Some(0));
}

#[test]
fn unknown_command_and_help() {
    assert_eq!(
        Command::new(exe())
            .arg("nope")
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .code(),
        Some(2)
    );
    let h = Command::new(exe()).arg("--help").output().unwrap();
    assert!(h.status.success());
    assert!(String::from_utf8_lossy(&h.stdout).contains("compare"));
}

/// Tiny tree, unshaped link: local + sshfs baselines produce numbers; Unlatch rows with a missing
/// `unlatchd` degrade to `unavailable` instead of failing the run.
#[test]
fn run_tiny_tree_end_to_end() {
    if !netns_available() {
        eprintln!("skipping: unprivileged netns unavailable");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("results/run.json");
    let mut cmd = Command::new(exe());
    cmd.args([
        "run",
        "--quick",
        "--tiny-tree",
        "--profile",
        "rtt0",
        "--only",
        "T1,T3,T16",
    ])
    .arg("--out")
    .arg(&out)
    .arg("--work")
    .arg(d.path().join("work"))
    .args([
        "--unlatchd",
        "/nonexistent/unlatchd",
        "--unlatch",
        "/nonexistent/unlatch",
    ]);
    let sshfs_ok = ["/usr/lib/openssh/sftp-server"]
        .iter()
        .all(|p| std::path::Path::new(p).exists())
        && Command::new("sh")
            .args(["-c", "command -v sshfs || test -x $HOME/.local/bin/sshfs"])
            .stdout(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        && std::path::Path::new("/dev/fuse").exists();
    if !sshfs_ok {
        cmd.args(["--systems", "local,unlatch"]);
    }
    let o = cmd.output().unwrap();
    assert!(
        o.status.success(),
        "stdout {} stderr {}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    let r = RunReport::load(&out).unwrap();
    assert!(d.path().join("results/SCORECARD.md").exists());
    let local_t1 = r
        .measurements
        .iter()
        .find(|m| m.id == "T1" && m.system == System::Local)
        .unwrap();
    assert_eq!(local_t1.status, Status::Ok);
    assert!(local_t1.value.unwrap() > 0.0);
    let unlatch: Vec<_> = r
        .measurements
        .iter()
        .filter(|m| m.system == System::Unlatch)
        .collect();
    assert!(!unlatch.is_empty());
    assert!(
        unlatch.iter().all(|m| m.status == Status::Unavailable),
        "{unlatch:?}"
    );
    assert!(r.profiles.iter().any(|p| p.name == "daemon"));
    assert_eq!(r.calibrations.len(), 1);
    if sshfs_ok {
        let cold = r
            .measurements
            .iter()
            .find(|m| m.id == "T3" && m.system == System::Sshfs && m.metric == "ls_la_cold_ms")
            .unwrap();
        assert_eq!(cold.status, Status::Ok, "{cold:?}");
    }
}
