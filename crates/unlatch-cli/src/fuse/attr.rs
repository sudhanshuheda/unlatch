//! `IpcItem` → kernel attributes.

use fuser::{FileAttr, FileType};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use unlatch_proto::ipc::IpcItem;
use unlatch_proto::{Kind, ACCESS_R, ACCESS_W, ACCESS_X};

/// Advertised preferred I/O size: large, so `cp`/`cat` issue few, big reads.
pub const BLKSIZE: u32 = 128 * 1024;

pub fn time_of_ns(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_nanos(ns.unsigned_abs()))
            .unwrap_or(UNIX_EPOCH)
    }
}

pub fn ns_of_time(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

pub fn now_ns() -> i64 {
    ns_of_time(SystemTime::now())
}

/// Permission bits shown to the kernel (`default_permissions` checks them).
///
/// The owner triad comes from `Entry.access` — what the *daemon's* user may actually do on the
/// VM — because files are shown as owned by the local user; showing the VM's owner bits would
/// let the kernel allow writes the VM then refuses. Group/other bits come from the VM mode.
/// The exec bit on files follows `user_exec` (the engine's exec rule, D12). setuid/setgid are
/// never shown.
pub fn perm_of(item: &IpcItem) -> u16 {
    let e = &item.entry;
    match e.kind {
        Kind::Symlink if !item.symlink_blocked => 0o777,
        Kind::Symlink => 0o444,
        _ => {
            let is_dir = e.kind == Kind::Dir;
            let owner = if e.access == 0 {
                // Daemon did not compute access (older daemon): fall back to the owner bits.
                e.mode & 0o700
            } else {
                let mut o = 0;
                if e.access & ACCESS_R != 0 {
                    o |= 0o400;
                }
                if e.access & ACCESS_W != 0 {
                    o |= 0o200;
                }
                if e.access & ACCESS_X != 0 {
                    o |= 0o100;
                }
                o
            };
            let mut perm = owner | (e.mode & 0o077) | if is_dir { e.mode & 0o1000 } else { 0 };
            if !is_dir && !item.user_exec {
                perm &= !0o111;
            }
            perm as u16
        }
    }
}

pub fn file_type_of(item: &IpcItem) -> FileType {
    match item.entry.kind {
        Kind::Dir => FileType::Directory,
        Kind::File => FileType::RegularFile,
        Kind::Symlink if item.symlink_blocked => FileType::RegularFile,
        Kind::Symlink => FileType::Symlink,
    }
}

/// Size as the kernel should see it (a blocked symlink reads as its target text).
pub fn size_of(item: &IpcItem) -> u64 {
    let e = &item.entry;
    match e.kind {
        Kind::Dir => 4096,
        Kind::Symlink => e
            .symlink_target
            .as_ref()
            .map(|t| t.len() as u64)
            .unwrap_or(e.size),
        Kind::File => e.size,
    }
}

pub fn attr_of(ino: u64, item: &IpcItem, uid: u32, gid: u32) -> FileAttr {
    let e = &item.entry;
    let size = size_of(item);
    let mtime = time_of_ns(e.mtime_ns);
    let crtime = item.local.creation_ns.map(time_of_ns).unwrap_or(mtime);
    FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime,
        kind: file_type_of(item),
        perm: perm_of(item),
        nlink: if e.kind == Kind::Dir { 2 } else { 1 },
        uid,
        gid,
        rdev: 0,
        blksize: BLKSIZE,
        flags: 0,
    }
}

/// Attributes of a locally created file whose create has not been uploaded yet.
pub fn pending_attr(ino: u64, size: u64, mode: u32, mtime_ns: i64, uid: u32, gid: u32) -> FileAttr {
    let t = time_of_ns(mtime_ns);
    FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime: t,
        mtime: t,
        ctime: t,
        crtime: t,
        kind: FileType::RegularFile,
        perm: (mode & 0o777) as u16,
        nlink: 1,
        uid,
        gid,
        rdev: 0,
        blksize: BLKSIZE,
        flags: 0,
    }
}

/// A negative entry: `nodeid == 0` with a timeout makes the kernel cache "does not exist"
/// (editors, git and shells probe many missing names; the push channel invalidates it).
pub fn negative_attr() -> FileAttr {
    pending_attr(0, 0, 0, 0, 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pathres::tests::mk;

    #[test]
    fn owner_bits_follow_access_and_exec_rule() {
        let mut f = mk(2, 1, "f", Kind::File);
        f.entry.mode = 0o755;
        f.entry.access = ACCESS_R | ACCESS_W | ACCESS_X;
        f.user_exec = false;
        assert_eq!(perm_of(&f), 0o644);
        f.user_exec = true;
        assert_eq!(perm_of(&f), 0o755);
        f.entry.access = ACCESS_R;
        assert_eq!(perm_of(&f), 0o455);
        f.entry.access = 0;
        f.entry.mode = 0o640;
        assert_eq!(perm_of(&f), 0o640, "fallback to mode owner bits");
        f.entry.mode = 0o4755;
        f.entry.access = ACCESS_R | ACCESS_W | ACCESS_X;
        assert_eq!(perm_of(&f), 0o755, "setuid never shown");
    }

    #[test]
    fn dirs_keep_x_and_sticky() {
        let mut d = mk(2, 1, "d", Kind::Dir);
        d.entry.mode = 0o1777;
        d.entry.access = ACCESS_R | ACCESS_W | ACCESS_X;
        assert_eq!(perm_of(&d), 0o1777);
        let a = attr_of(5, &d, 10, 20);
        assert_eq!(a.kind, FileType::Directory);
        assert_eq!(a.nlink, 2);
        assert_eq!((a.uid, a.gid, a.ino), (10, 20, 5));
    }

    #[test]
    fn symlinks_and_blocked_symlinks() {
        let mut s = mk(3, 1, "l", Kind::Symlink);
        s.entry.symlink_target = Some("../x".into());
        assert_eq!(file_type_of(&s), FileType::Symlink);
        assert_eq!(perm_of(&s), 0o777);
        assert_eq!(size_of(&s), 4);
        s.symlink_blocked = true;
        assert_eq!(file_type_of(&s), FileType::RegularFile);
        assert_eq!(perm_of(&s), 0o444);
    }

    #[test]
    fn time_round_trip() {
        for ns in [0i64, 1, 1_790_772_896_123_456_789, -5_000_000_001] {
            assert_eq!(ns_of_time(time_of_ns(ns)), ns);
        }
    }

    #[test]
    fn negative_entry_has_zero_ino() {
        assert_eq!(negative_attr().ino, 0);
    }
}
