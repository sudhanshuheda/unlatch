//! `Transport::Ssh` end to end: the real bootstrap script over a real `ssh localhost`, uploading a
//! fake `unlatchd` (a shell script that prints junk, a valid preamble, then echoes) into a
//! temporary install dir, plus the same script under dash and busybox through a fake `ssh`.
//!
//! The real `~/.unlatch` is never touched: `EngineConfig::remote_install_dir` points the probe at a
//! temp dir.
//! Set `UNLATCH_SKIP_SSH_TESTS=1` on machines without non-interactive `ssh localhost`.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use unlatch_core::transport::{open, Link};
use unlatch_core::{EngineConfig, Transport, UnlatchdBinary};
use unlatch_proto::ErrorCode;

const FAKE_UNLATCHD: &str = r#"#!/bin/sh
# Fake unlatchd: record argv + env, print junk, a valid v1 preamble, then echo stdin.
out="${FAKE_ARGS_OUT:-$UNLATCH_HOME/fake-args}"
{ printf '%s\n' "$@"; printf 'UNLATCH_HOME=%s\n' "${UNLATCH_HOME:-}"; } > "$out"
printf 'junk printed by unlatchd before its preamble\n'
printf '\000UNLATCH\001\000\001\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000'
exec cat
"#;

fn job_tmp() -> PathBuf {
    std::env::temp_dir()
}

struct Env {
    dir: tempfile::TempDir,
    payload: PathBuf,
    sha: String,
    home: PathBuf,
}

fn sha256_of(p: &Path) -> String {
    let out = Command::new("sha256sum")
        .arg(p)
        .output()
        .expect("sha256sum");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .split_whitespace()
        .next()
        .expect("hash")
        .to_string()
}

fn env() -> Env {
    let dir = tempfile::Builder::new()
        .prefix("ssh-e2e")
        .tempdir_in(job_tmp())
        .expect("tempdir");
    let payload = dir.path().join("fake-unlatchd");
    std::fs::write(&payload, FAKE_UNLATCHD).expect("write payload");
    std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let sha = sha256_of(&payload);
    let home = dir.path().join("remote-home");
    Env {
        dir,
        payload,
        sha,
        home,
    }
}

fn skip() -> bool {
    if std::env::var_os("UNLATCH_SKIP_SSH_TESTS").is_some() {
        eprintln!("UNLATCH_SKIP_SSH_TESTS set: skipping ssh localhost test");
        return true;
    }
    false
}

fn ssh_cfg(e: &Env, extra: &[&str], root: &str) -> EngineConfig {
    let mut extra_args: Vec<String> = vec!["-o".into(), "ControlMaster=no".into()];
    extra_args.extend(extra.iter().map(|s| s.to_string()));
    let mut cfg = EngineConfig::new(
        "e2e",
        Transport::Ssh {
            destination: "localhost".into(),
            port: None,
            identity: None,
            extra_args,
        },
        root,
        e.dir.path().join("state"),
        "mac",
    );
    cfg.unlatchd_upload = vec![UnlatchdBinary {
        arch: "x86_64".into(),
        path: e.payload.clone(),
        sha256_hex: e.sha.clone(),
    }];
    cfg.remote_install_dir = Some(e.home.display().to_string());
    cfg
}

fn remote_bin(e: &Env) -> PathBuf {
    e.home.join(format!(
        "unlatchd-{}-{}",
        env!("CARGO_PKG_VERSION"),
        &e.sha[..16]
    ))
}

/// The link reaches the fake unlatchd: it echoes our preamble, then our bytes.
async fn assert_echo(link: &mut Link) {
    let mut pre = [0u8; 28];
    link.reader
        .read_exact(&mut pre)
        .await
        .expect("echoed preamble");
    assert_eq!(
        &pre[..8],
        b"\0UNLATCH",
        "client preamble must reach unlatchd intact"
    );
    link.writer
        .write_all(b"ping-through-ssh\n")
        .await
        .expect("write");
    link.writer.flush().await.expect("flush");
    let mut back = [0u8; 17];
    link.reader.read_exact(&mut back).await.expect("echo");
    assert_eq!(&back, b"ping-through-ssh\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn bootstrap_upload_verify_reuse_and_repair() {
    if skip() {
        return;
    }
    let e = env();
    let root = "/tmp/it's a root";
    let cfg = ssh_cfg(&e, &[], root);

    // 1. First connect: install dir created 0700, binary uploaded and verified.
    let mut link = open(&cfg, false).await.expect("first open");
    assert_eq!(link.proto, 1);
    let junk = String::from_utf8_lossy(&link.junk).to_string();
    assert!(
        junk.contains("junk printed by unlatchd before its preamble"),
        "junk: {junk:?}"
    );
    assert!(
        !junk.contains("UNLATCH-"),
        "markers must not leak into junk: {junk:?}"
    );
    assert_echo(&mut link).await;
    let bin = remote_bin(&e);
    assert_eq!(
        std::fs::read(&bin).expect("uploaded binary"),
        FAKE_UNLATCHD.as_bytes()
    );
    assert_eq!(
        std::fs::metadata(&e.home)
            .expect("home")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&bin).expect("bin").permissions().mode() & 0o777,
        0o700
    );
    let args = std::fs::read_to_string(e.home.join("fake-args")).expect("args");
    assert_eq!(
        args,
        format!(
            "connect\n--root\n{root}\nUNLATCH_HOME={}\n",
            e.home.display()
        )
    );
    let ino1 = std::fs::metadata(&bin).expect("stat").ino();
    drop(link);

    // 2. Second connect: sha256 re-checked, no re-upload (same inode).
    let mut link = open(&cfg, false).await.expect("second open");
    assert_echo(&mut link).await;
    assert_eq!(
        std::fs::metadata(&bin).expect("stat").ino(),
        ino1,
        "binary must not be re-uploaded"
    );
    drop(link);

    // 3. Corrupt the remote binary in place (same inode): the next connect re-uploads it.
    std::fs::write(&bin, b"#!/bin/sh\necho tampered\n").expect("corrupt");
    assert_eq!(std::fs::metadata(&bin).expect("stat").ino(), ino1);
    // Keep the corrupted inode alive: the bootstrap unlinks the old binary before uploading, and
    // without an open handle the filesystem may hand the freed inode number straight to the new
    // file (ext4 does), which made the inode comparison below flaky.
    let mut old = std::fs::File::open(&bin).expect("open corrupted binary");
    let mut link = open(&cfg, false).await.expect("third open");
    assert_echo(&mut link).await;
    assert_eq!(
        std::fs::read(&bin).expect("repaired"),
        FAKE_UNLATCHD.as_bytes()
    );
    assert_ne!(
        std::fs::metadata(&bin).expect("stat").ino(),
        ino1,
        "repair must replace the file"
    );
    let mut before = String::new();
    std::io::Read::read_to_string(&mut old, &mut before).expect("read old inode");
    assert_eq!(
        before, "#!/bin/sh\necho tampered\n",
        "repair must not write into the old file"
    );
    drop(old);
    drop(link);

    // No temp files left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&e.home)
        .expect("readdir")
        .filter_map(|d| d.ok())
        .map(|d| d.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp") || n.starts_with(".unlatch-exec-test"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_checksum_is_retryable_and_leaves_nothing() {
    if skip() {
        return;
    }
    let e = env();
    let mut cfg = ssh_cfg(&e, &[], "/tmp");
    cfg.unlatchd_upload[0].sha256_hex = "ab".repeat(32);
    let err = open(&cfg, false)
        .await
        .err()
        .expect("verification must fail");
    assert_eq!(err.code, ErrorCode::Offline, "{}", err.msg);
    assert!(err.msg.contains("failed verification"), "{}", err.msg);
    let names: Vec<_> = std::fs::read_dir(&e.home)
        .expect("readdir")
        .filter_map(|d| d.ok())
        .map(|d| d.file_name())
        .collect();
    assert!(names.is_empty(), "{names:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_binary_for_the_vm_arch_needs_the_user() {
    if skip() {
        return;
    }
    let e = env();
    let mut cfg = ssh_cfg(&e, &[], "/tmp");
    cfg.unlatchd_upload[0].arch = "riscv64".into();
    let err = open(&cfg, false).await.err().expect("no binary");
    assert_eq!(err.code, ErrorCode::NeedsUser, "{}", err.msg);
    assert!(err.msg.contains("x86_64"), "{}", err.msg);
}

#[tokio::test(flavor = "multi_thread")]
async fn unlatchd_command_override_skips_bootstrap_and_expands_tilde() {
    if skip() {
        return;
    }
    let e = env();
    let out = e.dir.path().join("override-args");
    let mut cfg = ssh_cfg(&e, &[], "~/some proj");
    cfg.unlatchd_command = Some(format!(
        "env FAKE_ARGS_OUT='{}' '{}'",
        out.display(),
        e.payload.display()
    ));
    let mut link = open(&cfg, false).await.expect("open");
    assert_echo(&mut link).await;
    let home = std::env::var("HOME").expect("HOME");
    let args = std::fs::read_to_string(&out).expect("args");
    assert!(
        args.starts_with(&format!("connect\n--root\n{home}/some proj\n")),
        "{args:?}"
    );
    assert!(!e.home.exists(), "override must not probe or upload");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_host_key_needs_the_user() {
    if skip() {
        return;
    }
    let e = env();
    let cfg = ssh_cfg(
        &e,
        &[
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "GlobalKnownHostsFile=/dev/null",
            "-o",
            "StrictHostKeyChecking=yes",
        ],
        "/tmp",
    );
    let err = open(&cfg, false).await.err().expect("host key");
    assert_eq!(err.code, ErrorCode::NeedsUser, "{}", err.msg);
    assert!(
        err.msg.contains("Host key verification failed"),
        "{}",
        err.msg
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_port_is_offline() {
    if skip() {
        return;
    }
    let e = env();
    let mut cfg = ssh_cfg(&e, &[], "/tmp");
    if let Transport::Ssh { port, .. } = &mut cfg.transport {
        *port = Some(1);
    }
    let err = open(&cfg, false).await.err().expect("refused");
    assert_eq!(err.code, ErrorCode::Offline, "{}", err.msg);
    assert!(err.msg.contains("Connection refused"), "{}", err.msg);
}

/// dash and busybox sh read ahead on pipes (unlike bash). Run the real script under each via a
/// fake `ssh` that executes the remote command locally, to prove the upload and the client
/// preamble are never swallowed by the shell.
#[tokio::test(flavor = "multi_thread")]
async fn bootstrap_under_dash_and_busybox() {
    let shells: Vec<(&str, &str)> = [
        ("dash", "/bin/dash -s"),
        ("busybox", "/usr/bin/busybox sh -s"),
        ("bash", "/bin/bash -s"),
    ]
    .into_iter()
    .filter(|(_, cmd)| Path::new(cmd.split(' ').next().unwrap_or("")).exists())
    .collect();
    assert!(!shells.is_empty());
    for (name, shell_cmd) in shells {
        let e = env();
        let fakebin = e.dir.path().join("bin");
        std::fs::create_dir(&fakebin).expect("mkdir");
        let fake_ssh = fakebin.join("ssh");
        // The remote command is the last argument ("sh -s"); run the shell under test instead.
        std::fs::write(&fake_ssh, format!("#!/bin/sh\nexec {shell_cmd}\n")).expect("write");
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let mut cfg = ssh_cfg(&e, &[], "/tmp");
        let path = format!(
            "{}:{}",
            fakebin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        cfg.ssh_env.push(("PATH".into(), path));
        for round in 0..2 {
            let mut link = open(&cfg, false)
                .await
                .unwrap_or_else(|err| panic!("{name} round {round}: {err}"));
            assert_echo(&mut link).await;
            assert_eq!(
                std::fs::read(remote_bin(&e)).expect("bin"),
                FAKE_UNLATCHD.as_bytes(),
                "{name}"
            );
        }
    }
}
