//! Content cache: `<cache_dir>/<id>-<content-seq>`, LRU by byte budget. Private copies for
//! `fetch` are made with `clonefile` (macOS) / `FICLONE` (Linux) and fall back to a copy.

use crate::{err, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use unlatch_proto::{ErrorCode, ItemId};

type Key = (ItemId, u64);

#[derive(Default)]
struct Idx {
    entries: HashMap<Key, (u64, u64)>, // size, tick
    lru: BTreeMap<u64, Key>,
    by_id: HashMap<ItemId, Vec<u64>>,
    pinned: HashMap<Key, u32>,
    total: u64,
    tick: u64,
}

impl Idx {
    fn touch(&mut self, k: Key) {
        if let Some((_, t)) = self.entries.get_mut(&k) {
            self.lru.remove(t);
            self.tick += 1;
            *t = self.tick;
            self.lru.insert(self.tick, k);
        }
    }

    fn add(&mut self, k: Key, size: u64) {
        if let Some((s, t)) = self.entries.remove(&k) {
            self.lru.remove(&t);
            self.total -= s;
        }
        self.tick += 1;
        self.entries.insert(k, (size, self.tick));
        self.lru.insert(self.tick, k);
        let v = self.by_id.entry(k.0).or_default();
        if !v.contains(&k.1) {
            v.push(k.1);
        }
        self.total += size;
    }

    fn remove(&mut self, k: Key) -> bool {
        match self.entries.remove(&k) {
            Some((s, t)) => {
                self.lru.remove(&t);
                self.total -= s;
                if let Some(v) = self.by_id.get_mut(&k.0) {
                    v.retain(|x| *x != k.1);
                    if v.is_empty() {
                        self.by_id.remove(&k.0);
                    }
                }
                true
            }
            None => false,
        }
    }
}

pub(crate) struct Cache {
    dir: PathBuf,
    budget: u64,
    idx: Mutex<Idx>,
    locks: Mutex<HashMap<Key, Arc<Mutex<()>>>>,
}

fn io_err(what: &str, e: std::io::Error) -> unlatch_proto::ProtoError {
    let code = match e.raw_os_error() {
        Some(libc::ENOSPC) => ErrorCode::NoSpace,
        _ => ErrorCode::Io,
    };
    err(code, format!("{what}: {e}"))
}

fn parse_name(n: &str) -> Option<Key> {
    let (a, b) = n.split_once('-')?;
    Some((ItemId(a.parse().ok()?), b.parse().ok()?))
}

impl Cache {
    pub fn open(dir: &Path, budget: u64) -> Result<Cache> {
        std::fs::create_dir_all(dir).map_err(|e| io_err("create cache dir", e))?;
        let mut files: Vec<(std::time::SystemTime, Key, u64)> = Vec::new();
        for de in std::fs::read_dir(dir)
            .map_err(|e| io_err("read cache dir", e))?
            .flatten()
        {
            let name = de.file_name().to_string_lossy().into_owned();
            if name.starts_with("tmp-") {
                let _ = std::fs::remove_file(de.path());
                continue;
            }
            let (Some(k), Ok(md)) = (parse_name(&name), de.metadata()) else {
                continue;
            };
            files.push((md.modified().unwrap_or(std::time::UNIX_EPOCH), k, md.len()));
        }
        files.sort_by_key(|f| f.0);
        let mut idx = Idx::default();
        for (_, k, size) in files {
            idx.add(k, size);
        }
        let c = Cache {
            dir: dir.to_path_buf(),
            budget,
            idx: Mutex::new(idx),
            locks: Mutex::new(HashMap::new()),
        };
        c.evict();
        Ok(c)
    }

    fn idx(&self) -> std::sync::MutexGuard<'_, Idx> {
        self.idx.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn path(&self, id: ItemId, ver: u64) -> PathBuf {
        self.dir.join(format!("{}-{}", id.0, ver))
    }

    pub fn tmp_path(&self) -> PathBuf {
        self.dir
            .join(format!("tmp-{:032x}", rand::random::<u128>()))
    }

    pub fn total(&self) -> u64 {
        self.idx().total
    }

    pub fn contains(&self, id: ItemId, ver: u64) -> bool {
        self.idx().entries.contains_key(&(id, ver))
    }

    /// Cached path of this exact version, if present (touches LRU and pins it until `unpin`).
    pub fn get_pinned(&self, id: ItemId, ver: u64) -> Option<PathBuf> {
        let mut idx = self.idx();
        if !idx.entries.contains_key(&(id, ver)) {
            return None;
        }
        let p = self.path(id, ver);
        if !p.exists() {
            idx.remove((id, ver));
            return None;
        }
        idx.touch((id, ver));
        *idx.pinned.entry((id, ver)).or_insert(0) += 1;
        Some(p)
    }

    pub fn unpin(&self, id: ItemId, ver: u64) {
        let mut idx = self.idx();
        if let Some(n) = idx.pinned.get_mut(&(id, ver)) {
            *n -= 1;
            if *n == 0 {
                idx.pinned.remove(&(id, ver));
            }
        }
    }

    /// Serializes downloads of one (id, version).
    pub fn key_lock(&self, id: ItemId, ver: u64) -> Arc<Mutex<()>> {
        let mut l = self.locks.lock().unwrap_or_else(|p| p.into_inner());
        if l.len() > 4096 {
            l.retain(|_, v| Arc::strong_count(v) > 1);
        }
        l.entry((id, ver)).or_default().clone()
    }

    /// Move a completed temp file into the cache as `(id, ver)`.
    pub fn insert(&self, id: ItemId, ver: u64, tmp: &Path) -> Result<PathBuf> {
        self.insert_inner(id, ver, tmp, false)
    }

    /// [`Cache::insert`], pinned (until `unpin`) atomically with the insert: the eviction the
    /// insert runs can never remove the entry the caller is about to read, even when it alone
    /// exceeds the budget (a file larger than the whole cache stays readable; it is evicted by
    /// a later insert once unpinned).
    pub fn insert_pinned(&self, id: ItemId, ver: u64, tmp: &Path) -> Result<PathBuf> {
        self.insert_inner(id, ver, tmp, true)
    }

    fn insert_inner(&self, id: ItemId, ver: u64, tmp: &Path, pin: bool) -> Result<PathBuf> {
        let size = std::fs::metadata(tmp)
            .map_err(|e| io_err("stat cache temp", e))?
            .len();
        let dst = self.path(id, ver);
        std::fs::rename(tmp, &dst).map_err(|e| io_err("publish cache file", e))?;
        {
            let mut idx = self.idx();
            idx.add((id, ver), size);
            if pin {
                *idx.pinned.entry((id, ver)).or_insert(0) += 1;
            }
        }
        self.evict();
        Ok(dst)
    }

    /// Adopt a file from elsewhere on the same volume (an upload's staging file).
    pub fn adopt(&self, id: ItemId, ver: u64, src: &Path) -> Result<()> {
        let tmp = self.tmp_path();
        if std::fs::rename(src, &tmp).is_err() {
            std::fs::copy(src, &tmp).map_err(|e| io_err("copy into cache", e))?;
        }
        self.insert(id, ver, &tmp).map(|_| ())
    }

    /// A newer content version was applied: older cached versions of `id` are useless.
    pub fn invalidate_older(&self, id: ItemId, keep: u64) {
        let mut idx = self.idx();
        let vers: Vec<u64> = idx.by_id.get(&id).cloned().unwrap_or_default();
        for v in vers {
            if v != keep && !idx.pinned.contains_key(&(id, v)) && idx.remove((id, v)) {
                let _ = std::fs::remove_file(self.path(id, v));
            }
        }
    }

    pub fn clear(&self) {
        let mut idx = self.idx();
        let keys: Vec<Key> = idx.entries.keys().copied().collect();
        for k in keys {
            if !idx.pinned.contains_key(&k) && idx.remove(k) {
                let _ = std::fs::remove_file(self.path(k.0, k.1));
            }
        }
    }

    fn evict(&self) {
        let mut idx = self.idx();
        let mut skipped = Vec::new();
        while idx.total > self.budget {
            let Some((&t, &k)) = idx.lru.iter().next() else {
                break;
            };
            if idx.pinned.contains_key(&k) {
                idx.lru.remove(&t);
                skipped.push((t, k));
                continue;
            }
            idx.remove(k);
            let _ = std::fs::remove_file(self.path(k.0, k.1));
        }
        for (t, k) in skipped {
            idx.lru.insert(t, k);
        }
    }
}

/// Copy-on-write clone of `src` to `dst` (must not exist), falling back to a byte copy.
pub(crate) fn clone_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let s = CString::new(src.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        let d = CString::new(dst.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        // SAFETY: both are valid NUL-terminated paths; clonefile does not retain them.
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } == 0 {
            return Ok(());
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let from = std::fs::File::open(src)?;
        let to = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dst)?;
        // FICLONE = _IOW(0x94, 9, int)
        const FICLONE: libc::c_ulong = 0x4004_9409;
        // SAFETY: valid fds owned by `from`/`to` for the duration of the call.
        let rc = unsafe { libc::ioctl(to.as_raw_fd(), FICLONE as _, from.as_raw_fd()) };
        if rc == 0 {
            return Ok(());
        }
        drop(to);
        let mut from = from;
        let mut to = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(dst)?;
        std::io::copy(&mut from, &mut to)?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        std::fs::copy(src, dst).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_budget_and_versions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let c = Cache::open(dir.path(), 100).expect("open");
        let put = |id: u64, ver: u64, n: usize| {
            let t = c.tmp_path();
            std::fs::write(&t, vec![7u8; n]).expect("write");
            c.insert(ItemId(id), ver, &t).expect("insert");
        };
        put(1, 1, 40);
        put(2, 1, 40);
        assert!(c.get_pinned(ItemId(1), 1).is_some()); // touch 1 (and pin)
        c.unpin(ItemId(1), 1);
        put(3, 1, 40); // over budget → evicts LRU = 2
        assert!(c.contains(ItemId(1), 1));
        assert!(!c.contains(ItemId(2), 1));
        assert!(!c.path(ItemId(2), 1).exists());
        assert!(c.total() <= 100);
        put(1, 2, 10);
        c.invalidate_older(ItemId(1), 2);
        assert!(!c.contains(ItemId(1), 1));
        assert!(c.contains(ItemId(1), 2));
        // Reopen rebuilds the index from disk.
        drop(c);
        let c = Cache::open(dir.path(), 100).expect("reopen");
        assert!(c.contains(ItemId(1), 2) && c.contains(ItemId(3), 1));
    }

    #[test]
    fn pinned_entries_survive_eviction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let c = Cache::open(dir.path(), 50).expect("open");
        let t = c.tmp_path();
        std::fs::write(&t, vec![1u8; 40]).expect("write");
        c.insert(ItemId(1), 1, &t).expect("insert");
        assert!(c.get_pinned(ItemId(1), 1).is_some());
        let t = c.tmp_path();
        std::fs::write(&t, vec![1u8; 40]).expect("write");
        c.insert(ItemId(2), 1, &t).expect("insert");
        assert!(c.contains(ItemId(1), 1), "pinned entry kept");
        c.unpin(ItemId(1), 1);
    }

    #[test]
    fn oversized_insert_pinned_survives_its_own_eviction() {
        let dir = tempfile::tempdir().expect("tempdir");
        let c = Cache::open(dir.path(), 50).expect("open");
        let put = |id: u64, n: usize, pin: bool| {
            let t = c.tmp_path();
            std::fs::write(&t, vec![1u8; n]).expect("write");
            if pin {
                c.insert_pinned(ItemId(id), 1, &t).expect("insert")
            } else {
                c.insert(ItemId(id), 1, &t).expect("insert")
            }
        };
        put(1, 30, false);
        let p = put(2, 200, true);
        assert!(p.exists(), "larger than the budget, but pinned");
        assert!(
            !c.contains(ItemId(1), 1),
            "the rest was evicted to make room"
        );
        // Readable again while still cached (e.g. the next FUSE read of the same file).
        c.unpin(ItemId(2), 1);
        assert!(c.get_pinned(ItemId(2), 1).is_some());
        c.unpin(ItemId(2), 1);
        // The next insert brings the cache back under budget.
        put(3, 10, false);
        assert!(!c.contains(ItemId(2), 1) && !p.exists());
        assert!(c.total() <= 50);
    }

    #[test]
    fn clone_copies_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"hello").expect("write");
        clone_file(&a, &b).expect("clone");
        assert_eq!(std::fs::read(&b).expect("read"), b"hello");
        assert!(clone_file(&a, &b).is_err(), "dst must not exist");
    }
}
