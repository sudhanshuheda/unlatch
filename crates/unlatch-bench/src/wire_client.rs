//! Minimal blocking wire-protocol client for daemon-level scenarios (T15, T16, T17).
//!
//! Talks to `unlatchd stdio` directly (no engine), so daemon behaviour can be measured in
//! isolation. It grants bulk credit for every bulk frame it consumes, measured as the frame body
//! as sent (flags byte + possibly-compressed payload, no length prefix — `unlatch_proto::wire`
//! "Credit measure"; the server starts with an implicit 256 KiB grant).

use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use unlatch_proto::frame::{self, Preamble};
use unlatch_proto::wire::{ClientMsg, Request, Response, Resume, ServerMsg, WelcomeMode};
use unlatch_proto::{Entry, IndexId, ItemId, PROTO_VERSION};

/// A received server message plus its encoded frame size (bytes on the pipe).
pub struct Received {
    pub msg: ServerMsg,
    pub frame_bytes: usize,
}

impl Received {
    /// Credit cost of this frame (wire.rs "Credit measure"): the body as sent, i.e. the frame
    /// without its 4-byte length prefix — the compressed size for LZ4 frames.
    pub fn credit_bytes(&self) -> usize {
        credit_bytes(self.frame_bytes)
    }
}

/// Credit cost of a frame of `frame_bytes` bytes on the pipe (length prefix included).
pub fn credit_bytes(frame_bytes: usize) -> usize {
    frame_bytes.saturating_sub(4)
}

pub struct WireClient {
    child: Child,
    /// `None` once closed.
    w: Option<BufWriter<ChildStdin>>,
    rx: Receiver<Result<Received>>,
    next_req: u32,
    /// Total frame bytes received so far.
    pub bytes_in: u64,
    pub server: Preamble,
}

pub struct Welcome {
    pub index: IndexId,
    pub seq: u64,
    pub mode: WelcomeMode,
    pub root: Entry,
    pub entries: u64,
    pub watches: u64,
}

fn is_bulk(m: &ServerMsg) -> bool {
    matches!(
        m,
        ServerMsg::SnapshotChunk { .. }
            | ServerMsg::ReadChunk { .. }
            | ServerMsg::Response {
                resp: Response::ListingPart { .. },
                ..
            }
    )
}

impl WireClient {
    /// Spawn `argv` (e.g. `unlatchd stdio --root R --state S`) and exchange preambles.
    pub fn spawn(
        argv: &[String],
        env: &[(String, String)],
        stderr_log: Option<&Path>,
    ) -> Result<WireClient> {
        let (prog, args) = argv.split_first().ok_or_else(|| anyhow!("empty argv"))?;
        let mut cmd = Command::new(prog);
        cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        match stderr_log.and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        }) {
            Some(f) => cmd.stderr(Stdio::from(f)),
            None => cmd.stderr(Stdio::null()),
        };
        let mut child = cmd.spawn().with_context(|| format!("spawn {prog}"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let mut w = BufWriter::new(stdin);
        let mine = Preamble {
            proto_min: PROTO_VERSION as u16,
            proto_max: PROTO_VERSION as u16,
            build_id: [0; 16],
        };
        w.write_all(&mine.to_bytes())?;
        w.flush()?;
        let (tx, rx) = mpsc::sync_channel(1024);
        let (ptx, prx) = mpsc::channel();
        std::thread::Builder::new()
            .name("wire-reader".into())
            .spawn(move || {
                reader_loop(stdout, ptx, tx);
            })?;
        let server = match prx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("no preamble from {prog}: {e}");
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("no preamble from {prog} within 10 s");
            }
        };
        if frame::negotiate(&mine, &server).is_none() {
            bail!("no common protocol version with server {server:?}");
        }
        Ok(WireClient {
            child,
            w: Some(w),
            rx,
            next_req: 1,
            bytes_in: 0,
            server,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        let w = self.w.as_mut().ok_or_else(|| anyhow!("session closed"))?;
        frame::write_blocking(w, msg, true)?;
        w.flush()?;
        Ok(())
    }

    /// Next server message (granting credit for bulk frames).
    pub fn recv(&mut self, timeout: Duration) -> Result<Received> {
        let r = match self.rx.recv_timeout(timeout) {
            Ok(r) => r?,
            Err(RecvTimeoutError::Timeout) => bail!("timeout waiting for server message"),
            Err(RecvTimeoutError::Disconnected) => bail!("server closed the session"),
        };
        self.bytes_in += r.frame_bytes as u64;
        if is_bulk(&r.msg) {
            let grant = u32::try_from(r.credit_bytes()).unwrap_or(u32::MAX);
            self.send(&ClientMsg::Credit { bulk_bytes: grant })?;
        }
        if let ServerMsg::Error { req_id: None, err } = &r.msg {
            bail!("session error: {err}");
        }
        Ok(r)
    }

    /// Send Hello and wait for Welcome (messages after it stay queued).
    pub fn hello(
        &mut self,
        root: &str,
        resume: Option<Resume>,
        timeout: Duration,
    ) -> Result<Welcome> {
        let expect_index = resume.as_ref().map(|r| r.index);
        self.send(&ClientMsg::Hello {
            proto: PROTO_VERSION,
            root: root.to_string(),
            resume,
            expect_index,
            default_lazy_names: unlatch_proto::DEFAULT_LAZY_NAMES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            client_name: "bench".into(),
        })?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let r = self.recv(left)?;
            if let ServerMsg::Welcome {
                index,
                seq,
                mode,
                root,
                info,
                ..
            } = r.msg
            {
                return Ok(Welcome {
                    index,
                    seq,
                    mode,
                    root,
                    entries: info.entries,
                    watches: info.watches,
                });
            }
        }
    }

    /// Consume messages until `SnapshotDone`. Returns (snapshot bytes, done seq); snapshot
    /// entries are appended to `keep` when given.
    pub fn drain_snapshot(
        &mut self,
        timeout: Duration,
        mut keep: Option<&mut Vec<Entry>>,
    ) -> Result<(u64, u64)> {
        let deadline = Instant::now() + timeout;
        let mut bytes = 0;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("snapshot not done within {timeout:?}");
            }
            let r = self.recv(left)?;
            match r.msg {
                ServerMsg::SnapshotChunk { entries, .. } => {
                    bytes += r.frame_bytes as u64;
                    if let Some(k) = keep.as_deref_mut() {
                        k.extend(entries);
                    }
                }
                ServerMsg::SnapshotDone { seq } => return Ok((bytes, seq)),
                _ => {}
            }
        }
    }

    /// Issue `req`, returning every `Response` for it up to the final one (ListingPart until
    /// `last`). Other messages (Events…) are passed to `other`.
    pub fn call(
        &mut self,
        req: Request,
        timeout: Duration,
        other: &mut dyn FnMut(&Received),
    ) -> Result<Vec<Response>> {
        let req_id = self.next_req;
        self.next_req = self.next_req.wrapping_add(1).max(1);
        self.send(&ClientMsg::Request { req_id, req })?;
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("request {req_id} timed out");
            }
            let r = self.recv(left)?;
            match &r.msg {
                ServerMsg::Response { req_id: id, resp } if *id == req_id => {
                    let done = !matches!(resp, Response::ListingPart { last: false, .. });
                    out.push(resp.clone());
                    if done {
                        return Ok(out);
                    }
                }
                ServerMsg::Error {
                    req_id: Some(id),
                    err,
                } if *id == req_id => {
                    bail!("request failed: {err}");
                }
                _ => other(&r),
            }
        }
    }

    pub fn ping(&mut self, timeout: Duration) -> Result<u64> {
        let r = self.call(Request::Ping { nonce: 7 }, timeout, &mut |_| {})?;
        match r.last() {
            Some(Response::Pong { seq, .. }) => Ok(*seq),
            other => bail!("unexpected ping reply {other:?}"),
        }
    }

    pub fn stat(&mut self, id: ItemId, timeout: Duration) -> Result<Entry> {
        let r = self.call(Request::Stat { id }, timeout, &mut |_| {})?;
        match r.into_iter().last() {
            Some(Response::Entry(e)) => Ok(e),
            other => bail!("unexpected stat reply {other:?}"),
        }
    }

    /// `ListDir`: (dir entry, children).
    pub fn list_dir(&mut self, dir: ItemId, timeout: Duration) -> Result<(Entry, Vec<Entry>)> {
        let parts = self.call(Request::ListDir { dir }, timeout, &mut |_| {})?;
        let mut d = None;
        let mut all = Vec::new();
        for p in parts {
            if let Response::ListingPart { dir, entries, .. } = p {
                d = Some(dir);
                all.extend(entries);
            }
        }
        Ok((d.ok_or_else(|| anyhow!("empty listing"))?, all))
    }

    /// Close stdin and wait for the daemon to exit (up to `timeout`, then kill).
    pub fn close(mut self, timeout: Duration) -> Result<()> {
        if let Some(mut w) = self.w.take() {
            let _ = w.flush();
        }
        let deadline = Instant::now() + timeout;
        loop {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                bail!("daemon did not exit within {timeout:?} after stdin closed");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for WireClient {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn reader_loop(
    stdout: ChildStdout,
    preamble: mpsc::Sender<Result<Preamble>>,
    tx: mpsc::SyncSender<Result<Received>>,
) {
    let mut r = BufReader::with_capacity(256 << 10, stdout);
    match frame::read_preamble_blocking(&mut r) {
        Ok((p, _junk)) => {
            let _ = preamble.send(Ok(p));
        }
        Err(e) => {
            let _ = preamble.send(Err(anyhow!("{e}")));
            return;
        }
    }
    loop {
        match frame::read_body_blocking(&mut r) {
            Ok(Some(body)) => {
                let n = body.len() + 4;
                let msg = frame::decode_body::<ServerMsg>(&body)
                    .map(|msg| Received {
                        msg,
                        frame_bytes: n,
                    })
                    .map_err(|e| anyhow!("decode: {e}"));
                if tx.send(msg).is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(e) => {
                let _ = tx.send(Err(anyhow!("read: {e}")));
                return;
            }
        }
    }
}

/// Count inotify watches held by `pid` (sum of `inotify wd:` lines over its fdinfo).
pub fn inotify_watches(pid: u32) -> Result<u64> {
    let dir = format!("/proc/{pid}/fdinfo");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).with_context(|| format!("read {dir}"))? {
        let Ok(e) = e else { continue };
        if let Ok(s) = std::fs::read_to_string(e.path()) {
            n += s.lines().filter(|l| l.starts_with("inotify wd:")).count() as u64;
        }
    }
    Ok(n)
}

/// `VmRSS` of `pid`, bytes.
pub fn vm_rss(pid: u32) -> Result<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    for l in s.lines() {
        if let Some(v) = l.strip_prefix("VmRSS:") {
            let kb: u64 = v.trim().trim_end_matches("kB").trim().parse()?;
            return Ok(kb * 1024);
        }
    }
    bail!("no VmRSS for pid {pid}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_and_watches_of_self() {
        let pid = std::process::id();
        assert!(vm_rss(pid).unwrap() > 0);
        assert_eq!(inotify_watches(pid).unwrap(), 0);
    }

    #[test]
    fn grants_the_body_as_sent() {
        // A compressible chunk costs its compressed body, exactly what unlatchd charges
        // (`frame.len() - 4`), never the 64 KiB it decodes to.
        let f = frame::encode(
            &ServerMsg::ReadChunk {
                req_id: 1,
                offset: 0,
                data: vec![0u8; frame::BULK_CHUNK],
                last: false,
                version: 1,
            },
            true,
        )
        .unwrap();
        let r = Received {
            msg: frame::decode_body(&f[4..]).unwrap(),
            frame_bytes: f.len(),
        };
        assert_eq!(r.credit_bytes(), f.len() - 4);
        assert!(r.credit_bytes() < frame::BULK_CHUNK / 16);
        assert_eq!(credit_bytes(3), 0);
    }

    #[test]
    fn spawn_non_unlatch_process_fails_cleanly() {
        let r = WireClient::spawn(&["true".to_string()], &[], None);
        assert!(r.is_err());
    }
}
