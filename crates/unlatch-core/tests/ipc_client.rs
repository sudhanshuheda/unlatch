//! `IpcClient` against a hand-written fake engine speaking raw frames: out-of-order replies,
//! progress routing, unknown/late frames, and `Offline` when the engine goes away.
//! (Dispatcher/server behaviour is covered by the unit tests in `src/ipc/tests.rs`.)

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use unlatch_core::ipc::IpcClient;
use unlatch_proto::frame;
use unlatch_proto::ipc::{IpcFrame, IpcRequest, IpcResponse};
use unlatch_proto::{ErrorCode, ItemId, PROTO_VERSION};

const T: Duration = Duration::from_secs(10);

fn read_req(s: &mut &UnixStream) -> Option<IpcFrame<IpcRequest>> {
    frame::read_blocking(s).expect("read request")
}

fn write(s: &mut &UnixStream, call: u64, msg: IpcResponse) {
    frame::write_blocking(s, &IpcFrame { call, msg }, false).expect("write reply");
}

/// Accept one connection, answer Hello, then hand the stream to `rest`.
fn fake_engine<F>(
    rest: F,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::thread::JoinHandle<()>,
)
where
    F: FnOnce(UnixStream) + Send + 'static,
{
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("e.sock");
    let l = UnixListener::bind(&path).expect("bind");
    let h = std::thread::spawn(move || {
        let (s, _) = l.accept().expect("accept");
        let mut r = &s;
        let hello = read_req(&mut r).expect("hello");
        match hello.msg {
            IpcRequest::Hello { proto, domain } => {
                assert_eq!(proto, PROTO_VERSION);
                write(&mut r, hello.call, IpcResponse::Hello { proto, domain });
            }
            other => panic!("first frame must be Hello, got {other:?}"),
        }
        rest(s);
    });
    (dir, path, h)
}

#[test]
fn out_of_order_replies_progress_and_stray_frames() {
    let (_d, path, h) = fake_engine(|s| {
        let mut r = &s;
        let a = read_req(&mut r).expect("a");
        let b = read_req(&mut r).expect("b");
        // Unknown call id and a Progress for nobody: both must be ignored.
        write(&mut r, 9999, IpcResponse::Ok);
        write(&mut r, 9998, IpcResponse::Progress { done: 1, total: 2 });
        // Answer the second request first, with progress interleaved for the first.
        write(&mut r, a.call, IpcResponse::Progress { done: 1, total: 3 });
        write(&mut r, b.call, IpcResponse::Anchor(vec![2]));
        write(&mut r, a.call, IpcResponse::Progress { done: 3, total: 3 });
        write(&mut r, a.call, IpcResponse::Anchor(vec![1]));
    });
    let c = Arc::new(IpcClient::connect(&path, "dom", T).expect("connect"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (c1, s1) = (Arc::clone(&c), Arc::clone(&seen));
    let first_id = c.next_call_id();
    let t = std::thread::spawn(move || {
        let cb = |d, t| s1.lock().expect("lock").push((d, t));
        c1.call(IpcRequest::Item { id: ItemId(1) }, None, Some(&cb))
    });
    // Make sure the first request is on the wire before the second.
    while c.next_call_id() == first_id {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(20));
    let second = c
        .call(IpcRequest::CurrentAnchor, None, None)
        .expect("second");
    assert_eq!(second, IpcResponse::Anchor(vec![2]));
    assert_eq!(
        t.join().expect("join").expect("first"),
        IpcResponse::Anchor(vec![1])
    );
    assert_eq!(*seen.lock().expect("lock"), vec![(1, 3), (3, 3)]);
    h.join().expect("engine");
}

#[test]
fn engine_disappearing_fails_pending_and_later_calls_with_offline() {
    let (_d, path, h) = fake_engine(|s| {
        let mut r = &s;
        let _pending = read_req(&mut r).expect("request");
        // Crash without replying.
        drop(s);
    });
    let c = IpcClient::connect(&path, "dom", T).expect("connect");
    let e = c.call(IpcRequest::Status, None, None).expect_err("offline");
    assert_eq!(e.code, ErrorCode::Offline);
    let e = c
        .call(IpcRequest::Status, None, None)
        .expect_err("still offline");
    assert_eq!(e.code, ErrorCode::Offline);
    h.join().expect("engine");
}

#[test]
fn connect_failures() {
    let dir = tempfile::tempdir().expect("tempdir");
    let e = IpcClient::connect(&dir.path().join("missing.sock"), "d", T)
        .err()
        .expect("no socket");
    assert_eq!(e.code, ErrorCode::Offline);

    // An engine that never answers Hello: bounded by the timeout.
    let path = dir.path().join("mute.sock");
    let l = UnixListener::bind(&path).expect("bind");
    let h = std::thread::spawn(move || {
        let (s, _) = l.accept().expect("accept");
        std::thread::sleep(Duration::from_millis(500));
        drop(s);
    });
    let t0 = std::time::Instant::now();
    let e = IpcClient::connect(&path, "d", Duration::from_millis(100))
        .err()
        .expect("timeout");
    assert_eq!(e.code, ErrorCode::Timeout);
    assert!(t0.elapsed() < Duration::from_secs(2));
    h.join().expect("mute engine");
}

#[test]
fn garbage_from_the_engine_is_offline_not_a_hang() {
    let (_d, path, h) = fake_engine(|s| {
        use std::io::Write;
        let mut r = &s;
        let _ = read_req(&mut r).expect("request");
        (&s).write_all(&[2, 0, 0, 0, 0, 0xff]).expect("garbage");
        std::thread::sleep(Duration::from_millis(200));
    });
    let c = IpcClient::connect(&path, "dom", T).expect("connect");
    let e = c.call(IpcRequest::Status, None, None).expect_err("offline");
    assert_eq!(e.code, ErrorCode::Offline);
    assert!(e.msg.contains("undecodable"), "{}", e.msg);
    h.join().expect("engine");
}
