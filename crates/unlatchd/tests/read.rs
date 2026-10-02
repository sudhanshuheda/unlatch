//! Streamed reads under credit (D10) and torn-read protection (§2(d)5).

mod common;
use common::*;
use std::time::{Duration, Instant};
use unlatch_proto::wire::{ClientMsg, Request, Response, ServerMsg};
use unlatch_proto::ErrorCode;

fn session(root: &std::path::Path, state: &std::path::Path) -> Client {
    let mut c = Client::spawn(root, state, Opts::default());
    c.wait_snapshot();
    c
}

/// Incompressible bytes (xorshift): credit is charged in wire bytes, so window assertions need
/// data LZ4 cannot shrink.
fn pattern(n: usize) -> Vec<u8> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(n);
    v
}

#[test]
fn read_whole_ranged_and_expect() {
    let (root, state) = (tmp(), tmp());
    let data = pattern(300_000);
    std::fs::write(root.path().join("f.bin"), &data).unwrap();
    std::fs::write(root.path().join("empty"), b"").unwrap();
    let mut c = session(root.path(), state.path());
    let f = c.find("f.bin").unwrap();
    let (got, ver) = c.read(f.id, None).unwrap();
    assert_eq!(got, data);
    assert_eq!(ver, f.version.content);
    let (got, _) = c.read(f.id, Some(f.version.content)).unwrap();
    assert_eq!(got.len(), data.len());
    assert_eq!(
        c.read(f.id, Some(f.version.content + 1)).unwrap_err().code,
        ErrorCode::VersionMismatch
    );
    let e = c.find("empty").unwrap();
    let (z, _) = c.read(e.id, None).unwrap();
    assert!(z.is_empty());
    // ranged
    let rid = c.request(Request::Read {
        id: f.id,
        offset: 1000,
        len: Some(70_000),
        expect: None,
    });
    let mut out = Vec::new();
    loop {
        match c.next_for(rid, T).unwrap() {
            ServerMsg::ReadChunk {
                data: d,
                last,
                offset,
                ..
            } => {
                assert_eq!(offset as usize, 1000 + out.len());
                out.extend_from_slice(&d);
                let raw = c.take_raw(rid);
                c.grant(raw);
                if last {
                    break;
                }
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(out, &data[1000..71_000]);
    // directories cannot be read
    let d = c.stat(unlatch_proto::ItemId::ROOT).unwrap();
    assert_eq!(c.read(d.id, None).unwrap_err().code, ErrorCode::IsDir);
}

#[test]
fn file_modified_mid_stream_ends_with_version_mismatch() {
    let (root, state) = (tmp(), tmp());
    let p = root.path().join("big");
    std::fs::write(&p, pattern(2 << 20)).unwrap();
    let mut c = session(root.path(), state.path());
    let f = c.find("big").unwrap();
    // Grant nothing: the server stops after the implicit 256 KiB window.
    let rid = c.request(Request::Read {
        id: f.id,
        offset: 0,
        len: None,
        expect: None,
    });
    let mut got = 0usize;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(500) {
        if let Some(ServerMsg::ReadChunk { data, .. }) = c.next_for(rid, Duration::from_millis(50))
        {
            got += data.len();
        }
    }
    // One window of credit (measured in raw frame bytes, so slightly less data).
    assert!(
        got > 256 * 1024 - 1024 && got <= 256 * 1024,
        "one window without credit: {got}"
    );
    // The agent rewrites the file (same size) while we are mid-stream.
    std::fs::write(
        &p,
        pattern(2 << 20)
            .iter()
            .map(|b| b ^ 0xff)
            .collect::<Vec<u8>>(),
    )
    .unwrap();
    c.send(&ClientMsg::Credit {
        bulk_bytes: 64 << 20,
    });
    let mut last_seen = false;
    loop {
        match c.next_for(rid, T).expect("stream end") {
            ServerMsg::ReadChunk { last, .. } => {
                if last {
                    last_seen = true;
                    break;
                }
            }
            ServerMsg::Error { err, .. } => {
                assert_eq!(err.code, ErrorCode::VersionMismatch);
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(!last_seen, "a torn read must never end with last=true");
}

#[test]
fn credit_bounds_bulk_and_pings_stay_fast() {
    let (root, state) = (tmp(), tmp());
    let size = 50 << 20;
    std::fs::write(root.path().join("50mb"), pattern(size)).unwrap();
    let mut c = session(root.path(), state.path());
    let f = c.find("50mb").unwrap();
    let rid = c.request(Request::Read {
        id: f.id,
        offset: 0,
        len: None,
        expect: None,
    });
    let window: u64 = 256 * 1024;
    let (base_recv, base_granted) = (c.bulk_received, c.bulk_granted);
    let mut received_data: u64 = 0;
    let mut max_outstanding: u64 = 0;
    let mut pong_lat: Vec<Duration> = Vec::new();
    let mut chunks = 0u64;
    let mut done = false;
    let mut hasher = blake3::Hasher::new();
    while !done {
        match c.next_for(rid, T).expect("chunk") {
            ServerMsg::ReadChunk { data, last, .. } => {
                received_data += data.len() as u64;
                hasher.update(&data);
                let received = c.bulk_received - base_recv;
                let granted = c.bulk_granted - base_granted;
                let outstanding = received.saturating_sub(granted);
                max_outstanding = max_outstanding.max(outstanding);
                // At most one frame of overshoot (a frame may dip the balance below zero).
                assert!(
                    received <= window + granted + 64,
                    "server exceeded credit: {received} > {window} + {granted}"
                );
                chunks += 1;
                // Grant back what we consumed, like the engine.
                let raw = c.take_raw(rid);
                c.grant(raw);
                if chunks & 63 == 0 {
                    let t0 = Instant::now();
                    let pid = c.request(Request::Ping { nonce: chunks });
                    match c.next_for(pid, T).unwrap() {
                        ServerMsg::Response {
                            resp: Response::Pong { .. },
                            ..
                        } => {}
                        other => panic!("{other:?}"),
                    }
                    pong_lat.push(t0.elapsed());
                }
                done = last;
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(received_data, size as u64);
    assert_eq!(
        *hasher.finalize().as_bytes(),
        *blake3::hash(&pattern(size)).as_bytes()
    );
    pong_lat.sort();
    let p99 = pong_lat[(pong_lat.len() * 99 / 100).min(pong_lat.len() - 1)];
    eprintln!(
        "50 MB read: max outstanding {max_outstanding} B, {} pings, pong p50 {:?} p99 {:?}",
        pong_lat.len(),
        pong_lat[pong_lat.len() / 2],
        p99
    );
    assert!(max_outstanding <= window + 64);
    assert!(p99 < Duration::from_millis(250), "pong p99 {p99:?}");
}

#[test]
fn cancel_stops_a_read() {
    let (root, state) = (tmp(), tmp());
    std::fs::write(root.path().join("f"), pattern(4 << 20)).unwrap();
    let mut c = session(root.path(), state.path());
    let f = c.find("f").unwrap();
    let rid = c.request(Request::Read {
        id: f.id,
        offset: 0,
        len: None,
        expect: None,
    });
    // take the first window, then cancel and grant a lot: nothing more may arrive
    let mut got = 0;
    while got < 256 * 1024 - 1024 {
        if let Some(ServerMsg::ReadChunk { data, .. }) = c.next_for(rid, T) {
            got += data.len();
        }
    }
    c.send(&ClientMsg::Cancel { req_id: rid });
    std::thread::sleep(Duration::from_millis(100));
    c.send(&ClientMsg::Credit {
        bulk_bytes: 8 << 20,
    });
    c.ping();
    let mut after = 0;
    while let Some(m) = c.next_for(rid, Duration::from_millis(200)) {
        if let ServerMsg::ReadChunk { data, .. } = m {
            after += data.len();
        }
    }
    assert!(
        after <= 64 * 1024,
        "at most one in-flight chunk after Cancel: {after}"
    );
    // The session still works.
    let (d, _) = c.read(f.id, None).unwrap();
    assert_eq!(d.len(), 4 << 20);
}

#[test]
fn upload_cancel_discards_staged_data() {
    let (root, state) = (tmp(), tmp());
    let mut c = session(root.path(), state.path());
    let data = pattern(1 << 20);
    let rid = c.request(Request::Write {
        op: op(1),
        parent: unlatch_proto::ItemId::ROOT,
        name: "partial".into(),
        target: None,
        base: None,
        size: data.len() as u64,
        content_hash: *blake3::hash(&data).as_bytes(),
        mtime_ns: None,
        exec: None,
        move_to: None,
        may_exist: false,
    });
    c.send(&ClientMsg::WriteChunk {
        req_id: rid,
        data: data[..65536].to_vec(),
        last: false,
    });
    c.send(&ClientMsg::Cancel { req_id: rid });
    assert_eq!(c.response(rid).unwrap_err().code, ErrorCode::Cancelled);
    assert!(!root.path().join("partial").exists());
    c.ping();
    assert!(c.find("partial").is_none());
    // late chunks for the cancelled upload are ignored (and credited)
    c.send(&ClientMsg::WriteChunk {
        req_id: rid,
        data: vec![0; 1000],
        last: true,
    });
    c.ping();
}
