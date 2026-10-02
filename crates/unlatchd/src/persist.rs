//! Durable state under `<state>/` (D17, review §2(d)9):
//!
//! * `index.bin` — checkpoint of the whole index (atomic write + fsync, postcard, checksummed).
//! * `journal.<gen>` — append-only log of index changes and op results since that checkpoint.
//!   Mutations fsync it together with their op record before replying (§2(d)1); watcher batches
//!   append without fsync (a lost tail is recovered by the startup verify walk).
//! * `alloc.bin` — hi/lo high-water marks for ids and seqs, fsync'd *before* any id/seq in the
//!   reserved block is published (D4), so a crash never re-issues one.

use crate::index::{ColdChild, Index, PNode, Tomb};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

/// Bump on any change to the persisted encoding (forces a new index id, D4).
/// 2: header + one record per node (streamed; no whole-index transient copy, T12).
pub const FORMAT: u32 = 2;
const MAGIC: &[u8; 8] = b"UNLATIDX";
pub const ID_BLOCK: u64 = 65_536;
pub const SEQ_BLOCK: u64 = 1 << 20;
pub const OP_TTL_SECS: u64 = 7 * 24 * 3600;
pub const OP_MAX: usize = 100_000;
const JOURNAL_CHECKPOINT_BYTES: u64 = 32 << 20;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OpRec {
    pub op: [u8; 16],
    pub resp: Vec<u8>,
    pub time: u64,
}

/// Where this index was built; any difference ⇒ new index id (D4).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Origin {
    pub machine_id: String,
    pub root_fsid: u64,
    pub root_dev: u64,
    pub root_ino: u64,
    pub root_path: String,
}

#[derive(Serialize, Deserialize)]
pub struct Checkpoint {
    pub format: u32,
    pub index_id: u128,
    pub origin: Origin,
    pub seq: u64,
    pub next_id: u64,
    pub gc_seq: u64,
    pub lazy_names: Vec<String>,
    pub tombs: Vec<Tomb>,
    pub ops: Vec<OpRec>,
    pub cold: Vec<(u64, Vec<ColdChild>)>,
    pub suspect: Vec<(u64, u64)>,
    pub seq_times: Vec<(u64, i64)>,
    pub journal_gen: u64,
    /// Wall clock of the last persisted observation (racy-git rule at startup, D3).
    pub observed_ns: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum JRec {
    Node(PNode),
    Remove {
        id: u64,
        seq: u64,
        tomb: bool,
        time: u64,
    },
    Seq(u64),
    Op(OpRec),
    Observed(i64),
    /// A content change of item `id` was held back by the hot-file throttle: its seq may sit
    /// below seqs already published (appended variants: older journals still decode).
    Deferred(u64),
    /// The held-back change of `id` was published.
    Published(u64),
}

/// Idempotency table: op id → encoded Response (§2(d)1).
#[derive(Default)]
pub struct OpTable {
    map: HashMap<[u8; 16], OpRec>,
    order: VecDeque<[u8; 16]>,
}

impl OpTable {
    pub fn get(&self, op: &[u8; 16]) -> Option<&OpRec> {
        self.map
            .get(op)
            .filter(|r| crate::sys::now_secs().saturating_sub(r.time) <= OP_TTL_SECS)
    }
    pub fn insert(&mut self, rec: OpRec) {
        if self.map.insert(rec.op, rec.clone()).is_none() {
            self.order.push_back(rec.op);
        }
        while self.order.len() > OP_MAX {
            if let Some(o) = self.order.pop_front() {
                self.map.remove(&o);
            }
        }
    }
    pub fn expire(&mut self) {
        let now = crate::sys::now_secs();
        while let Some(o) = self.order.front() {
            match self.map.get(o) {
                Some(r) if now.saturating_sub(r.time) <= OP_TTL_SECS => break,
                _ => {
                    let o = *o;
                    self.order.pop_front();
                    self.map.remove(&o);
                }
            }
        }
    }
    pub fn all(&self) -> Vec<OpRec> {
        self.order
            .iter()
            .filter_map(|o| self.map.get(o).cloned())
            .collect()
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Everything loaded from `<state>/`.
pub struct Loaded {
    pub index_id: u128,
    pub origin: Origin,
    pub index: Index,
    pub ops: OpTable,
    pub suspect: Vec<(u64, u64)>,
    pub observed_ns: i64,
    /// True when the journal had to be replayed (the previous process did not shut down cleanly).
    pub replayed: bool,
}

pub struct Store {
    dir: PathBuf,
    gen: u64,
    journal: Option<File>,
    journal_bytes: u64,
    pub id_hwm: u64,
    pub seq_hwm: u64,
    /// Journal records written since the last fsync (fsync is skipped when nothing is pending).
    dirty: bool,
    /// Test hook: every append fails with ENOSPC.
    #[cfg(test)]
    pub fail_appends: bool,
}

/// Corruption check (not security): a word-wise multiply/rotate hash, fast even in debug
/// builds (startup reads the whole checkpoint before Welcome, T16).
fn checksum(b: &[u8]) -> [u8; 32] {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h1: u64 = 0x243F_6A88_85A3_08D3 ^ b.len() as u64;
    let mut h2: u64 = 0x1319_8A2E_0370_7344;
    let full = b.len() / 16 * 16;
    let mut i = 0;
    while i < full {
        let c = &b[i..i + 16];
        let a = u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
        let d = u64::from_le_bytes([c[8], c[9], c[10], c[11], c[12], c[13], c[14], c[15]]);
        h1 = (h1 ^ a).wrapping_mul(K).rotate_left(31);
        h2 = (h2 ^ d).wrapping_mul(K).rotate_left(27);
        i += 16;
    }
    for (i, &x) in b[full..].iter().enumerate() {
        h1 = (h1 ^ ((x as u64) << (8 * (i % 8))))
            .wrapping_mul(K)
            .rotate_left(31);
    }
    let mut out = [0u8; 32];
    let f1 = (h1 ^ (h2 >> 29)).wrapping_mul(K);
    let f2 = (h2 ^ (h1 >> 31)).wrapping_mul(K);
    out[..8].copy_from_slice(&f1.to_le_bytes());
    out[8..16].copy_from_slice(&f2.to_le_bytes());
    out[16..24].copy_from_slice(&(f1 ^ f2).to_le_bytes());
    out[24..].copy_from_slice(&f1.wrapping_add(f2).to_le_bytes());
    out
}

pub fn fsync_dir(dir: &Path) -> io::Result<()> {
    let f = File::open(dir)?;
    f.sync_all()
}

/// Atomic file replace: write tmp, fsync, rename, fsync dir.
pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let tmp = path.with_extension(format!("tmp{}", crate::sys::getpid()));
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    let r = f
        .write_all(data)
        .and_then(|_| f.sync_all())
        .and_then(|_| std::fs::rename(&tmp, path));
    drop(f);
    if let Err(e) = r {
        // Never leave a partial file behind (on a full disk it would hold the space every
        // retry needs).
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    fsync_dir(dir)
}

impl Store {
    pub fn open(dir: &Path) -> io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        let mut s = Store {
            dir: dir.to_path_buf(),
            gen: 0,
            journal: None,
            journal_bytes: 0,
            id_hwm: 0,
            seq_hwm: 0,
            dirty: false,
            #[cfg(test)]
            fail_appends: false,
        };
        if let Ok(b) = std::fs::read(dir.join("alloc.bin")) {
            if b.len() == 24 && checksum(&b[..16])[..8] == b[16..24] {
                s.id_hwm = u64::from_le_bytes(b[0..8].try_into().unwrap_or_default());
                s.seq_hwm = u64::from_le_bytes(b[8..16].try_into().unwrap_or_default());
            }
        }
        Ok(s)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn journal_path(&self, gen: u64) -> PathBuf {
        self.dir.join(format!("journal.{gen}"))
    }

    fn write_alloc(&mut self, id_hwm: u64, seq_hwm: u64) -> io::Result<()> {
        let mut b = Vec::with_capacity(24);
        b.extend_from_slice(&id_hwm.to_le_bytes());
        b.extend_from_slice(&seq_hwm.to_le_bytes());
        let c = checksum(&b);
        b.extend_from_slice(&c[..8]);
        atomic_write(&self.dir.join("alloc.bin"), &b)?;
        self.id_hwm = id_hwm;
        self.seq_hwm = seq_hwm;
        Ok(())
    }

    /// Reserve id/seq blocks (fsync'd) so that everything up to `next_id`/`seq` may be published.
    pub fn reserve(&mut self, next_id: u64, seq: u64) -> io::Result<()> {
        if next_id <= self.id_hwm && seq <= self.seq_hwm {
            return Ok(());
        }
        let id_hwm = if next_id > self.id_hwm {
            next_id + ID_BLOCK
        } else {
            self.id_hwm
        };
        let seq_hwm = if seq > self.seq_hwm {
            seq + SEQ_BLOCK
        } else {
            self.seq_hwm
        };
        self.write_alloc(id_hwm, seq_hwm)
    }

    /// Load checkpoint + journal. `None` when there is no usable index (fresh start).
    pub fn load(&mut self) -> io::Result<Option<Loaded>> {
        let raw = match std::fs::read(self.dir.join("index.bin")) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some((cp, nodes)) = decode_checkpoint(&raw) else {
            crate::log!("index.bin unreadable or old format; starting a new index");
            return Ok(None);
        };
        let mut index = Index::new(cp.lazy_names.clone());
        // One node at a time: no transient copy of the whole index (T12).
        for rec in nodes {
            let Ok(p) = postcard::from_bytes::<PNode>(rec) else {
                crate::log!("index.bin: undecodable node record; starting a new index");
                return Ok(None);
            };
            if !index.apply_pnode(&p) {
                crate::log!("index.bin: orphan node {} dropped", p.id);
            }
        }
        drop(raw);
        index.seq = index.seq.max(cp.seq);
        index.next_id = index.next_id.max(cp.next_id);
        index.gc_seq = cp.gc_seq;
        index.tombs = cp.tombs;
        index.seq_times = cp.seq_times;
        for (d, c) in cp.cold {
            index.cold.insert(d, c);
        }
        let mut ops = OpTable::default();
        for r in cp.ops {
            ops.insert(r);
        }
        let mut observed_ns = cp.observed_ns;
        self.gen = cp.journal_gen;
        // Replay the journal.
        let mut replayed = false;
        let mut deferred: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let jp = self.journal_path(self.gen);
        let mut valid_len = 0u64;
        if let Ok(mut f) = File::open(&jp) {
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            let mut off = 0usize;
            while off + 8 <= buf.len() {
                let len = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
                    as usize;
                let sum = &buf[off + 4..off + 8];
                if off + 8 + len > buf.len() {
                    break;
                }
                let payload = &buf[off + 8..off + 8 + len];
                if checksum(payload)[..4] != *sum {
                    break;
                }
                let Ok(rec) = postcard::from_bytes::<JRec>(payload) else {
                    break;
                };
                replayed = true;
                match rec {
                    JRec::Node(p) => {
                        index.apply_pnode(&p);
                    }
                    JRec::Remove {
                        id,
                        seq,
                        tomb,
                        time,
                    } => index.apply_remove(id, seq, tomb, time),
                    JRec::Seq(s) => index.seq = index.seq.max(s),
                    JRec::Op(r) => ops.insert(r),
                    JRec::Observed(t) => observed_ns = observed_ns.max(t),
                    JRec::Deferred(id) => {
                        deferred.insert(id);
                    }
                    JRec::Published(id) => {
                        deferred.remove(&id);
                    }
                }
                off += 8 + len;
                valid_len = off as u64;
            }
            if (valid_len as usize) < buf.len() {
                crate::log!(
                    "journal: torn tail of {} bytes dropped",
                    buf.len() - valid_len as usize
                );
            }
        }
        if replayed {
            // Per-record replay can leave an inode without a holder (see `rebuild_idents`).
            index.rebuild_idents();
            // A journal can repeat removals (a release after a hold re-journals its diff):
            // tombstones stay in seq order (Resume and the GC cut rely on it).
            index.tombs.sort_by_key(|t| t.seq);
        }
        // Open the journal for appending at the last valid record.
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&jp)?;
        f.set_len(valid_len)?;
        self.journal = Some(f);
        self.journal_bytes = valid_len;
        let mut suspect = cp.suspect;
        // Seqs above the durable ones but inside the reserved block may have been published and
        // then lost (power loss before a journal fsync): a client resuming from there must
        // snapshot.
        if self.seq_hwm > index.seq {
            suspect.push((index.seq, self.seq_hwm));
            if suspect.len() > 32 {
                suspect.remove(0);
            }
            index.seq = self.seq_hwm;
        }
        index.next_id = index.next_id.max(self.id_hwm);
        // Changes held back by the hot-file throttle and never published (the process died
        // first): clients may already have resumed past their seq, so give them a fresh one —
        // a Resume then replays them (above the reserved block: never a published seq).
        let mut ids: Vec<u64> = deferred.into_iter().collect();
        ids.sort_unstable();
        for id in ids {
            if let Some(s) = index.slot_of(id) {
                let seq = index.bump();
                index.node_mut(s).seq = seq;
            }
        }
        Ok(Some(Loaded {
            index_id: cp.index_id,
            origin: cp.origin,
            index,
            ops,
            suspect,
            observed_ns,
            replayed,
        }))
    }

    /// Append records (one write). `durable`: fsync before returning (mutations, §2(d)1).
    pub fn append(&mut self, recs: &[JRec], durable: bool) -> io::Result<()> {
        #[cfg(test)]
        if self.fail_appends {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        if !recs.is_empty() {
            let mut buf = Vec::new();
            for r in recs {
                let payload =
                    postcard::to_stdvec(r).map_err(|e| io::Error::other(e.to_string()))?;
                buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                buf.extend_from_slice(&checksum(&payload)[..4]);
                buf.extend_from_slice(&payload);
            }
            if self.journal.is_none() {
                let f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.journal_path(self.gen))?;
                self.journal = Some(f);
            }
            if let Some(f) = self.journal.as_mut() {
                use std::io::Seek;
                f.seek(io::SeekFrom::Start(self.journal_bytes))?;
                f.write_all(&buf)?;
            }
            self.journal_bytes += buf.len() as u64;
            self.dirty = true;
        }
        if durable && self.dirty {
            if let Some(f) = self.journal.as_ref() {
                // Append-only log: its reader needs the bytes and the size, never the mtime.
                crate::sys::fdatasync(f.as_raw_fd())?;
            }
            self.dirty = false;
        }
        Ok(())
    }

    pub fn wants_checkpoint(&self) -> bool {
        self.journal_bytes > JOURNAL_CHECKPOINT_BYTES
    }

    pub fn journal_bytes(&self) -> u64 {
        self.journal_bytes
    }

    /// Write a full checkpoint and start a new journal generation.
    #[allow(clippy::too_many_arguments)]
    pub fn checkpoint(
        &mut self,
        index_id: u128,
        origin: &Origin,
        index: &Index,
        ops: &OpTable,
        suspect: &[(u64, u64)],
        observed_ns: i64,
        clean: bool,
    ) -> io::Result<()> {
        let new_gen = self.gen + 1;
        let cp = Checkpoint {
            format: FORMAT,
            index_id,
            origin: origin.clone(),
            seq: index.seq,
            next_id: index.next_id,
            gc_seq: index.gc_seq,
            lazy_names: index.lazy_names.clone(),
            tombs: index.tombs.clone(),
            ops: ops.all(),
            cold: index.cold.iter().map(|(k, v)| (*k, v.clone())).collect(),
            suspect: suspect.to_vec(),
            seq_times: index.seq_times.clone(),
            journal_gen: new_gen,
            observed_ns,
        };
        let enc = |e: postcard::Error| io::Error::other(e.to_string());
        let header = postcard::to_stdvec(&cp).map_err(enc)?;
        let order = index.bfs();
        let mut out = Vec::with_capacity(header.len() + order.len() * 64 + 64);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(&(order.len() as u64).to_le_bytes());
        for s in order {
            // Borrowed record, encoded straight into the output (no per-node allocation).
            let at = out.len();
            out.extend_from_slice(&[0u8; 4]);
            out = postcard::to_extend(&index.pnode_ref(s), out).map_err(enc)?;
            let len = (out.len() - at - 4) as u32;
            out[at..at + 4].copy_from_slice(&len.to_le_bytes());
        }
        let sum = checksum(&out[12..]);
        out.extend_from_slice(&sum);
        // The new (empty) journal must exist before index.bin points at it.
        let new_path = self.journal_path(new_gen);
        let nf = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&new_path)?;
        atomic_write(&self.dir.join("index.bin"), &out)?;
        let old = self.journal_path(self.gen);
        self.gen = new_gen;
        self.journal = Some(nf);
        self.journal_bytes = 0;
        self.dirty = false;
        let _ = std::fs::remove_file(old);
        if clean {
            // Exact hwm: the next process continues right where this one stopped (no suspect range).
            self.write_alloc(index.next_id.max(1), index.seq)?;
        }
        Ok(())
    }

    /// Delete all persisted index state (index id change).
    pub fn reset(&mut self) -> io::Result<()> {
        let _ = std::fs::remove_file(self.dir.join("index.bin"));
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with("journal.") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        self.gen = 0;
        self.journal = None;
        self.journal_bytes = 0;
        Ok(())
    }
}

/// Parse `index.bin`: the header and an iterator over the node records (parents first).
pub fn decode_checkpoint(raw: &[u8]) -> Option<(Checkpoint, NodeRecords<'_>)> {
    if raw.len() < 12 + 4 + 8 + 32 || &raw[..8] != MAGIC {
        return None;
    }
    let fmt = u32::from_le_bytes(raw[8..12].try_into().ok()?);
    if fmt != FORMAT {
        return None;
    }
    let body = &raw[12..raw.len() - 32];
    if checksum(body)[..] != raw[raw.len() - 32..] {
        return None;
    }
    let hlen = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
    let header = body.get(4..4 + hlen)?;
    let cp: Checkpoint = postcard::from_bytes(header).ok()?;
    let rest = body.get(4 + hlen..)?;
    let count = u64::from_le_bytes(rest.get(..8)?.try_into().ok()?);
    Some((
        cp,
        NodeRecords {
            buf: &rest[8..],
            left: count,
        },
    ))
}

pub struct NodeRecords<'a> {
    buf: &'a [u8],
    left: u64,
}

impl<'a> Iterator for NodeRecords<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.left == 0 || self.buf.len() < 4 {
            return None;
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        let rec = self.buf.get(4..4 + len)?;
        self.buf = &self.buf[4 + len..];
        self.left -= 1;
        Some(rec)
    }
}

pub fn machine_id() -> String {
    for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(s) = std::fs::read_to_string(p) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{NKind, Txn};
    use crate::sys::Stat;

    fn st(ino: u64, dir: bool) -> Stat {
        Stat {
            dev: 7,
            ino,
            btime_ns: 1,
            mode: if dir {
                libc::S_IFDIR | 0o755
            } else {
                libc::S_IFREG | 0o644
            },
            nlink: 1,
            size: 1,
            ..Default::default()
        }
    }

    #[test]
    fn checkpoint_journal_replay() {
        let t = tempfile::tempdir().unwrap();
        let mut store = Store::open(t.path()).unwrap();
        assert!(store.load().unwrap().is_none());
        let mut idx = Index::new(vec!["x".into()]);
        let r = idx.create_root("r", &st(1, true));
        let mut txn = Txn::default();
        let a = idx.create(r, "a", &st(2, true), NKind::Dir, None, &mut txn);
        let ops = OpTable::default();
        let origin = Origin::default();
        store.reserve(idx.next_id, idx.seq).unwrap();
        store
            .checkpoint(42, &origin, &idx, &ops, &[], 0, false)
            .unwrap();
        // journal a new file + op
        let f = idx.create(a, "f", &st(3, false), NKind::File, None, &mut txn);
        let recs = vec![
            JRec::Node(idx.pnode(f)),
            JRec::Seq(idx.seq),
            JRec::Op(OpRec {
                op: [9; 16],
                resp: vec![1, 2],
                time: crate::sys::now_secs(),
            }),
        ];
        store.append(&recs, true).unwrap();
        // torn tail
        store.append(&[JRec::Seq(1)], false).unwrap();
        drop(store);
        let jp = std::fs::read_dir(t.path())
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().starts_with("journal."))
            .unwrap()
            .path();
        let len = std::fs::metadata(&jp).unwrap().len();
        let f2 = OpenOptions::new().write(true).open(&jp).unwrap();
        f2.set_len(len - 2).unwrap();

        let mut store = Store::open(t.path()).unwrap();
        let l = store.load().unwrap().unwrap();
        assert_eq!(l.index_id, 42);
        assert!(l.replayed);
        let a2 = l.index.lookup(l.index.root, "a").unwrap();
        let f2 = l.index.lookup(a2, "f").unwrap();
        assert_eq!(l.index.node(f2).id, idx.node(f).id);
        assert_eq!(l.ops.get(&[9; 16]).unwrap().resp, vec![1, 2]);
        // hwm reserved beyond seq → a suspect range and seq continues above it
        assert!(l.index.seq >= store.seq_hwm);
        assert!(l.index.next_id >= store.id_hwm);
        assert!(!l.suspect.is_empty());
    }

    /// Journal replay of a rename-over (`mv b a`: `a` keeps its id and takes b's inode, b's id
    /// is removed — reconcile's `finish`). A txn is journalled upserts first, removals after:
    /// replaying a's upsert found b still holding the inode in the identity map (two live
    /// nodes, "keep the first"), then b's removal dropped the inode from the map. After a crash
    /// `a` was no longer found by inode: a new hard link to it never dirtied it (stress seed
    /// 81 with UNLATCHD_DEBOUNCE_MS=0: `link n5 n1; write n1` left n5's old size), and a move of
    /// it would split its id.
    #[test]
    fn replayed_rename_over_keeps_the_destination_findable_by_inode() {
        let t = tempfile::tempdir().unwrap();
        let mut store = Store::open(t.path()).unwrap();
        let mut idx = Index::new(vec![]);
        let r = idx.create_root("r", &st(1, true));
        let mut txn = Txn::default();
        let a = idx.create(r, "a", &st(10, false), NKind::File, None, &mut txn);
        let b = idx.create(r, "b", &st(20, false), NKind::File, None, &mut txn);
        let (a_id, b_id) = (idx.node(a).id, idx.node(b).id);
        store.reserve(idx.next_id, idx.seq).unwrap();
        store
            .checkpoint(
                3,
                &Origin::default(),
                &idx,
                &OpTable::default(),
                &[],
                0,
                false,
            )
            .unwrap();
        // `mv b a`, as reconcile applies it: b moves to "a", then a takes over its inode.
        let mut txn = Txn::default();
        idx.detach(b);
        idx.detach(a);
        idx.attach(b, r, "a", Some((r, "b")), &mut txn);
        idx.transplant(a, b, &mut txn);
        idx.detach(b);
        idx.remove_subtree(b, true, &mut txn);
        idx.attach(a, r, "a", Some((r, "a")), &mut txn);
        assert_eq!(idx.ident_lookup(&st(20, false)), Some(a), "live index");
        let mut recs: Vec<JRec> = idx.pnodes(&txn).into_iter().map(JRec::Node).collect();
        for &(id, seq) in &txn.removed {
            recs.push(JRec::Remove {
                id,
                seq,
                tomb: true,
                time: 0,
            });
        }
        assert!(matches!(recs.last(), Some(JRec::Remove { id, .. }) if *id == b_id));
        store.append(&recs, true).unwrap();
        drop(store);
        let mut store = Store::open(t.path()).unwrap();
        let l = store.load().unwrap().unwrap();
        assert!(l.replayed);
        let la = l.index.slot_of(a_id).unwrap();
        assert!(l.index.slot_of(b_id).is_none());
        assert_eq!(l.index.node(la).ino, 20);
        assert_eq!(
            l.index.ident_lookup(&st(20, false)),
            Some(la),
            "replayed index finds a by its inode"
        );
    }

    #[test]
    fn clean_checkpoint_has_no_suspect_range() {
        let t = tempfile::tempdir().unwrap();
        let mut store = Store::open(t.path()).unwrap();
        let mut idx = Index::new(vec![]);
        idx.create_root("r", &st(1, true));
        store.reserve(idx.next_id, idx.seq).unwrap();
        store
            .checkpoint(
                1,
                &Origin::default(),
                &idx,
                &OpTable::default(),
                &[],
                0,
                true,
            )
            .unwrap();
        drop(store);
        let mut store = Store::open(t.path()).unwrap();
        let l = store.load().unwrap().unwrap();
        assert!(l.suspect.is_empty());
        assert!(!l.replayed);
        assert_eq!(l.index.seq, idx.seq);
    }

    #[test]
    fn op_table_evicts() {
        let mut t = OpTable::default();
        for i in 0..(OP_MAX + 5) {
            let mut op = [0u8; 16];
            op[..8].copy_from_slice(&(i as u64).to_le_bytes());
            t.insert(OpRec {
                op,
                resp: vec![],
                time: crate::sys::now_secs(),
            });
        }
        assert_eq!(t.len(), OP_MAX);
        assert!(t.get(&[0u8; 16]).is_none());
        let mut old = OpTable::default();
        old.insert(OpRec {
            op: [1; 16],
            resp: vec![],
            time: 0,
        });
        assert!(old.get(&[1; 16]).is_none(), "expired");
        old.expire();
        assert!(old.is_empty());
    }
}
