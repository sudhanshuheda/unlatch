//! Unit tests for the IPC machinery (dispatcher, server, client, fd passing) with injected
//! handlers, and for the request→engine mapping with a mock engine.

use super::dispatch::{dispatch_api, Disposition, EngineApi};
use super::dispatcher::Handler;
use super::{serve_with_handler, sock, Dispatcher, IpcClient, IpcServerHandle};
use crate::{CancelToken, Changes, CreateRequest, Fetched, Modified, ModifyRequest, Page, Result};
use std::io::{Read, Seek, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Barrier, Mutex};
use std::time::{Duration, Instant};
use unlatch_proto::ipc::{
    ConnState, CreateKind, EngineStatus, IpcFrame, IpcItem, IpcRequest, IpcResponse, LocalMeta,
};
use unlatch_proto::{
    frame, BaseVersion, Entry, ErrorCode, ItemId, Kind, ProtoError, Version, PROTO_VERSION,
};

const T: Duration = Duration::from_secs(10);

fn tmpdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("hipc")
        .tempdir_in("/tmp")
        .expect("tempdir")
}

fn item(id: u64, name: &str) -> IpcItem {
    IpcItem {
        entry: Entry {
            id: ItemId(id),
            parent: ItemId::ROOT,
            name: name.to_string(),
            kind: Kind::File,
            size: 0,
            mtime_ns: 0,
            mode: 0o644,
            version: Version {
                content: 1,
                meta: 1,
            },
            symlink_target: None,
            lazy: false,
            seq: 1,
            access: 3,
        },
        display_name: name.to_string(),
        caps: 0,
        local: LocalMeta::default(),
        user_exec: false,
        symlink_blocked: false,
    }
}

type Reply<'a> = &'a mut dyn FnMut(IpcFrame<IpcResponse>);

/// Handler that answers Hello itself and delegates everything else to `f`.
fn handler<F>(f: F) -> Handler
where
    F: Fn(IpcFrame<IpcRequest>, Option<OwnedFd>, Reply<'_>, &CancelToken) -> Disposition
        + Send
        + Sync
        + 'static,
{
    Arc::new(
        move |req: IpcFrame<IpcRequest>, fd, reply: Reply<'_>, cancel: &CancelToken| {
            if let IpcRequest::Hello { proto, domain } = req.msg {
                reply(IpcFrame {
                    call: req.call,
                    msg: IpcResponse::Hello { proto, domain },
                });
                return Disposition::Continue;
            }
            f(req, fd, reply, cancel)
        },
    )
}

fn reply_item(req: &IpcFrame<IpcRequest>, reply: Reply<'_>, id: u64, name: &str) {
    reply(IpcFrame {
        call: req.call,
        msg: IpcResponse::Item(item(id, name)),
    });
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    server: Option<IpcServerHandle>,
}

fn start(h: Handler) -> Fixture {
    let dir = tmpdir();
    let path = dir.path().join("engine.sock");
    let server = serve_with_handler(h, &path).expect("serve");
    Fixture {
        _dir: dir,
        path,
        server: Some(server),
    }
}

fn connect(path: &Path) -> IpcClient {
    IpcClient::connect(path, "dom", T).expect("connect")
}

fn item_id(r: &IpcResponse) -> u64 {
    match r {
        IpcResponse::Item(it) => it.entry.id.0,
        other => panic!("expected Item, got {other:?}"),
    }
}

// ---- server / client / dispatcher ---------------------------------------------------------

#[test]
fn hello_and_socket_is_0600() {
    let fx = start(handler(|_, _, _, _| Disposition::Continue));
    let mode = std::fs::metadata(&fx.path)
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "socket mode {mode:o}");
    let _c = connect(&fx.path);
    // No stray temporary socket names are left next to it.
    let names: Vec<_> = std::fs::read_dir(fx.path.parent().expect("parent"))
        .expect("readdir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("engine.sock")]);
}

/// Replies leave as each call completes, not in submission order. All calls are in flight at
/// once (each handler parks on its own gate), the test opens the gates in a fixed order that is
/// never submission order, and each reply must reach its caller while every other call is still
/// parked. The order comes from the test, not from sleeps racing the scheduler.
#[test]
fn replies_are_multiplexed_out_of_order() {
    const N: u64 = 9;
    // Last-submitted first, then scrambled: never submission order.
    const RELEASE: [u64; N as usize] = [9, 3, 8, 1, 6, 2, 7, 5, 4];
    let mut gate_tx = std::collections::HashMap::new();
    let mut gate_rx = std::collections::HashMap::new();
    for i in 1..=N {
        let (tx, rx) = mpsc::channel::<()>();
        gate_tx.insert(i, tx);
        gate_rx.insert(i, rx);
    }
    let gate_rx = Mutex::new(gate_rx);
    let (started_tx, started_rx) = mpsc::channel::<u64>();
    let started_tx = Mutex::new(started_tx);
    let fx = start(handler(move |req, _, reply, _| {
        if let IpcRequest::Item { id } = req.msg {
            let gate = gate_rx.lock().expect("lock").remove(&id.0).expect("gate");
            let _ = started_tx.lock().expect("lock").send(id.0);
            // Released by the test, or by its gates dropping once it has failed; the bound only
            // outlasts the test's own wait so a missing reply is reported as such.
            let _ = gate.recv_timeout(3 * T);
            reply_item(&req, reply, id.0, "x");
        }
        Disposition::Continue
    }));
    let c = Arc::new(connect(&fx.path));
    let (done_tx, done_rx) = mpsc::channel::<(u64, u64)>();
    let mut threads = Vec::new();
    // Submit 1..=N strictly in order: call i+1 is sent only once call i's handler is running.
    for i in 1..=N {
        let (c, done_tx) = (Arc::clone(&c), done_tx.clone());
        threads.push(std::thread::spawn(move || {
            let r = c
                .call(IpcRequest::Item { id: ItemId(i) }, None, None)
                .expect("call");
            let _ = done_tx.send((i, item_id(&r)));
        }));
        assert_eq!(started_rx.recv_timeout(T).expect("handler started"), i);
    }
    let mut running: Vec<u64> = (1..=N).collect();
    for k in RELEASE {
        gate_tx
            .remove(&k)
            .expect("gate")
            .send(())
            .expect("open gate");
        running.retain(|&r| r != k);
        let (got, id) = done_rx.recv_timeout(T).unwrap_or_else(|_| {
            panic!("reply to call {k} not delivered while calls {running:?} were still running")
        });
        assert_eq!((got, id), (k, k), "released {k}, got a reply for {got}");
    }
    for t in threads {
        t.join().expect("join");
    }
    assert!(done_rx.try_recv().is_err(), "one reply per call");
}

#[test]
fn progress_frames_reach_the_callback_in_order() {
    let fx = start(handler(|req, _, reply, _| {
        for i in 1..=5 {
            reply(IpcFrame {
                call: req.call,
                msg: IpcResponse::Progress {
                    done: i * 10,
                    total: 50,
                },
            });
        }
        reply(IpcFrame {
            call: req.call,
            msg: IpcResponse::Fetched {
                path: "/p".into(),
                item: item(7, "f"),
            },
        });
        Disposition::Continue
    }));
    let c = connect(&fx.path);
    let seen = Mutex::new(Vec::new());
    let cb = |d: u64, t: u64| seen.lock().expect("lock").push((d, t));
    let r = c
        .call(
            IpcRequest::Fetch {
                id: ItemId(7),
                version: None,
                dest_dir: "/tmp".into(),
            },
            None,
            Some(&cb),
        )
        .expect("call");
    assert!(matches!(r, IpcResponse::Fetched { .. }), "{r:?}");
    assert_eq!(
        *seen.lock().expect("lock"),
        vec![(10, 50), (20, 50), (30, 50), (40, 50), (50, 50)]
    );
}

#[test]
fn cancel_replies_cancelled_and_fires_the_token() {
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let started_tx = Mutex::new(started_tx);
    // Sent once the late reply below has been handed to the dispatcher: carries whether the
    // handler saw its token cancelled.
    let (late_tx, late_rx) = mpsc::channel::<bool>();
    let late_tx = Mutex::new(late_tx);
    let fx = start(handler(move |req, _, reply, cancel| {
        if !matches!(req.msg, IpcRequest::Fetch { .. }) {
            reply_item(&req, reply, 77, "other");
            return Disposition::Continue;
        }
        let _ = started_tx.lock().expect("lock").send(());
        let t0 = Instant::now();
        while !cancel.is_cancelled() && t0.elapsed() < T {
            std::thread::sleep(Duration::from_millis(2));
        }
        // A late final reply after cancellation must be swallowed.
        reply(IpcFrame {
            call: req.call,
            msg: IpcResponse::Fetched {
                path: "/late".into(),
                item: item(1, "f"),
            },
        });
        let _ = late_tx.lock().expect("lock").send(cancel.is_cancelled());
        Disposition::Continue
    }));
    let c = Arc::new(connect(&fx.path));
    let target = c.next_call_id();
    let c2 = Arc::clone(&c);
    let worker = std::thread::spawn(move || {
        c2.call(
            IpcRequest::Fetch {
                id: ItemId(1),
                version: None,
                dest_dir: "/tmp".into(),
            },
            None,
            None,
        )
    });
    started_rx.recv_timeout(T).expect("handler started");
    let t0 = Instant::now();
    let r = c
        .call(IpcRequest::Cancel { call: target }, None, None)
        .expect("cancel call");
    assert_eq!(r, IpcResponse::Ok);
    let r = worker.join().expect("join").expect("call");
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert!(
        matches!(
            r,
            IpcResponse::Error {
                code: ErrorCode::Cancelled,
                ..
            }
        ),
        "{r:?}"
    );
    assert!(
        late_rx.recv_timeout(T).expect("handler finished"),
        "handler never saw its token cancelled"
    );
    // The late reply has gone through the dispatcher by now; the connection keeps working and
    // that reply did not leak into another call.
    assert_eq!(
        item_id(&c.call(IpcRequest::Status, None, None).expect("status")),
        77
    );
}

#[test]
fn cancel_of_unknown_call_is_ok() {
    let fx = start(handler(|_, _, _, _| Disposition::Continue));
    let c = connect(&fx.path);
    assert_eq!(
        c.call(IpcRequest::Cancel { call: 999 }, None, None)
            .expect("cancel"),
        IpcResponse::Ok
    );
}

fn content_handler() -> Handler {
    handler(|req, fd, reply, _| {
        let msg = match fd {
            Some(fd) => {
                let mut f = std::fs::File::from(fd);
                let mut s = String::new();
                f.read_to_string(&mut s).expect("read content fd");
                IpcResponse::Item(item(1, &s))
            }
            None => IpcResponse::Error {
                code: ErrorCode::Protocol,
                msg: "no fd".into(),
                current: None,
            },
        };
        reply(IpcFrame {
            call: req.call,
            msg,
        });
        Disposition::Continue
    })
}

fn create_req(has_content: bool) -> IpcRequest {
    IpcRequest::Create {
        template_id: "t".into(),
        parent: ItemId::ROOT,
        name: "n".into(),
        kind: CreateKind::File,
        has_content,
        symlink_target: None,
        mtime_ns: None,
        user_exec: None,
        changed_fields: 0,
        local: LocalMeta::default(),
        may_already_exist: false,
        deletion_conflicted: false,
    }
}

fn unlinked_file_with(content: &str) -> OwnedFd {
    let mut f = tempfile::tempfile().expect("tempfile");
    f.write_all(content.as_bytes()).expect("write");
    f.rewind().expect("rewind");
    OwnedFd::from(f)
}

#[test]
fn fd_passing_delivers_content_of_an_unlinked_file() {
    let fx = start(content_handler());
    let c = connect(&fx.path);
    let r = c
        .call(
            create_req(true),
            Some(unlinked_file_with("hello from the mac")),
            None,
        )
        .expect("call");
    match r {
        IpcResponse::Item(it) => assert_eq!(it.entry.name, "hello from the mac"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn client_enforces_fd_iff_has_content() {
    let fx = start(content_handler());
    let c = connect(&fx.path);
    let e = c
        .call(create_req(true), None, None)
        .expect_err("missing fd");
    assert_eq!(e.code, ErrorCode::Protocol);
    let e = c
        .call(create_req(false), Some(unlinked_file_with("x")), None)
        .expect_err("stray fd");
    assert_eq!(e.code, ErrorCode::Protocol);
    // Connection still usable.
    assert!(matches!(
        c.call(create_req(true), Some(unlinked_file_with("ok")), None),
        Ok(IpcResponse::Item(_))
    ));
}

/// Raw client: a stray fd on a frame that did not declare content is closed, and the next
/// frame's fd reaches the right call even when both frames arrive in one read.
#[test]
fn stray_fd_is_not_handed_to_the_next_call() {
    let fx = start(content_handler());
    let s = UnixStream::connect(&fx.path).expect("connect");
    let a = frame::encode(
        &IpcFrame {
            call: 1,
            msg: create_req(false),
        },
        false,
    )
    .expect("encode");
    let b = frame::encode(
        &IpcFrame {
            call: 2,
            msg: create_req(true),
        },
        false,
    )
    .expect("encode");
    let fa = unlinked_file_with("stray");
    let fb = unlinked_file_with("right");
    {
        use std::os::fd::AsFd;
        sock::send_frame(&s, &a, Some(fa.as_fd())).expect("send a");
        sock::send_frame(&s, &b, Some(fb.as_fd())).expect("send b");
    }
    let mut r = &s;
    let mut got = std::collections::HashMap::new();
    while got.len() < 2 {
        let body = frame::read_body_blocking(&mut r)
            .expect("read")
            .expect("frame");
        let f: IpcFrame<IpcResponse> = frame::decode_body(&body).expect("decode");
        got.insert(f.call, f.msg);
    }
    assert!(
        matches!(
            &got[&1],
            IpcResponse::Error {
                code: ErrorCode::Protocol,
                ..
            }
        ),
        "{:?}",
        got[&1]
    );
    match &got[&2] {
        IpcResponse::Item(it) => assert_eq!(it.entry.name, "right"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn hundred_concurrent_calls() {
    let fx = start(handler(|req, _, reply, _| {
        if let IpcRequest::Item { id } = req.msg {
            std::thread::sleep(Duration::from_millis(id.0 % 7));
            reply_item(&req, reply, id.0, "x");
        }
        Disposition::Continue
    }));
    let c = Arc::new(connect(&fx.path));
    let barrier = Arc::new(Barrier::new(100));
    let threads: Vec<_> = (0..100u64)
        .map(|i| {
            let (c, b) = (Arc::clone(&c), Arc::clone(&barrier));
            std::thread::spawn(move || {
                b.wait();
                let r = c
                    .call(
                        IpcRequest::Item {
                            id: ItemId(1000 + i),
                        },
                        None,
                        None,
                    )
                    .expect("call");
                assert_eq!(item_id(&r), 1000 + i);
            })
        })
        .collect();
    for t in threads {
        t.join().expect("join");
    }
}

#[test]
fn metadata_calls_do_not_queue_behind_transfers() {
    let release = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let (rel, run, fin) = (
        Arc::clone(&release),
        Arc::clone(&running),
        Arc::clone(&finished),
    );
    let fx = start(handler(move |req, _, reply, cancel| {
        match req.msg {
            IpcRequest::Fetch { .. } => {
                run.fetch_add(1, Ordering::SeqCst);
                // Parked until released (or torn down); the safety bound is far beyond any
                // scheduling delay so only a release can let a transfer finish.
                let t0 = Instant::now();
                while !rel.load(Ordering::SeqCst) && !cancel.is_cancelled() && t0.elapsed() < 6 * T
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                fin.fetch_add(1, Ordering::SeqCst);
                reply(IpcFrame {
                    call: req.call,
                    msg: IpcResponse::Fetched {
                        path: "/p".into(),
                        item: item(1, "f"),
                    },
                });
            }
            IpcRequest::Item { id } => reply_item(&req, reply, id.0, "meta"),
            _ => {}
        }
        Disposition::Continue
    }));
    let c = Arc::new(connect(&fx.path));
    let fetches: Vec<_> = (0..40)
        .map(|_| {
            let c = Arc::clone(&c);
            std::thread::spawn(move || {
                c.call(
                    IpcRequest::Fetch {
                        id: ItemId(1),
                        version: None,
                        dest_dir: "/tmp".into(),
                    },
                    None,
                    None,
                )
            })
        })
        .collect();
    wait_until("24 transfers running", || {
        running.load(Ordering::SeqCst) >= 24
    });
    // The transfer pool is saturated (24 running, 16 queued) and no transfer can finish until
    // released, yet metadata answers: it did not queue behind them. The proof is that no
    // transfer had finished when the answer came, not a latency bound; the wait only turns a
    // regression (an answer that never comes while transfers are parked) into a quick failure.
    let (item_tx, item_rx) = mpsc::channel();
    let c2 = Arc::clone(&c);
    let item_call = std::thread::spawn(move || {
        let _ = item_tx.send(c2.call(IpcRequest::Item { id: ItemId(5) }, None, None));
    });
    let r = item_rx
        .recv_timeout(T)
        .expect("metadata call queued behind the parked transfers")
        .expect("item");
    assert_eq!(item_id(&r), 5);
    assert_eq!(
        finished.load(Ordering::SeqCst),
        0,
        "item answered only after a transfer finished"
    );
    assert_eq!(
        running.load(Ordering::SeqCst),
        24,
        "transfer pool must be bounded"
    );
    release.store(true, Ordering::SeqCst);
    item_call.join().expect("join");
    for f in fetches {
        assert!(matches!(
            f.join().expect("join"),
            Ok(IpcResponse::Fetched { .. })
        ));
    }
}

#[test]
fn close_connection_disposition_makes_calls_offline() {
    let fx = start(handler(|req, _, reply, _| {
        match req.msg {
            IpcRequest::Delete { .. } => return Disposition::CloseConnection,
            IpcRequest::Item { id } => reply_item(&req, reply, id.0, "x"),
            _ => {}
        }
        Disposition::Continue
    }));
    let c = connect(&fx.path);
    assert_eq!(
        item_id(
            &c.call(IpcRequest::Item { id: ItemId(3) }, None, None)
                .expect("item")
        ),
        3
    );
    let e = c
        .call(
            IpcRequest::Delete {
                id: ItemId(3),
                base: BaseVersion::default(),
                recursive: false,
            },
            None,
            None,
        )
        .expect_err("dropped reply");
    assert_eq!(e.code, ErrorCode::Offline);
    let e = c
        .call(IpcRequest::Item { id: ItemId(3) }, None, None)
        .expect_err("dead connection");
    assert_eq!(e.code, ErrorCode::Offline);
    // The server keeps accepting new connections.
    let c2 = connect(&fx.path);
    assert_eq!(
        item_id(
            &c2.call(IpcRequest::Item { id: ItemId(4) }, None, None)
                .expect("item")
        ),
        4
    );
}

#[test]
fn server_drop_disconnects_clients_and_removes_socket() {
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let started_tx = Mutex::new(started_tx);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let mut fx = start(handler(move |req, _, reply, _| {
        if let IpcRequest::Item { id } = req.msg {
            let _ = started_tx.lock().expect("lock").send(());
            // Parked until the test is done: the reply can never beat the server drop.
            let _ = release_rx.lock().expect("lock").recv_timeout(T);
            reply_item(&req, reply, id.0, "x");
        }
        Disposition::Continue
    }));
    let c = Arc::new(connect(&fx.path));
    let c2 = Arc::clone(&c);
    let pending =
        std::thread::spawn(move || c2.call(IpcRequest::Item { id: ItemId(1) }, None, None));
    started_rx.recv_timeout(T).expect("call in flight");
    drop(fx.server.take());
    assert!(!fx.path.exists(), "socket file must be removed");
    let e = pending.join().expect("join").expect_err("offline");
    assert_eq!(e.code, ErrorCode::Offline);
    assert_eq!(
        c.call(IpcRequest::Status, None, None)
            .expect_err("offline")
            .code,
        ErrorCode::Offline
    );
    assert_eq!(
        IpcClient::connect(&fx.path, "d", T).err().map(|e| e.code),
        Some(ErrorCode::Offline)
    );
    drop(release_tx);
}

#[test]
fn stale_socket_is_replaced_and_live_one_refused() {
    let dir = tmpdir();
    let path = dir.path().join("s.sock");
    // A socket file nobody listens on.
    drop(std::os::unix::net::UnixListener::bind(&path).expect("bind"));
    assert!(path.exists());
    // Another test's process spawn may fork while that listener is open; the child holds a copy
    // of the descriptor until it execs, and until then connects succeed. Wait until it is stale.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::os::unix::net::UnixStream::connect(&path).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "socket never became stale"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let h = handler(|_, _, _, _| Disposition::Continue);
    let live = serve_with_handler(Arc::clone(&h), &path).expect("replace stale");
    let e = serve_with_handler(Arc::clone(&h), &path)
        .err()
        .expect("second server refused");
    assert_eq!(e.code, ErrorCode::Exists);
    drop(live);
    std::fs::write(&path, b"not a socket").expect("write");
    assert_eq!(
        serve_with_handler(h, &path).err().map(|e| e.code),
        Some(ErrorCode::Exists)
    );
}

#[test]
fn overlong_socket_path_is_a_readable_error() {
    let dir = tmpdir();
    let path = dir.path().join("x".repeat(150));
    let e = serve_with_handler(handler(|_, _, _, _| Disposition::Continue), &path)
        .err()
        .expect("too long");
    assert_eq!(e.code, ErrorCode::InvalidName);
    assert!(e.msg.contains("limit"), "{}", e.msg);
}

#[test]
fn handler_panic_becomes_an_error_reply() {
    let fx = start(handler(|req, _, reply, _| {
        if let IpcRequest::Item { id } = req.msg {
            if id.0 == 13 {
                panic!("boom");
            }
            reply_item(&req, reply, id.0, "x");
        }
        Disposition::Continue
    }));
    let c = connect(&fx.path);
    match c
        .call(IpcRequest::Item { id: ItemId(13) }, None, None)
        .expect("call")
    {
        IpcResponse::Error {
            code: ErrorCode::Io,
            msg,
            ..
        } => assert!(msg.contains("boom"), "{msg}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        item_id(
            &c.call(IpcRequest::Item { id: ItemId(2) }, None, None)
                .expect("call")
        ),
        2
    );
}

#[test]
fn missing_final_reply_is_synthesized() {
    let fx = start(handler(|_, _, _, _| Disposition::Continue));
    let c = connect(&fx.path);
    let r = c.call(IpcRequest::Status, None, None).expect("call");
    assert!(
        matches!(
            r,
            IpcResponse::Error {
                code: ErrorCode::Io,
                ..
            }
        ),
        "{r:?}"
    );
}

#[test]
fn undecodable_frame_closes_only_that_connection() {
    let fx = start(handler(|req, _, reply, _| {
        if let IpcRequest::Item { id } = req.msg {
            reply_item(&req, reply, id.0, "x");
        }
        Disposition::Continue
    }));
    let mut s = UnixStream::connect(&fx.path).expect("connect");
    s.write_all(&[3, 0, 0, 0, 0, 0xff, 0xff])
        .expect("write junk frame");
    let mut buf = [0u8; 16];
    s.set_read_timeout(Some(T)).expect("timeout");
    assert_eq!(
        s.read(&mut buf).expect("read"),
        0,
        "server must close the connection"
    );
    let c = connect(&fx.path);
    assert_eq!(
        item_id(
            &c.call(IpcRequest::Item { id: ItemId(8) }, None, None)
                .expect("call")
        ),
        8
    );
}

#[test]
fn hello_error_fails_connect() {
    let fx = start(Arc::new(
        |req: IpcFrame<IpcRequest>, _, reply: Reply<'_>, _: &CancelToken| {
            reply(IpcFrame {
                call: req.call,
                msg: IpcResponse::Error {
                    code: ErrorCode::Protocol,
                    msg: "nope".into(),
                    current: None,
                },
            });
            Disposition::Continue
        },
    ));
    let e = IpcClient::connect(&fx.path, "d", T)
        .err()
        .expect("hello refused");
    assert_eq!(e.code, ErrorCode::Protocol);
}

#[test]
fn dispatcher_drop_cancels_calls_and_stops_sending() {
    let sent = Arc::new(Mutex::new(Vec::<IpcFrame<IpcResponse>>::new()));
    let s2 = Arc::clone(&sent);
    let seen_cancel = Arc::new(AtomicBool::new(false));
    let sc = Arc::clone(&seen_cancel);
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let started_tx = Mutex::new(started_tx);
    // Dropped with the handler, i.e. once the dispatcher's last call has fully finished
    // (including any synthesized final frame).
    let alive = Arc::new(());
    let held = Arc::clone(&alive);
    let d = Dispatcher::with_handler(
        handler(move |req, _, reply, cancel| {
            let _held = &held;
            let _ = started_tx.lock().expect("lock").send(());
            let t0 = Instant::now();
            while !cancel.is_cancelled() && t0.elapsed() < T {
                std::thread::sleep(Duration::from_millis(2));
            }
            sc.store(cancel.is_cancelled(), Ordering::SeqCst);
            reply_item(&req, reply, 1, "late");
            Disposition::Continue
        }),
        Box::new(move |f| s2.lock().expect("lock").push(f)),
        None,
    );
    d.submit(
        IpcFrame {
            call: 1,
            msg: IpcRequest::Item { id: ItemId(1) },
        },
        None,
    );
    started_rx.recv_timeout(T).expect("started");
    drop(d);
    wait_until("the call to finish and the handler to be dropped", || {
        Arc::strong_count(&alive) == 1
    });
    assert!(seen_cancel.load(Ordering::SeqCst));
    assert!(
        sent.lock().expect("lock").is_empty(),
        "no frames after the connection is gone"
    );
}

#[test]
fn duplicate_call_id_is_refused() {
    let sent = Arc::new(Mutex::new(Vec::<IpcFrame<IpcResponse>>::new()));
    let s2 = Arc::clone(&sent);
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let d = Dispatcher::with_handler(
        handler(move |req, _, reply, _| {
            let _ = go_rx.lock().expect("lock").recv_timeout(T);
            reply_item(&req, reply, 1, "x");
            Disposition::Continue
        }),
        Box::new(move |f| s2.lock().expect("lock").push(f)),
        None,
    );
    d.submit(
        IpcFrame {
            call: 5,
            msg: IpcRequest::Item { id: ItemId(1) },
        },
        None,
    );
    d.submit(
        IpcFrame {
            call: 5,
            msg: IpcRequest::Item { id: ItemId(1) },
        },
        None,
    );
    go_tx.send(()).expect("go");
    let t0 = Instant::now();
    while sent.lock().expect("lock").len() < 2 && t0.elapsed() < T {
        std::thread::sleep(Duration::from_millis(2));
    }
    let sent = sent.lock().expect("lock");
    assert!(
        matches!(
            sent[0].msg,
            IpcResponse::Error {
                code: ErrorCode::Protocol,
                ..
            }
        ),
        "{:?}",
        sent[0]
    );
    assert!(matches!(sent[1].msg, IpcResponse::Item(_)), "{:?}", sent[1]);
}

// ---- request → engine mapping (mock engine) -----------------------------------------------

#[derive(Default)]
struct Mock {
    calls: Mutex<Vec<String>>,
    delete_err: Option<ErrorCode>,
    content_seen: Mutex<Option<String>>,
    upload_gate: bool,
}

impl Mock {
    fn log(&self, s: impl Into<String>) {
        self.calls.lock().expect("lock").push(s.into());
    }

    /// Simulated upload: `(0, n)`, a burst the dispatcher coalesces, then `(n, n)`; with
    /// `upload_gate` set, blocks after the first frame until the call is cancelled.
    fn upload(&self, n: u64, progress: &dyn Fn(u64, u64), cancel: &CancelToken) -> Result<()> {
        progress(0, n);
        if self.upload_gate {
            let t0 = Instant::now();
            while !cancel.is_cancelled() {
                assert!(t0.elapsed() < T, "upload never cancelled");
                std::thread::sleep(Duration::from_millis(2));
            }
            self.log("upload cancelled");
            return Err(ProtoError::new(ErrorCode::Cancelled, "cancelled"));
        }
        progress(n / 2, n);
        progress(n, n);
        Ok(())
    }
}

fn modified(id: u64) -> Modified {
    Modified {
        item: item(id, "m"),
        still_pending: 4,
        should_fetch_content: true,
        conflict_copy: Some(item(99, "c")),
    }
}

impl EngineApi for Mock {
    fn name(&self) -> &str {
        "dom"
    }
    fn item(&self, id: ItemId) -> Result<IpcItem> {
        self.log(format!("item {id}"));
        if id.0 == 404 {
            return Err(ProtoError::new(ErrorCode::NotFound, "gone"));
        }
        Ok(item(id.0, "cur"))
    }
    fn list(
        &self,
        container: ItemId,
        cursor: Option<&[u8]>,
        limit: u32,
        viewer: bool,
    ) -> Result<Page> {
        self.log(format!("list {container} {cursor:?} {limit} {viewer}"));
        Ok(Page {
            items: vec![item(2, "a")],
            next: Some(vec![9]),
        })
    }
    fn anchor(&self) -> Vec<u8> {
        vec![1, 2, 3]
    }
    fn changes_since(&self, anchor: &[u8], limit: u32) -> Result<Changes> {
        self.log(format!("changes {anchor:?} {limit}"));
        Ok(Changes {
            updated: vec![],
            removed: vec![ItemId(4)],
            anchor: vec![7],
            more: true,
        })
    }
    fn materialized_changed(&self, added: &[ItemId], removed: &[ItemId], full: bool) -> Result<()> {
        self.log(format!("mat {added:?} {removed:?} {full}"));
        Ok(())
    }
    fn fetch(
        &self,
        id: ItemId,
        _version: Option<u64>,
        dest_dir: &Path,
        progress: &dyn Fn(u64, u64),
        _cancel: &CancelToken,
    ) -> Result<Fetched> {
        self.log(format!("fetch {id}"));
        progress(0, 100);
        progress(1, 100); // coalesced: within 50 ms of the previous frame
        progress(100, 100); // completion is always reported
        Ok(Fetched {
            path: dest_dir.join("f"),
            item: item(id.0, "f"),
        })
    }
    fn create_with(
        &self,
        mut req: CreateRequest,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Modified> {
        self.log(format!("create {}", req.name));
        if let Some(f) = req.content.as_mut() {
            let mut s = String::new();
            f.read_to_string(&mut s).expect("read");
            let n = s.len() as u64;
            *self.content_seen.lock().expect("lock") = Some(s);
            self.upload(n, progress, cancel)?;
        }
        Ok(modified(10))
    }
    fn modify_with(
        &self,
        id: ItemId,
        _base: BaseVersion,
        req: ModifyRequest,
        progress: &dyn Fn(u64, u64),
        cancel: &CancelToken,
    ) -> Result<Modified> {
        self.log(format!(
            "modify {id} {:?} content={}",
            req.new_name,
            req.content.is_some()
        ));
        if let Some(f) = &req.content {
            self.upload(f.metadata().expect("stat").len(), progress, cancel)?;
        }
        Ok(modified(id.0))
    }
    fn delete(&self, id: ItemId, _base: BaseVersion, recursive: bool) -> Result<()> {
        self.log(format!("delete {id} {recursive}"));
        match self.delete_err {
            Some(code) => Err(ProtoError::new(code, "refused")),
            None => Ok(()),
        }
    }
    fn status(&self) -> EngineStatus {
        EngineStatus {
            state: ConnState::Live,
            entries: 3,
            anchor: vec![],
            rtt_us: None,
            cache_bytes: 0,
            pending_uploads: 0,
            server: None,
        }
    }
    fn confirm_paused(&self, apply: bool) -> Result<()> {
        self.log(format!("confirm {apply}"));
        Ok(())
    }
}

fn run(
    api: &Mock,
    req: IpcRequest,
    fd: Option<OwnedFd>,
    fault: &dyn Fn(&str) -> bool,
) -> (Vec<IpcResponse>, Disposition) {
    let mut out = Vec::new();
    let mut reply = |f: IpcFrame<IpcResponse>| {
        assert_eq!(f.call, 42);
        out.push(f.msg);
    };
    let d = dispatch_api(
        api,
        IpcFrame { call: 42, msg: req },
        fd,
        &mut reply,
        &CancelToken::new(),
        fault,
    );
    (out, d)
}

fn no_fault(_: &str) -> bool {
    false
}

fn one(api: &Mock, req: IpcRequest) -> IpcResponse {
    let (mut out, d) = run(api, req, None, &no_fault);
    assert_eq!(d, Disposition::Continue);
    assert_eq!(out.len(), 1, "{out:?}");
    out.remove(0)
}

#[test]
fn mapping_of_simple_requests() {
    let m = Mock::default();
    assert_eq!(
        one(
            &m,
            IpcRequest::Hello {
                proto: PROTO_VERSION,
                domain: "dom".into()
            }
        ),
        IpcResponse::Hello {
            proto: PROTO_VERSION,
            domain: "dom".into()
        }
    );
    assert!(matches!(
        one(
            &m,
            IpcRequest::Hello {
                proto: PROTO_VERSION + 1,
                domain: "dom".into()
            }
        ),
        IpcResponse::Error {
            code: ErrorCode::Protocol,
            ..
        }
    ));
    // The wrong engine for this domain: fail loudly instead of browsing another VM.
    match one(
        &m,
        IpcRequest::Hello {
            proto: PROTO_VERSION,
            domain: "other-vm".into(),
        },
    ) {
        IpcResponse::Error {
            code: ErrorCode::Protocol,
            msg,
            current: None,
        } => assert!(msg.contains("other-vm") && msg.contains("dom"), "{msg}"),
        other => panic!("expected Protocol error, got {other:?}"),
    }
    assert_eq!(
        one(&m, IpcRequest::Item { id: ItemId(5) }),
        IpcResponse::Item(item(5, "cur"))
    );
    assert_eq!(
        one(&m, IpcRequest::Item { id: ItemId(404) }),
        IpcResponse::Error {
            code: ErrorCode::NotFound,
            msg: "gone".into(),
            current: None
        }
    );
    assert_eq!(
        one(
            &m,
            IpcRequest::Enumerate {
                container: ItemId::ROOT,
                cursor: Some(vec![1]),
                limit: 10,
                viewer: true
            }
        ),
        IpcResponse::Page {
            items: vec![item(2, "a")],
            next: Some(vec![9])
        }
    );
    assert_eq!(
        one(&m, IpcRequest::CurrentAnchor),
        IpcResponse::Anchor(vec![1, 2, 3])
    );
    assert_eq!(
        one(
            &m,
            IpcRequest::ChangesSince {
                anchor: vec![5],
                limit: 7
            }
        ),
        IpcResponse::Changes {
            updated: vec![],
            removed: vec![ItemId(4)],
            anchor: vec![7],
            more: true
        }
    );
    assert_eq!(
        one(
            &m,
            IpcRequest::MaterializedChanged {
                added: vec![ItemId(2)],
                removed: vec![],
                full: true
            }
        ),
        IpcResponse::Ok
    );
    assert!(matches!(
        one(&m, IpcRequest::Status),
        IpcResponse::Status(EngineStatus { entries: 3, .. })
    ));
    assert_eq!(
        one(&m, IpcRequest::ConfirmPaused { apply: true }),
        IpcResponse::Ok
    );
    assert_eq!(one(&m, IpcRequest::Cancel { call: 1 }), IpcResponse::Ok);
    let calls = m.calls.lock().expect("lock").clone();
    assert!(
        calls.contains(&"list 1 Some([1]) 10 true".to_string()),
        "{calls:?}"
    );
    assert!(calls.contains(&"changes [5] 7".to_string()), "{calls:?}");
    assert!(
        calls.contains(&"mat [ItemId(2)] [] true".to_string()),
        "{calls:?}"
    );
    assert!(calls.contains(&"confirm true".to_string()), "{calls:?}");
}

#[test]
fn fetch_streams_coalesced_progress_then_fetched() {
    let m = Mock::default();
    let (out, _) = run(
        &m,
        IpcRequest::Fetch {
            id: ItemId(3),
            version: Some(2),
            dest_dir: "/tmp/d".into(),
        },
        None,
        &no_fault,
    );
    assert_eq!(
        out,
        vec![
            IpcResponse::Progress {
                done: 0,
                total: 100
            },
            IpcResponse::Progress {
                done: 100,
                total: 100
            },
            IpcResponse::Fetched {
                path: "/tmp/d/f".into(),
                item: item(3, "f")
            },
        ]
    );
}

#[test]
fn create_and_modify_take_content_from_the_fd() {
    let m = Mock::default();
    let r = run(
        &m,
        create_req(true),
        Some(unlinked_file_with("payload")),
        &no_fault,
    )
    .0;
    assert_eq!(
        r,
        vec![
            IpcResponse::Progress { done: 0, total: 7 },
            IpcResponse::Progress { done: 7, total: 7 },
            IpcResponse::Done {
                item: item(10, "m"),
                still_pending: 4,
                should_fetch_content: true,
                conflict_copy: Some(item(99, "c")),
            }
        ]
    );
    assert_eq!(
        m.content_seen.lock().expect("lock").as_deref(),
        Some("payload")
    );
    // Declared content without an fd never reaches the engine.
    let before = m.calls.lock().expect("lock").len();
    assert!(matches!(
        one(&m, create_req(true)),
        IpcResponse::Error {
            code: ErrorCode::Protocol,
            ..
        }
    ));
    assert_eq!(m.calls.lock().expect("lock").len(), before);
    // An fd on a request that did not declare content is ignored.
    *m.content_seen.lock().expect("lock") = None;
    let _ = run(
        &m,
        create_req(false),
        Some(unlinked_file_with("stray")),
        &no_fault,
    );
    assert_eq!(*m.content_seen.lock().expect("lock"), None);

    let modify = |has_content| IpcRequest::Modify {
        id: ItemId(8),
        base: BaseVersion::default(),
        changed_fields: 1,
        new_parent: None,
        new_name: Some("r".into()),
        has_content,
        mtime_ns: None,
        user_exec: None,
        local: LocalMeta::default(),
    };
    let out = run(&m, modify(true), Some(unlinked_file_with("x")), &no_fault).0;
    assert!(
        matches!(
            out.as_slice(),
            [
                IpcResponse::Progress { done: 0, total: 1 },
                IpcResponse::Progress { done: 1, total: 1 },
                IpcResponse::Done { .. }
            ]
        ),
        "{out:?}"
    );
    // Without content: no progress frames.
    let out = run(&m, modify(false), None, &no_fault).0;
    assert!(
        matches!(out.as_slice(), [IpcResponse::Done { .. }]),
        "{out:?}"
    );
    assert!(m
        .calls
        .lock()
        .expect("lock")
        .contains(&"modify 8 Some(\"r\") content=true".to_string()));
    assert!(matches!(
        one(&m, modify(true)),
        IpcResponse::Error {
            code: ErrorCode::Protocol,
            ..
        }
    ));
}

#[test]
fn deletion_rejected_carries_the_current_item() {
    let del = IpcRequest::Delete {
        id: ItemId(6),
        base: BaseVersion::default(),
        recursive: true,
    };
    let ok = Mock::default();
    assert_eq!(one(&ok, del.clone()), IpcResponse::Deleted);

    let rejected = Mock {
        delete_err: Some(ErrorCode::DeletionRejected),
        ..Mock::default()
    };
    assert_eq!(
        one(&rejected, del.clone()),
        IpcResponse::Error {
            code: ErrorCode::DeletionRejected,
            msg: "refused".into(),
            current: Some(item(6, "cur"))
        }
    );

    let other = Mock {
        delete_err: Some(ErrorCode::Offline),
        ..Mock::default()
    };
    assert_eq!(
        one(&other, del),
        IpcResponse::Error {
            code: ErrorCode::Offline,
            msg: "refused".into(),
            current: None
        }
    );
    assert!(!other
        .calls
        .lock()
        .expect("lock")
        .iter()
        .any(|c| c.starts_with("item")));
}

#[test]
fn die_before_ipc_reply_runs_the_op_then_withholds_the_reply() {
    let m = Mock::default();
    let armed = |t: &str| t == "die_before_ipc_reply:create";
    let (out, d) = run(&m, create_req(true), Some(unlinked_file_with("c")), &armed);
    assert_eq!(d, Disposition::CloseConnection);
    // Upload progress may have gone out; the final reply never does.
    assert!(
        out.iter()
            .all(|r| matches!(r, IpcResponse::Progress { .. })),
        "{out:?}"
    );
    assert!(
        m.calls
            .lock()
            .expect("lock")
            .contains(&"create n".to_string()),
        "op must have run"
    );
    // Other kinds are unaffected.
    let (out, d) = run(
        &m,
        IpcRequest::Delete {
            id: ItemId(1),
            base: BaseVersion::default(),
            recursive: false,
        },
        None,
        &armed,
    );
    assert_eq!((out.len(), d), (1, Disposition::Continue));
    for (req, kind) in [
        (
            IpcRequest::Delete {
                id: ItemId(1),
                base: BaseVersion::default(),
                recursive: false,
            },
            "delete",
        ),
        (
            IpcRequest::Fetch {
                id: ItemId(1),
                version: None,
                dest_dir: "/tmp".into(),
            },
            "fetch",
        ),
    ] {
        let tok = format!("die_before_ipc_reply:{kind}");
        let armed = |t: &str| t == tok;
        let (out, d) = run(&m, req, None, &armed);
        assert_eq!(d, Disposition::CloseConnection, "{kind}");
        assert!(
            !out.iter()
                .any(|r| !matches!(r, IpcResponse::Progress { .. })),
            "{kind}: {out:?}"
        );
    }
}

/// End to end through the socket: the fault closes the connection → the client sees `Offline`.
#[test]
fn fault_disposition_over_the_socket() {
    let api = Arc::new(Mock::default());
    let a2 = Arc::clone(&api);
    let h: Handler = Arc::new(move |req, fd, reply, cancel| {
        dispatch_api(&*a2, req, fd, reply, cancel, &|t: &str| {
            t == "die_before_ipc_reply:modify"
        })
    });
    let fx = start(h);
    let c = connect(&fx.path);
    let e = c
        .call(
            IpcRequest::Modify {
                id: ItemId(8),
                base: BaseVersion::default(),
                changed_fields: 0,
                new_parent: None,
                new_name: None,
                has_content: false,
                mtime_ns: None,
                user_exec: None,
                local: LocalMeta::default(),
            },
            None,
            None,
        )
        .expect_err("reply withheld");
    assert_eq!(e.code, ErrorCode::Offline);
    assert!(api
        .calls
        .lock()
        .expect("lock")
        .iter()
        .any(|c| c.starts_with("modify 8")));
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < T, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Create with content through the multiplexing dispatcher: upload progress streams as
/// `Progress` frames, and `Cancel` answers the call `Cancelled` at once while the engine's upload
/// observes its token (docs/PROTOCOL-NOTES.md, "Upload progress").
#[test]
fn create_upload_progress_and_cancel_through_the_dispatcher() {
    let api = Arc::new(Mock {
        upload_gate: true,
        ..Mock::default()
    });
    let a2 = Arc::clone(&api);
    let h: Handler = Arc::new(move |req, fd, reply, cancel| {
        dispatch_api(&*a2, req, fd, reply, cancel, &no_fault)
    });
    let sent = Arc::new(Mutex::new(Vec::<IpcFrame<IpcResponse>>::new()));
    let s2 = Arc::clone(&sent);
    let d = Dispatcher::with_handler(h, Box::new(move |f| s2.lock().expect("lock").push(f)), None);
    d.submit(
        IpcFrame {
            call: 5,
            msg: create_req(true),
        },
        Some(unlinked_file_with("0123456789")),
    );
    wait_until("first progress frame", || {
        sent.lock()
            .expect("lock")
            .iter()
            .any(|f| f.call == 5 && f.msg == IpcResponse::Progress { done: 0, total: 10 })
    });
    d.submit(
        IpcFrame {
            call: 6,
            msg: IpcRequest::Cancel { call: 5 },
        },
        None,
    );
    wait_until("engine saw the cancel", || {
        api.calls
            .lock()
            .expect("lock")
            .contains(&"upload cancelled".to_string())
    });
    std::thread::sleep(Duration::from_millis(20));
    let frames = sent.lock().expect("lock").clone();
    let finals: Vec<_> = frames
        .iter()
        .filter(|f| f.call == 5 && !matches!(f.msg, IpcResponse::Progress { .. }))
        .collect();
    assert_eq!(finals.len(), 1, "{frames:?}");
    assert!(
        matches!(
            finals[0].msg,
            IpcResponse::Error {
                code: ErrorCode::Cancelled,
                ..
            }
        ),
        "{frames:?}"
    );
    assert!(frames
        .iter()
        .any(|f| f.call == 6 && f.msg == IpcResponse::Ok));
}

/// `on_close` (public as `Dispatcher::with_close`) runs exactly once when the dispatcher closes
/// the connection for `die_before_ipc_reply`, the reply is withheld, and nothing is sent after.
#[test]
fn on_close_runs_once_when_the_fault_closes_the_connection() {
    let api = Arc::new(Mock::default());
    let a2 = Arc::clone(&api);
    let h: Handler = Arc::new(move |req, fd, reply, cancel| {
        dispatch_api(&*a2, req, fd, reply, cancel, &|t: &str| {
            t == "die_before_ipc_reply:delete"
        })
    });
    let sent = Arc::new(Mutex::new(Vec::<IpcFrame<IpcResponse>>::new()));
    let s2 = Arc::clone(&sent);
    let closes = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&closes);
    let d = Dispatcher::with_handler(
        h,
        Box::new(move |f| s2.lock().expect("lock").push(f)),
        Some(Box::new(move || {
            c2.fetch_add(1, Ordering::SeqCst);
        })),
    );
    d.submit(
        IpcFrame {
            call: 1,
            msg: IpcRequest::Status,
        },
        None,
    );
    wait_until("status reply", || sent.lock().expect("lock").len() == 1);
    assert_eq!(closes.load(Ordering::SeqCst), 0);
    d.submit(
        IpcFrame {
            call: 2,
            msg: IpcRequest::Delete {
                id: ItemId(9),
                base: BaseVersion::default(),
                recursive: false,
            },
        },
        None,
    );
    wait_until("on_close", || closes.load(Ordering::SeqCst) == 1);
    assert!(api
        .calls
        .lock()
        .expect("lock")
        .contains(&"delete 9 false".to_string()));
    // Closed: later submissions are dropped, dropping the dispatcher does not re-run on_close.
    d.submit(
        IpcFrame {
            call: 3,
            msg: IpcRequest::Status,
        },
        None,
    );
    drop(d);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    let frames = sent.lock().expect("lock").clone();
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0].call, 1);
}
