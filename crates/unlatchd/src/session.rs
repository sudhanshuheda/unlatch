//! One wire session (review §2(b) wire.rs): preamble, Hello/Welcome, snapshot walker, request
//! dispatch, uploads and streamed reads under credit, Cancel.
//!
//! Threads per session: the reader (this function's caller thread), one writer (drains the
//! two-lane outbox), a small worker pool for requests, one thread per in-flight Read/Write,
//! and a snapshot walker while a snapshot is streaming.

use crate::core::{entry_size, perr, Core, FRAME_BUDGET};
use crate::ops::{sanitize_client, Chunk, Ops, WriteReq};
use crate::outbox::{credit_cost, SessionShared};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use unlatch_proto::frame::{self, Preamble, BULK_CHUNK};
use unlatch_proto::wire::{ClientMsg, Request, Response, ServerMsg};
use unlatch_proto::{Entry, ErrorCode, ItemId, ProtoError, PROTO_VERSION};

const WORKERS: usize = 4;
/// Bytes a streamed `Read` reads from disk ahead of the client's credit.
const READ_AHEAD: usize = 256 * 1024;
/// Upload credit is returned in steps of at least this size. The engine sizes its upload
/// chunks to its credit unit (4–64 KiB), so in practice every chunk is returned as consumed.
const CREDIT_UNIT: usize = 4 * 1024;
const MAX_READS: usize = 32;

pub fn our_preamble() -> Preamble {
    Preamble {
        proto_min: PROTO_VERSION as u16,
        proto_max: PROTO_VERSION as u16,
        build_id: [0; 16],
    }
}

struct Sess {
    core: Arc<Core>,
    shared: Arc<SessionShared>,
    client: Mutex<String>,
    writes: Mutex<HashMap<u32, Sender<Chunk>>>,
    reads: Mutex<HashMap<u32, Arc<AtomicBool>>>,
    active_reads: AtomicUsize,
    /// Upload bytes consumed but not yet granted back.
    ungranted: Mutex<u64>,
}

impl Sess {
    fn send(&self, msg: &ServerMsg) {
        if let Ok(f) = frame::encode(msg, true) {
            self.shared.out.push_inter(f);
        }
    }

    fn reply(&self, req_id: u32, r: Result<Response, ProtoError>) {
        // Nothing carrying an id or seq above the durable reservation goes out (D4).
        let r = r.and_then(|resp| self.core.deliverable().map(|_| resp));
        match r {
            Ok(resp) => self.send(&ServerMsg::Response { req_id, resp }),
            Err(err) => self.send(&ServerMsg::Error {
                req_id: Some(req_id),
                err,
            }),
        }
    }

    /// Grant upload credit back as it is consumed (batched to ≥ [`CREDIT_UNIT`], or immediately
    /// when `flush`). The engine's upload window counts bytes until they come back here, so a
    /// coarse batch would idle the link or need a larger standing queue.
    fn consumed(&self, n: usize, flush: bool) {
        let grant = {
            let Ok(mut u) = self.ungranted.lock() else {
                return;
            };
            *u += n as u64;
            if *u >= CREDIT_UNIT as u64 || (flush && *u > 0) {
                let g = *u;
                *u = 0;
                g
            } else {
                0
            }
        };
        if grant > 0 {
            self.send(&ServerMsg::Credit {
                bulk_bytes: grant.min(u32::MAX as u64) as u32,
            });
        }
    }

    fn ops(&self) -> Ops<'_> {
        let client = self
            .client
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|_| "mac".into());
        Ops {
            core: &self.core,
            client,
            session: self.shared.id,
        }
    }

    fn closed(&self) -> bool {
        self.shared.is_closed()
    }
}

/// Split a listing into ≤ 64 KiB `ListingPart`s.
pub fn listing_parts(req_id: u32, dir: &Entry, entries: Vec<Entry>) -> Vec<Vec<u8>> {
    let mut parts: Vec<Vec<Entry>> = Vec::new();
    let mut cur = Vec::new();
    let mut size = entry_size(dir);
    for e in entries {
        let sz = entry_size(&e);
        if !cur.is_empty() && size + sz > FRAME_BUDGET {
            parts.push(std::mem::take(&mut cur));
            size = entry_size(dir);
        }
        size += sz;
        cur.push(e);
    }
    parts.push(cur);
    let n = parts.len();
    parts
        .into_iter()
        .enumerate()
        .filter_map(|(i, entries)| {
            let resp = Response::ListingPart {
                dir: dir.clone(),
                entries,
                last: i + 1 == n,
            };
            frame::encode(&ServerMsg::Response { req_id, resp }, true).ok()
        })
        .collect()
}

/// Run one session over a byte stream until EOF. `close_out` is invoked at the end to make the
/// peer see EOF promptly (sockets: shutdown both directions).
pub fn run<Rd: Read, Wr: Write + Send + 'static>(
    core: Arc<Core>,
    mut rd: Rd,
    mut wr: Wr,
    close_out: Box<dyn FnOnce() + Send>,
) -> std::io::Result<()> {
    wr.write_all(&our_preamble().to_bytes())?;
    wr.flush()?;
    let shared = core.new_session();
    let writer = {
        let sh = shared.clone();
        std::thread::Builder::new()
            .name("writer".into())
            .spawn(move || {
                if let Err(e) = sh.out.run_writer(&mut wr) {
                    crate::log!("session {}: write: {e}", sh.id);
                }
                sh.close();
            })?
    };
    let sess = Arc::new(Sess {
        core: core.clone(),
        shared: shared.clone(),
        client: Mutex::new("mac".into()),
        writes: Mutex::new(HashMap::new()),
        reads: Mutex::new(HashMap::new()),
        active_reads: AtomicUsize::new(0),
        ungranted: Mutex::new(0),
    });
    let r = serve(&sess, &mut rd);
    if let Err(e) = &r {
        crate::log!("session {}: {e}", shared.id);
    }
    // Teardown: stop reads, abort uploads, unregister, flush the writer, close the stream.
    if let Ok(m) = sess.reads.lock() {
        for c in m.values() {
            c.store(true, Ordering::Relaxed);
        }
    }
    if let Ok(mut w) = sess.writes.lock() {
        w.clear();
    }
    core.remove_session(&shared);
    shared.close();
    let _ = writer.join();
    close_out();
    Ok(())
}

fn serve<Rd: Read>(sess: &Arc<Sess>, rd: &mut Rd) -> std::io::Result<()> {
    let (theirs, junk) =
        frame::read_preamble_blocking(rd).map_err(|e| std::io::Error::other(e.to_string()))?;
    if !junk.is_empty() {
        crate::log!(
            "session {}: {} junk bytes before the client preamble",
            sess.shared.id,
            junk.len()
        );
    }
    let Some(proto) = frame::negotiate(&our_preamble(), &theirs) else {
        sess.send(&ServerMsg::Error {
            req_id: None,
            err: perr(
                ErrorCode::Protocol,
                format!(
                    "no common protocol version (client {}..={}, unlatchd {PROTO_VERSION})",
                    theirs.proto_min, theirs.proto_max
                ),
            ),
        });
        return Ok(());
    };
    // Hello.
    let first: Option<ClientMsg> =
        frame::read_blocking(rd).map_err(|e| std::io::Error::other(e.to_string()))?;
    let Some(ClientMsg::Hello {
        root,
        resume,
        default_lazy_names,
        client_name,
        ..
    }) = first
    else {
        sess.send(&ServerMsg::Error {
            req_id: None,
            err: perr(ErrorCode::Protocol, "expected Hello"),
        });
        return Ok(());
    };
    if let Ok(mut c) = sess.client.lock() {
        *c = sanitize_client(&client_name);
    }
    let snapshot = match sess.core.hello(
        &sess.shared,
        &root,
        resume.map(|r| (r.index, r.seq)),
        &default_lazy_names,
        proto as u32,
    ) {
        Ok(s) => s,
        Err(err) => {
            sess.send(&ServerMsg::Error { req_id: None, err });
            return Ok(());
        }
    };
    // Widen the upload window beyond the implicit 256 KiB (below the ssh 2 MiB window, D10).
    sess.send(&ServerMsg::Credit {
        bulk_bytes: 768 * 1024,
    });
    if snapshot {
        let s = sess.clone();
        std::thread::Builder::new()
            .name("snapshot".into())
            .spawn(move || snapshot_walker(&s))?;
    }
    // Worker pool for interactive requests and metadata mutations.
    let (job_tx, job_rx) = mpsc::channel::<(u32, Request)>();
    let job_rx = Arc::new(Mutex::new(job_rx));
    let mut workers = Vec::new();
    for _ in 0..WORKERS {
        let s = sess.clone();
        let rx = job_rx.clone();
        workers.push(
            std::thread::Builder::new()
                .name("req".into())
                .spawn(move || loop {
                    let job = match rx.lock() {
                        Ok(r) => r.recv(),
                        Err(_) => return,
                    };
                    let Ok((req_id, req)) = job else { return };
                    if s.closed() {
                        continue;
                    }
                    handle(&s, req_id, req);
                })?,
        );
    }
    let res = loop {
        let msg: Option<ClientMsg> = match frame::read_blocking(rd) {
            Ok(m) => m,
            Err(e) => break Err(std::io::Error::other(e.to_string())),
        };
        let Some(msg) = msg else { break Ok(()) };
        if sess.closed() {
            break Ok(());
        }
        match msg {
            ClientMsg::Hello { .. } => {
                sess.send(&ServerMsg::Error {
                    req_id: None,
                    err: perr(ErrorCode::Protocol, "duplicate Hello"),
                });
                break Ok(());
            }
            ClientMsg::Credit { bulk_bytes } => sess.shared.credit.grant(bulk_bytes),
            ClientMsg::Cancel { req_id } => {
                if let Ok(m) = sess.reads.lock() {
                    if let Some(c) = m.get(&req_id) {
                        c.store(true, Ordering::Relaxed);
                    }
                }
                // Dropping the sender aborts the upload (staged data discarded).
                if let Ok(mut w) = sess.writes.lock() {
                    w.remove(&req_id);
                }
                sess.shared.credit.wake();
            }
            ClientMsg::WriteChunk { req_id, data, last } => {
                let n = data.len();
                let delivered = match sess.writes.lock() {
                    Ok(w) => match w.get(&req_id) {
                        Some(tx) => tx.send(Chunk { data, last }).is_ok(),
                        None => false,
                    },
                    Err(_) => false,
                };
                if !delivered {
                    // Unknown / finished / cancelled upload: still return the credit.
                    sess.consumed(n, true);
                }
            }
            ClientMsg::Request { req_id, req } => match req {
                Request::Read {
                    id,
                    offset,
                    len,
                    expect,
                } => start_read(sess, req_id, id, offset, len, expect),
                Request::Write { .. } => start_write(sess, req_id, req),
                other => {
                    if job_tx.send((req_id, other)).is_err() {
                        break Ok(());
                    }
                }
            },
        }
    };
    drop(job_tx);
    sess.shared.close();
    for w in workers {
        let _ = w.join();
    }
    res
}

fn handle(sess: &Arc<Sess>, req_id: u32, req: Request) {
    let core = &sess.core;
    match req {
        Request::Ping { nonce } => {
            let r = core.barrier().map(|seq| Response::Pong { nonce, seq });
            sess.reply(req_id, r);
        }
        Request::Stat { id } => sess.reply(req_id, core.stat(id).map(Response::Entry)),
        Request::ListDir { dir } => match core
            .list_dir(sess.shared.id, dir)
            .and_then(|l| core.deliverable().map(|_| l))
        {
            Ok((d, entries)) => {
                let frames = listing_parts(req_id, &d, entries);
                let credit = &sess.shared.credit;
                if frames.len() <= 1 {
                    // One small part: interactive lane, charged without waiting.
                    for f in &frames {
                        credit.charge(credit_cost(f));
                    }
                    sess.shared.out.push_inter_many(&frames);
                } else {
                    // Large listing: bulk lane under credit (interactive frames keep flowing).
                    for f in frames {
                        if !credit.wait_positive(&|| sess.closed()) {
                            return;
                        }
                        credit.charge(credit_cost(&f));
                        sess.shared.out.push_bulk(f);
                    }
                }
            }
            Err(e) => sess.reply(req_id, Err(e)),
        },
        Request::Unwatch { dir } => {
            let r = core
                .unwatch(sess.shared.id, dir)
                .and_then(|_| core.stat(dir))
                .map(Response::Entry);
            sess.reply(req_id, r);
        }
        Request::Mkdir { .. }
        | Request::Symlink { .. }
        | Request::Rename { .. }
        | Request::Remove { .. }
        | Request::SetAttr { .. }
            if mutation_blocked(sess, req_id) => {}
        Request::Mkdir {
            op,
            parent,
            name,
            may_exist,
        } => sess.reply(req_id, sess.ops().mkdir(op, parent, &name, may_exist)),
        Request::Symlink {
            op,
            parent,
            name,
            target,
        } => sess.reply(req_id, sess.ops().symlink(op, parent, &name, &target)),
        Request::Rename {
            op,
            id,
            base_parent,
            base_name,
            new_parent,
            new_name,
        } => sess.reply(
            req_id,
            sess.ops()
                .rename(op, id, base_parent, &base_name, new_parent, &new_name),
        ),
        Request::Remove {
            op,
            id,
            base,
            recursive,
            seen_seq,
        } => sess.reply(req_id, sess.ops().remove(op, id, base, recursive, seen_seq)),
        Request::SetAttr {
            op,
            id,
            exec,
            mtime_ns,
        } => sess.reply(req_id, sess.ops().setattr(op, id, exec, mtime_ns)),
        Request::Read { .. } | Request::Write { .. } => {}
    }
}

/// Reserve ids/seqs ahead of a mutation, before it touches the VM: with the state dir full or
/// not writable the mutation fails cleanly (and the client retries it later). True: refused.
fn mutation_blocked(sess: &Arc<Sess>, req_id: u32) -> bool {
    match sess.core.reserve_for_mutation() {
        Ok(()) => false,
        Err(e) => {
            sess.reply(req_id, Err(e));
            true
        }
    }
}

fn start_write(sess: &Arc<Sess>, req_id: u32, req: Request) {
    if mutation_blocked(sess, req_id) {
        // The upload's chunks are dropped (and credited back) as for an unknown request.
        return;
    }
    let Request::Write {
        op,
        parent,
        name,
        target,
        base,
        size,
        content_hash,
        mtime_ns,
        exec,
        move_to,
        may_exist,
    } = req
    else {
        return;
    };
    let (tx, rx) = mpsc::channel::<Chunk>();
    if let Ok(mut w) = sess.writes.lock() {
        w.insert(req_id, tx);
    }
    let s = sess.clone();
    let wr = WriteReq {
        op,
        parent,
        name,
        target,
        base,
        size,
        content_hash,
        mtime_ns,
        exec,
        move_to,
        may_exist,
    };
    let spawned = std::thread::Builder::new()
        .name("upload".into())
        .spawn(move || {
            let r = s.ops().write(wr, &rx, &|n| s.consumed(n, false));
            // Unregister first (the reader sends under this lock), then return credit for chunks
            // still queued.
            if let Ok(mut w) = s.writes.lock() {
                w.remove(&req_id);
            }
            let mut left = 0;
            while let Ok(c) = rx.try_recv() {
                left += c.data.len();
            }
            s.consumed(left, true);
            s.reply(req_id, r);
        });
    if spawned.is_err() {
        if let Ok(mut w) = sess.writes.lock() {
            w.remove(&req_id);
        }
        sess.reply(req_id, Err(perr(ErrorCode::Io, "cannot start upload")));
    }
}

fn start_read(
    sess: &Arc<Sess>,
    req_id: u32,
    id: ItemId,
    offset: u64,
    len: Option<u64>,
    expect: Option<u64>,
) {
    let cancel = Arc::new(AtomicBool::new(false));
    if let Ok(mut m) = sess.reads.lock() {
        m.insert(req_id, cancel.clone());
    }
    let s = sess.clone();
    let spawned = std::thread::Builder::new()
        .name("read".into())
        .spawn(move || {
            // Bound concurrent streams; queued reads wait here.
            while s.active_reads.fetch_add(1, Ordering::AcqRel) >= MAX_READS {
                s.active_reads.fetch_sub(1, Ordering::AcqRel);
                if s.closed() || cancel.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // A read carries the item's version (a seq): never one above the reservation.
            match s.core.deliverable() {
                Ok(()) => stream_read(&s, req_id, id, offset, len, expect, &cancel),
                Err(e) => s.reply(req_id, Err(e)),
            }
            s.active_reads.fetch_sub(1, Ordering::AcqRel);
            if let Ok(mut m) = s.reads.lock() {
                m.remove(&req_id);
            }
        });
    if spawned.is_err() {
        sess.reply(req_id, Err(perr(ErrorCode::Io, "cannot start read")));
    }
}

/// Stream a file under credit (§2(d)5): fstat before the first and after the last chunk; if the
/// file changed meanwhile, `Error(VersionMismatch)` replaces the `last = true` chunk.
fn stream_read(
    sess: &Sess,
    req_id: u32,
    id: ItemId,
    offset: u64,
    len: Option<u64>,
    expect: Option<u64>,
    cancel: &AtomicBool,
) {
    let ops = sess.ops();
    let (fd, version, st0) = match ops.open_read(id, expect) {
        Ok(x) => x,
        Err(e) => return sess.reply(req_id, Err(e)),
    };
    use std::os::fd::AsRawFd;
    let end = match len {
        Some(l) => offset.saturating_add(l).min(st0.size),
        None => st0.size,
    };
    let stop = || cancel.load(Ordering::Relaxed) || sess.closed();
    let mut pos = offset.min(end);
    // Read ahead of the credit: the bytes are ready when a grant arrives, so a slow `pread` on
    // a busy disk never sits inside the credit loop (it would inflate the loop's latency and
    // make the engine size its window for it). The final fstat/seq check below still rejects
    // a file that changed while it was being streamed.
    let mut ahead: Vec<u8> = Vec::new();
    let mut ahead_off = 0usize;
    loop {
        let remaining = end - pos;
        let (data, last) = if remaining == 0 {
            (Vec::new(), true)
        } else {
            if ahead_off == ahead.len() {
                let want = (remaining as usize).min(READ_AHEAD);
                ahead.resize(want, 0);
                ahead_off = 0;
                let got = match crate::sys::pread(fd.as_raw_fd(), &mut ahead, pos) {
                    Ok(g) => g,
                    Err(e) => return sess.reply(req_id, Err(crate::core::io_err(&e, "read"))),
                };
                ahead.truncate(got);
                if got == 0 {
                    // Shrunk underneath us.
                    return sess.send(&ServerMsg::Error {
                        req_id: Some(req_id),
                        err: perr(ErrorCode::VersionMismatch, "file changed while reading"),
                    });
                }
            }
            let avail = (ahead.len() - ahead_off).min(BULK_CHUNK);
            let n = sess.shared.credit.take_up_to(avail, &stop);
            if n == 0 {
                return; // cancelled: no further chunks, no reply
            }
            let d = ahead[ahead_off..ahead_off + n].to_vec();
            ahead_off += n;
            let last = pos + n as u64 >= end;
            (d, last)
        };
        if last {
            let changed = match crate::sys::fstat(fd.as_raw_fd()) {
                Ok(st1) => {
                    st1.size != st0.size
                        || st1.mtime_ns != st0.mtime_ns
                        || st1.ctime_ns != st0.ctime_ns
                }
                Err(_) => true,
            };
            let reseq = !changed && ops.content_seq(id).is_some_and(|c| c != version);
            if changed || reseq {
                return sess.send(&ServerMsg::Error {
                    req_id: Some(req_id),
                    err: perr(ErrorCode::VersionMismatch, "file changed while reading"),
                });
            }
        }
        let n = data.len() as u64;
        let msg = ServerMsg::ReadChunk {
            req_id,
            offset: pos,
            data,
            last,
            version,
        };
        match frame::encode(&msg, true) {
            Ok(f) => {
                // Credit was taken for the data; the frame costs its raw payload length.
                sess.shared.credit.refund(n as usize);
                sess.shared.credit.charge(credit_cost(&f));
                sess.shared.out.push_bulk(f)
            }
            Err(e) => return sess.reply(req_id, Err(perr(ErrorCode::Protocol, e.to_string()))),
        }
        pos += n;
        if last {
            return;
        }
        if stop() {
            return;
        }
    }
}

fn snapshot_walker(sess: &Arc<Sess>) {
    loop {
        if sess.closed() {
            return;
        }
        // Wait for credit *before* reading the next chunk from the index, so the chunk reflects
        // the index as late as possible.
        if !sess.shared.credit.wait_positive(&|| sess.closed()) {
            return;
        }
        match sess.core.snapshot_step(&sess.shared) {
            Ok(Some(f)) => {
                sess.shared.credit.charge(credit_cost(&f));
                sess.shared.out.push_bulk(f);
            }
            // Held back (ids not reservable right now): try again shortly.
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(seq) => {
                // On the bulk lane so it can never overtake the last chunk.
                if let Ok(f) = frame::encode(&ServerMsg::SnapshotDone { seq }, true) {
                    sess.shared.out.push_bulk(f);
                }
                return;
            }
        }
    }
}

/// Keep the Receiver type referenced for docs.
pub type ChunkRx = Receiver<Chunk>;
