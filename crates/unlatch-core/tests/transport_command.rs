//! `Transport::Command`: preamble exchange with a fake server that prints junk first.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use unlatch_core::transport::open;
use unlatch_core::{EngineConfig, Transport};
use unlatch_proto::ErrorCode;

const CLIENT_PREAMBLE_LEN: usize = 28;

/// `\000UNLATCH` + proto_min + proto_max (LE u16) + 16-byte build id, as printf octal.
fn preamble_printf(min: u16, max: u16) -> String {
    let mut s = String::from(r"\000UNLATCH");
    for b in min
        .to_le_bytes()
        .into_iter()
        .chain(max.to_le_bytes())
        .chain([7u8; 16])
    {
        s.push_str(&format!("\\{b:03o}"));
    }
    s
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    p
}

fn cfg(argv: Vec<String>, env: Vec<(String, String)>) -> EngineConfig {
    EngineConfig::new(
        "t",
        Transport::Command { argv, env },
        "/r",
        PathBuf::from("/tmp/unused-state"),
        "mac",
    )
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[tokio::test]
async fn junk_then_preamble_then_echo() {
    let d = tmp();
    let s = script(
        d.path(),
        "srv",
        &format!(
            "echo \"motd: $GREETING\"\necho 'Last login: yesterday'\nprintf '{}'\nexec cat",
            preamble_printf(1, 1)
        ),
    );
    let c = cfg(
        vec![s.display().to_string()],
        vec![("GREETING".into(), "hi there".into())],
    );
    let mut link = open(&c, false).await.expect("open");
    assert_eq!(link.proto, 1);
    assert_eq!(
        (
            link.server.proto_min,
            link.server.proto_max,
            link.server.build_id
        ),
        (1, 1, [7; 16])
    );
    assert_eq!(
        String::from_utf8_lossy(&link.junk),
        "motd: hi there\nLast login: yesterday\n"
    );
    assert!(link.child.is_some());
    // `cat` echoes our preamble first, then whatever we send: the stream starts right after
    // the server preamble, nothing lost or duplicated.
    let mut echoed = [0u8; CLIENT_PREAMBLE_LEN];
    link.reader
        .read_exact(&mut echoed)
        .await
        .expect("read echo");
    assert_eq!(&echoed[..8], b"\0UNLATCH");
    link.writer.write_all(b"frame bytes").await.expect("write");
    link.writer.flush().await.expect("flush");
    let mut back = [0u8; 11];
    link.reader.read_exact(&mut back).await.expect("read back");
    assert_eq!(&back, b"frame bytes");
}

#[tokio::test]
async fn bytes_after_the_preamble_in_the_same_write_are_kept() {
    let d = tmp();
    let s = script(
        d.path(),
        "srv",
        &format!(
            "printf 'x{}FRAMEDATA'\nexec cat >/dev/null",
            preamble_printf(1, 3)
        ),
    );
    let mut link = open(&cfg(vec![s.display().to_string()], vec![]), false)
        .await
        .expect("open");
    assert_eq!(link.proto, 1, "highest common version");
    let mut buf = [0u8; 9];
    link.reader.read_exact(&mut buf).await.expect("read");
    assert_eq!(&buf, b"FRAMEDATA");
    assert_eq!(link.junk, b"x");
}

#[tokio::test]
async fn exit_before_preamble_reports_stderr() {
    let d = tmp();
    let s = script(
        d.path(),
        "srv",
        "echo 'unlatchd: cannot open index: Permission denied' >&2\nexit 3",
    );
    let e = open(&cfg(vec![s.display().to_string()], vec![]), false)
        .await
        .err()
        .expect("must fail");
    assert_eq!(e.code, ErrorCode::Offline);
    assert!(e.msg.contains("Permission denied"), "{}", e.msg);
    assert!(e.msg.contains("exit 3"), "{}", e.msg);
}

#[tokio::test]
async fn no_common_version() {
    let d = tmp();
    let s = script(
        d.path(),
        "srv",
        &format!("printf '{}'\nexec cat >/dev/null", preamble_printf(5, 6)),
    );
    let e = open(&cfg(vec![s.display().to_string()], vec![]), false)
        .await
        .err()
        .expect("must fail");
    assert_eq!(e.code, ErrorCode::Protocol);
    assert!(e.msg.contains("update unlatchd"), "{}", e.msg);
}

#[tokio::test]
async fn endless_junk_is_a_protocol_error() {
    let d = tmp();
    let s = script(
        d.path(),
        "srv",
        "yes 'this is not unlatchd' | head -c 200000\nsleep 5",
    );
    let e = open(&cfg(vec![s.display().to_string()], vec![]), false)
        .await
        .err()
        .expect("must fail");
    assert_eq!(e.code, ErrorCode::Protocol);
    assert!(
        e.msg.contains("remote shell printed: this is not unlatchd"),
        "{}",
        e.msg
    );
}

#[tokio::test]
async fn missing_program_and_empty_argv() {
    let e = open(&cfg(vec!["/nonexistent/unlatchd".into()], vec![]), false)
        .await
        .err()
        .expect("must fail");
    assert_eq!(e.code, ErrorCode::Io);
    let e = open(&cfg(vec![], vec![]), false)
        .await
        .err()
        .expect("must fail");
    assert_eq!(e.code, ErrorCode::Protocol);
}

#[tokio::test]
async fn dropping_the_link_kills_the_server() {
    let d = tmp();
    let pidfile = d.path().join("pid");
    let s = script(
        d.path(),
        "srv",
        &format!(
            "echo $$ > '{}'\nprintf '{}'\nexec sleep 60",
            pidfile.display(),
            preamble_printf(1, 1)
        ),
    );
    let link = open(&cfg(vec![s.display().to_string()], vec![]), false)
        .await
        .expect("open");
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("pid")
        .trim()
        .parse()
        .expect("int");
    drop(link);
    let t0 = std::time::Instant::now();
    loop {
        let alive = Path::new(&format!("/proc/{pid}")).exists()
            && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .unwrap_or_default()
                .contains(") Z ");
        if !alive {
            break;
        }
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "server {pid} survived the link"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
