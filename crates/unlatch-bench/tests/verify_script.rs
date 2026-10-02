//! `scripts/verify.sh` honours `UNLATCH_NO_NETNS=1` (CI runners without unprivileged netns): the netns /
//! netem stages are reported SKIPPED (never PASS), visibly, and the correctness gates are not
//! affected. Runs only the netns stages so it stays fast; results go to a temp dir.

use std::path::PathBuf;
use std::process::Command;

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/verify.sh")
}

fn run(no_netns: Option<&str>, args: &[&str]) -> (bool, String) {
    let results = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new("bash");
    cmd.arg(script())
        .args(args)
        .env("VERIFY_RESULTS", results.path())
        .env_remove("UNLATCH_NO_NETNS");
    if let Some(v) = no_netns {
        cmd.env("UNLATCH_NO_NETNS", v);
    }
    let out = cmd.output().expect("run verify.sh");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn no_netns_skips_the_bench_visibly() {
    let (ok, out) = run(Some("1"), &["--quick", "--only", "bench,compare"]);
    assert!(ok, "{out}");
    for stage in ["bench", "compare"] {
        assert!(
            out.lines().any(|l| l.starts_with(&format!("== {stage}"))
                && l.contains("SKIPPED (UNLATCH_NO_NETNS=1")),
            "no visible SKIPPED line for {stage}:\n{out}"
        );
        assert!(
            out.lines()
                .any(|l| l.starts_with(stage) && l.contains("SKIP") && !l.contains("PASS")),
            "summary row for {stage}:\n{out}"
        );
    }
    assert!(out.contains("Scorecard: SKIPPED"), "{out}");
    assert!(
        out.contains("VERIFY: PASS (quick, bench SKIPPED: UNLATCH_NO_NETNS=1)"),
        "{out}"
    );
}

#[test]
fn only_the_netns_stages_are_affected() {
    // The skip list is exactly the stages that need `unshare -rn` / `tc netem`.
    let text = std::fs::read_to_string(script()).expect("read verify.sh");
    let stages = text
        .lines()
        .find_map(|l| l.strip_prefix("NETNS_STAGES="))
        .expect("NETNS_STAGES in verify.sh");
    assert_eq!(stages, "\",bench,compare,\"");
    // Without the variable nothing is skipped on its account (stages excluded by --only are
    // "skipped by option", not by UNLATCH_NO_NETNS).
    let (_, out) = run(None, &["--quick", "--only", "none"]);
    assert!(!out.contains("UNLATCH_NO_NETNS"), "{out}");
    let (_, out) = run(Some("0"), &["--quick", "--only", "none"]);
    assert!(!out.contains("SKIPPED (UNLATCH_NO_NETNS"), "{out}");
    for gate in ["fmt", "clippy", "test", "build", "fpsim", "fuzz"] {
        assert!(
            !stages.contains(&format!(",{gate},")),
            "{gate} must never be skipped for lack of netns"
        );
    }
}
