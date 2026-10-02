//! Minimal blocking protocol client for unlatchd integration tests: spawns `unlatchd stdio` (or
//! `connect`), performs the handshake, keeps a replica (LWW by `Entry.seq`), and offers helpers
//! for requests, uploads and streamed reads.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use unlatch_proto::frame::{self, Preamble};
use unlatch_proto::wire::{
    Change, ClientMsg, Request, Response, Resume, ServerInfo, ServerMsg, WelcomeMode,
};
use unlatch_proto::{Entry, IndexId, ItemId, Kind, OpId, ProtoError, Version, PROTO_VERSION};

pub const BIN: &str = env!("CARGO_BIN_EXE_unlatchd");
pub const T: Duration = Duration::from_secs(20);

pub fn tmp() -> tempfile::TempDir {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&base).unwrap();
    tempfile::tempdir_in(base).unwrap()
}

#[derive(Default, Clone)]
pub struct Opts {
    pub resume: Option<Resume>,
    pub env: Vec<(String, String)>,
    pub lazy: Option<Vec<String>>,
    pub client_name: Option<String>,
    /// Use `unlatchd connect` instead of `stdio`.
    pub connect: bool,
    /// Run this copy of the unlatchd binary instead of the built one.
    pub bin: Option<PathBuf>,
    /// Environment variables removed from the daemon's environment.
    pub env_remove: Vec<String>,
}

pub struct Welcome {
    pub index: IndexId,
    pub seq: u64,
    pub mode: WelcomeMode,
    pub root: Entry,
    pub info: ServerInfo,
    pub lazy_names: Vec<String>,
    pub elapsed: Duration,
}

#[derive(Default)]
pub struct Stats {
    pub snapshot_bytes: AtomicU64,
    pub read_bytes: AtomicU64,
    pub total_bytes: AtomicU64,
}

pub struct Client {
    pub child: Child,
    w: Option<BufWriter<ChildStdin>>,
    rx: Receiver<(ServerMsg, usize)>,
    pub stats: Arc<Stats>,
    next: u32,
    pub replica: HashMap<ItemId, Entry>,
    pub tombs: HashMap<ItemId, u64>,
    pub events: Vec<Change>,
    pub event_seqs: Vec<u64>,
    pub responses: HashMap<u32, VecDeque<ServerMsg>>,
    pub credits: u64,
    pub snapshot_done: Option<u64>,
    pub welcome: Option<Welcome>,
    pub complete_dirs: Vec<ItemId>,
    pub session_error: Option<ProtoError>,
    /// Grant credit for snapshot chunks and listings automatically (like the engine).
    pub auto_credit: bool,
    /// Raw (credit) size of the last ReadChunk handed out by `next_for`.
    pub raw_of: HashMap<u32, VecDeque<usize>>,
    /// Bulk bytes received / granted (credit accounting check).
    pub bulk_received: u64,
    pub bulk_granted: u64,
}

/// `unlatchd`, or `$UNLATCHD_TEST_WRAP <args…> unlatchd` (e.g. `strace -f --seccomp-bpf -c -o
/// /tmp/s -e trace=statx` to count syscalls in measurements; whitespace-separated).
fn wrapped_bin() -> Command {
    match std::env::var("UNLATCHD_TEST_WRAP") {
        Ok(w) if !w.trim().is_empty() => {
            let mut it = w.split_whitespace();
            let mut c = Command::new(it.next().unwrap());
            c.args(it).arg(BIN);
            c
        }
        _ => Command::new(BIN),
    }
}

/// `cmd.spawn()`, retried while the binary is "busy" (ETXTBSY): a test that copies unlatchd
/// somewhere (`Opts::bin`) holds the copy open for writing, and a child another test thread
/// forks in that instant inherits that descriptor until its own exec, so exec'ing the copy
/// can fail for a moment.
pub fn spawn_retrying(cmd: &mut Command) -> std::io::Result<Child> {
    let t0 = Instant::now();
    loop {
        match cmd.spawn() {
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && t0.elapsed() < Duration::from_secs(5) =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            r => return r,
        }
    }
}

impl Client {
    pub fn spawn(root: &Path, state: &Path, o: Opts) -> Client {
        let mut cmd = match &o.bin {
            Some(b) => Command::new(b),
            None => wrapped_bin(),
        };
        for k in &o.env_remove {
            cmd.env_remove(k);
        }
        cmd.arg(if o.connect { "connect" } else { "stdio" })
            .arg("--root")
            .arg(root)
            .arg("--state")
            .arg(state);
        for (k, v) in &o.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let t0 = Instant::now();
        let mut child = spawn_retrying(&mut cmd).expect("spawn unlatchd");
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let stats = Arc::new(Stats::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let st2 = stats.clone();
        std::thread::spawn(move || {
            let Ok((_p, _junk)) = frame::read_preamble_blocking(&mut stdout) else {
                return;
            };
            loop {
                let body = match frame::read_body_blocking(&mut stdout) {
                    Ok(Some(b)) => b,
                    _ => return,
                };
                let len = body.len() + 4;
                st2.total_bytes.fetch_add(len as u64, Ordering::Relaxed);
                // unlatchd's credit measure: the frame body as it travelled.
                let raw = body.len();
                let msg: ServerMsg = match frame::decode_body(&body) {
                    Ok(m) => m,
                    Err(e) => panic!("decode: {e}"),
                };
                match &msg {
                    ServerMsg::SnapshotChunk { .. } | ServerMsg::SnapshotDone { .. } => {
                        st2.snapshot_bytes.fetch_add(len as u64, Ordering::Relaxed);
                    }
                    ServerMsg::ReadChunk { data, .. } => {
                        st2.read_bytes
                            .fetch_add(data.len() as u64, Ordering::Relaxed);
                    }
                    _ => {}
                }
                if tx.send((msg, raw)).is_err() {
                    return;
                }
            }
        });
        let mut c = Client {
            child,
            w: Some(BufWriter::new(stdin)),
            rx,
            stats,
            next: 1,
            replica: HashMap::new(),
            tombs: HashMap::new(),
            events: Vec::new(),
            event_seqs: Vec::new(),
            responses: HashMap::new(),
            credits: 0,
            snapshot_done: None,
            welcome: None,
            complete_dirs: Vec::new(),
            session_error: None,
            auto_credit: true,
            raw_of: HashMap::new(),
            bulk_received: 0,
            bulk_granted: 0,
        };
        let p = Preamble {
            proto_min: 1,
            proto_max: PROTO_VERSION as u16,
            build_id: [0; 16],
        };
        let w = c.w.as_mut().unwrap();
        w.write_all(&p.to_bytes()).unwrap();
        let hello = ClientMsg::Hello {
            proto: PROTO_VERSION,
            root: root.to_string_lossy().into_owned(),
            resume: o.resume.clone(),
            expect_index: o.resume.as_ref().map(|r| r.index),
            default_lazy_names: o.lazy.clone().unwrap_or_else(|| {
                unlatch_proto::DEFAULT_LAZY_NAMES
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            }),
            client_name: o.client_name.clone().unwrap_or_else(|| "testmac".into()),
        };
        c.send(&hello);
        // Welcome
        loop {
            let (m, _) = c.rx.recv_timeout(T).expect("no Welcome");
            match m {
                ServerMsg::Welcome {
                    index,
                    seq,
                    mode,
                    root,
                    info,
                    lazy_names,
                    ..
                } => {
                    c.replica.insert(root.id, root.clone());
                    c.welcome = Some(Welcome {
                        index,
                        seq,
                        mode,
                        root,
                        info,
                        lazy_names,
                        elapsed: t0.elapsed(),
                    });
                    break;
                }
                ServerMsg::Error { err, .. } => panic!("session error before Welcome: {err:?}"),
                other => c.absorb(other, 0),
            }
        }
        c
    }

    pub fn try_spawn_err(root: &Path, state: &Path, o: Opts) -> ProtoError {
        let mut cmd = Command::new(BIN);
        cmd.arg("stdio")
            .arg("--root")
            .arg(root)
            .arg("--state")
            .arg(state);
        for (k, v) in &o.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let p = Preamble {
            proto_min: 1,
            proto_max: PROTO_VERSION as u16,
            build_id: [0; 16],
        };
        stdin.write_all(&p.to_bytes()).unwrap();
        let hello = ClientMsg::Hello {
            proto: PROTO_VERSION,
            root: root.to_string_lossy().into_owned(),
            resume: None,
            expect_index: None,
            default_lazy_names: vec![],
            client_name: "t".into(),
        };
        stdin
            .write_all(&frame::encode(&hello, false).unwrap())
            .unwrap();
        stdin.flush().unwrap();
        frame::read_preamble_blocking(&mut stdout).unwrap();
        let m: ServerMsg = frame::read_blocking(&mut stdout).unwrap().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        match m {
            ServerMsg::Error { err, .. } => err,
            other => panic!("expected error, got {other:?}"),
        }
    }

    pub fn welcome(&self) -> &Welcome {
        self.welcome.as_ref().unwrap()
    }

    pub fn send(&mut self, m: &ClientMsg) {
        let f = frame::encode(m, true).unwrap();
        let w = self.w.as_mut().unwrap();
        w.write_all(&f).unwrap();
        w.flush().unwrap();
    }

    pub fn try_send(&mut self, m: &ClientMsg) -> std::io::Result<()> {
        let f = frame::encode(m, true).unwrap();
        let w = self.w.as_mut().unwrap();
        w.write_all(&f)?;
        w.flush()
    }

    fn apply_entry(&mut self, e: Entry) {
        if let Some(&ts) = self.tombs.get(&e.id) {
            if ts >= e.seq {
                return;
            }
        }
        match self.replica.get(&e.id) {
            Some(old) if old.seq > e.seq => {}
            _ => {
                self.replica.insert(e.id, e);
            }
        }
    }

    fn remove_subtree(&mut self, id: ItemId, seq: u64) {
        self.tombs.insert(id, seq);
        let mut stack = vec![id];
        while let Some(x) = stack.pop() {
            if let Some(e) = self.replica.get(&x) {
                if e.seq > seq && x != id {
                    continue;
                }
            }
            self.replica.remove(&x);
            let kids: Vec<ItemId> = self
                .replica
                .values()
                .filter(|e| e.parent == x && e.id != x)
                .map(|e| e.id)
                .collect();
            stack.extend(kids);
        }
    }

    pub fn grant(&mut self, n: usize) {
        if n > 0 {
            self.bulk_granted += n as u64;
            self.send(&ClientMsg::Credit {
                bulk_bytes: n as u32,
            });
        }
    }

    fn absorb(&mut self, m: ServerMsg, raw: usize) {
        let bulk = matches!(
            m,
            ServerMsg::SnapshotChunk { .. }
                | ServerMsg::ReadChunk { .. }
                | ServerMsg::Response {
                    resp: Response::ListingPart { .. },
                    ..
                }
        );
        if bulk {
            self.bulk_received += raw as u64;
            let auto = !matches!(m, ServerMsg::ReadChunk { .. });
            if auto && self.auto_credit && self.w.is_some() {
                self.grant(raw);
            }
        }
        if let ServerMsg::ReadChunk { req_id, .. } = &m {
            self.raw_of.entry(*req_id).or_default().push_back(raw);
        }
        match m {
            ServerMsg::Events { seq, changes, .. } => {
                self.event_seqs.push(seq);
                for c in changes {
                    match &c {
                        Change::Upsert(e) => self.apply_entry(e.clone()),
                        Change::Remove { id, seq } => self.remove_subtree(*id, *seq),
                    }
                    self.events.push(c);
                }
            }
            ServerMsg::SnapshotChunk {
                entries,
                complete_dirs,
            } => {
                for e in entries {
                    self.apply_entry(e);
                }
                self.complete_dirs.extend(complete_dirs);
            }
            ServerMsg::SnapshotDone { seq } => self.snapshot_done = Some(seq),
            ServerMsg::Credit { bulk_bytes } => self.credits += bulk_bytes as u64,
            ServerMsg::Response { req_id, resp } => {
                match &resp {
                    Response::ListingPart { dir, entries, .. } => {
                        self.apply_entry(dir.clone());
                        for e in entries {
                            self.apply_entry(e.clone());
                        }
                    }
                    Response::Entry(e) => self.apply_entry(e.clone()),
                    Response::Written {
                        entry,
                        conflict_copy,
                    } => {
                        self.apply_entry(entry.clone());
                        if let Some(c) = conflict_copy {
                            self.apply_entry(c.clone());
                        }
                    }
                    Response::Renamed { entry, .. } => self.apply_entry(entry.clone()),
                    _ => {}
                }
                self.responses
                    .entry(req_id)
                    .or_default()
                    .push_back(ServerMsg::Response { req_id, resp });
            }
            ServerMsg::ReadChunk {
                req_id,
                offset,
                data,
                last,
                version,
            } => {
                self.responses
                    .entry(req_id)
                    .or_default()
                    .push_back(ServerMsg::ReadChunk {
                        req_id,
                        offset,
                        data,
                        last,
                        version,
                    });
            }
            ServerMsg::Error {
                req_id: Some(r),
                err,
            } => {
                self.responses
                    .entry(r)
                    .or_default()
                    .push_back(ServerMsg::Error {
                        req_id: Some(r),
                        err,
                    });
            }
            ServerMsg::Error { req_id: None, err } => self.session_error = Some(err),
            ServerMsg::Welcome { .. } => panic!("second Welcome"),
        }
    }

    /// Pump one message (or time out).
    pub fn pump(&mut self, timeout: Duration) -> bool {
        match self.rx.recv_timeout(timeout) {
            Ok((m, raw)) => {
                self.absorb(m, raw);
                true
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => {
                std::thread::sleep(timeout.min(Duration::from_millis(10)));
                false
            }
        }
    }

    pub fn pump_until(&mut self, timeout: Duration, mut done: impl FnMut(&Client) -> bool) -> bool {
        let t0 = Instant::now();
        while !done(self) {
            if t0.elapsed() > timeout {
                return false;
            }
            self.pump(Duration::from_millis(20));
        }
        true
    }

    pub fn wait_snapshot(&mut self) {
        assert!(
            self.pump_until(T, |c| c.snapshot_done.is_some()),
            "no SnapshotDone"
        );
    }

    pub fn request(&mut self, req: Request) -> u32 {
        let id = self.next;
        self.next += 1;
        self.send(&ClientMsg::Request { req_id: id, req });
        id
    }

    /// Next message for `req_id`.
    pub fn next_for(&mut self, req_id: u32, timeout: Duration) -> Option<ServerMsg> {
        let t0 = Instant::now();
        loop {
            if let Some(q) = self.responses.get_mut(&req_id) {
                if let Some(m) = q.pop_front() {
                    return Some(m);
                }
            }
            if t0.elapsed() > timeout {
                return None;
            }
            self.pump(Duration::from_millis(20));
        }
    }

    pub fn response(&mut self, req_id: u32) -> Result<Response, ProtoError> {
        match self
            .next_for(req_id, T)
            .unwrap_or_else(|| panic!("no response for {req_id}"))
        {
            ServerMsg::Response { resp, .. } => Ok(resp),
            ServerMsg::Error { err, .. } => Err(err),
            other => panic!("unexpected {other:?}"),
        }
    }

    pub fn call(&mut self, req: Request) -> Result<Response, ProtoError> {
        let id = self.request(req);
        self.response(id)
    }

    /// Ping barrier: every change before this is in the replica afterwards.
    pub fn ping(&mut self) -> u64 {
        match self.call(Request::Ping { nonce: 7 }) {
            Ok(Response::Pong { nonce: 7, seq }) => seq,
            other => panic!("bad pong {other:?}"),
        }
    }

    pub fn list(&mut self, dir: ItemId) -> Result<(Entry, Vec<Entry>), ProtoError> {
        let id = self.request(Request::ListDir { dir });
        let mut all = Vec::new();
        loop {
            match self.next_for(id, T).expect("listing") {
                ServerMsg::Response {
                    resp: Response::ListingPart { dir, entries, last },
                    ..
                } => {
                    all.extend(entries);
                    if last {
                        return Ok((dir, all));
                    }
                }
                ServerMsg::Error { err, .. } => return Err(err),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    pub fn stat(&mut self, id: ItemId) -> Result<Entry, ProtoError> {
        match self.call(Request::Stat { id })? {
            Response::Entry(e) => Ok(e),
            other => panic!("unexpected {other:?}"),
        }
    }

    pub fn root_id(&self) -> ItemId {
        ItemId::ROOT
    }

    /// Replica lookup by relative path ("a/b/c").
    pub fn find(&self, path: &str) -> Option<Entry> {
        let mut cur = ItemId::ROOT;
        if path.is_empty() {
            return self.replica.get(&cur).cloned();
        }
        for comp in path.split('/') {
            let e = self
                .replica
                .values()
                .find(|e| e.parent == cur && e.name == comp && e.id != cur)?;
            cur = e.id;
        }
        self.replica.get(&cur).cloned()
    }

    pub fn children(&self, dir: ItemId) -> Vec<Entry> {
        let mut v: Vec<Entry> = self
            .replica
            .values()
            .filter(|e| e.parent == dir && e.id != dir)
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Upload `data` (create when `target` is None).
    #[allow(clippy::too_many_arguments)]
    pub fn write(
        &mut self,
        op: OpId,
        parent: ItemId,
        name: &str,
        target: Option<ItemId>,
        base: Option<u64>,
        data: &[u8],
        may_exist: bool,
    ) -> Result<Response, ProtoError> {
        let id = self.start_write(op, parent, name, target, base, data, may_exist, None, None);
        self.response(id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_write(
        &mut self,
        op: OpId,
        parent: ItemId,
        name: &str,
        target: Option<ItemId>,
        base: Option<u64>,
        data: &[u8],
        may_exist: bool,
        exec: Option<bool>,
        move_to: Option<(ItemId, String)>,
    ) -> u32 {
        let id = self.request(Request::Write {
            op,
            parent,
            name: name.to_string(),
            target,
            base,
            size: data.len() as u64,
            content_hash: *blake3::hash(data).as_bytes(),
            mtime_ns: None,
            exec,
            move_to,
            may_exist,
        });
        if data.is_empty() {
            self.send(&ClientMsg::WriteChunk {
                req_id: id,
                data: vec![],
                last: true,
            });
        }
        let n = data.chunks(64 * 1024).count();
        for (i, c) in data.chunks(64 * 1024).enumerate() {
            self.send(&ClientMsg::WriteChunk {
                req_id: id,
                data: c.to_vec(),
                last: i + 1 == n,
            });
        }
        id
    }

    /// Stream a file, granting credit as chunks arrive. Returns (bytes, version).
    pub fn read(&mut self, id: ItemId, expect: Option<u64>) -> Result<(Vec<u8>, u64), ProtoError> {
        let rid = self.request(Request::Read {
            id,
            offset: 0,
            len: None,
            expect,
        });
        let mut out = Vec::new();
        loop {
            match self.next_for(rid, T).expect("read chunk") {
                ServerMsg::ReadChunk {
                    data,
                    last,
                    version,
                    offset,
                    ..
                } => {
                    assert_eq!(offset as usize, out.len());
                    let n = data.len() as u32;
                    out.extend_from_slice(&data);
                    if n > 0 {
                        self.send(&ClientMsg::Credit { bulk_bytes: n });
                    }
                    if last {
                        return Ok((out, version));
                    }
                }
                ServerMsg::Error { err, .. } => return Err(err),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// Credit size of the oldest not-yet-granted ReadChunk of `req_id`.
    pub fn take_raw(&mut self, req_id: u32) -> usize {
        self.raw_of
            .get_mut(&req_id)
            .and_then(|q| q.pop_front())
            .unwrap_or(0)
    }

    /// Close stdin and wait for exit (clean shutdown).
    pub fn close(mut self) -> std::process::ExitStatus {
        self.w.take();
        let t0 = Instant::now();
        loop {
            if let Ok(Some(s)) = self.child.try_wait() {
                return s;
            }
            if t0.elapsed() > T {
                let _ = self.child.kill();
                return self.child.wait().unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> Option<std::process::ExitStatus> {
        let t0 = Instant::now();
        loop {
            if let Ok(Some(s)) = self.child.try_wait() {
                return Some(s);
            }
            if t0.elapsed() > timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn op(n: u64) -> OpId {
    let mut o = [0u8; 16];
    o[..8].copy_from_slice(&n.to_le_bytes());
    o[8..].copy_from_slice(&0xfeed_u64.to_le_bytes());
    o
}

pub fn v(e: &Entry) -> Version {
    e.version
}

pub fn is_dir(e: &Entry) -> bool {
    e.kind == Kind::Dir
}

/// Build a tree: `n_dirs` dirs × `per_dir` files.
pub fn make_tree(root: &Path, n_dirs: usize, per_dir: usize) {
    for d in 0..n_dirs {
        let dp = root.join(format!("d{d:04}"));
        std::fs::create_dir_all(&dp).unwrap();
        for f in 0..per_dir {
            std::fs::write(dp.join(format!("f{f:04}.txt")), format!("{d}/{f}")).unwrap();
        }
    }
}
