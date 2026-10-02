//! Thin, safe wrappers over the Linux syscalls unlatchd needs.
//!
//! Everything goes through `libc::syscall` or plain libc functions that exist in both glibc and
//! musl, so the static musl build behaves exactly like the glibc build (musl's libc crate
//! bindings lack `statx`, `openat2` and `renameat2` wrappers).

use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;

pub fn cstr(s: &[u8]) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

/// NUL-terminated copy of a short name on the stack (the scan hot path: one statx per entry
/// must not allocate). Falls back to the heap for long paths.
fn with_cstr<T>(s: &[u8], f: impl FnOnce(&CStr) -> io::Result<T>) -> io::Result<T> {
    if s.len() < 256 {
        let mut buf = [0u8; 256];
        buf[..s.len()].copy_from_slice(s);
        let c = CStr::from_bytes_until_nul(&buf)
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        if c.to_bytes().len() != s.len() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL)); // interior NUL
        }
        f(c)
    } else {
        f(&cstr(s)?)
    }
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn cvt_long(r: libc::c_long) -> io::Result<libc::c_long> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Retry on EINTR.
fn retry<T, F: FnMut() -> io::Result<T>>(mut f: F) -> io::Result<T> {
    loop {
        match f() {
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            r => return r,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// statx

const STATX_BASIC_STATS: u32 = 0x07ff;
const STATX_BTIME: u32 = 0x0800;
const AT_STATX_SYNC_AS_STAT: i32 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct StatxTs {
    tv_sec: i64,
    tv_nsec: u32,
    _reserved: i32,
}

/// Kernel `struct statx` (256 bytes, stable ABI).
#[repr(C)]
#[derive(Clone, Copy)]
struct RawStatx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    _spare0: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: StatxTs,
    stx_btime: StatxTs,
    stx_ctime: StatxTs,
    stx_mtime: StatxTs,
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
    _spare: [u64; 14],
}

const _: () = assert!(std::mem::size_of::<RawStatx>() == 256);

/// The subset of `statx` unlatchd uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    /// Birth time in ns, or 0 when the filesystem does not report it.
    pub btime_ns: i64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl Stat {
    pub fn file_type(&self) -> u32 {
        self.mode & libc::S_IFMT
    }
    pub fn is_dir(&self) -> bool {
        self.file_type() == libc::S_IFDIR
    }
    pub fn is_file(&self) -> bool {
        self.file_type() == libc::S_IFREG
    }
    pub fn is_symlink(&self) -> bool {
        self.file_type() == libc::S_IFLNK
    }
    pub fn perm(&self) -> u32 {
        self.mode & 0o7777
    }
}

fn makedev(major: u32, minor: u32) -> u64 {
    let (ma, mi) = (major as u64, minor as u64);
    ((ma & 0xffff_f000) << 32) | ((ma & 0xfff) << 8) | ((mi & 0xffff_ff00) << 12) | (mi & 0xff)
}

fn ts_ns(t: StatxTs) -> i64 {
    t.tv_sec
        .saturating_mul(1_000_000_000)
        .saturating_add(t.tv_nsec as i64)
}

/// statx(2) calls made by this process (diagnostics: logged at a clean stop).
pub static STATX_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn statx_raw(dirfd: RawFd, path: &CStr, flags: i32) -> io::Result<Stat> {
    STATX_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: RawStatx is plain old data; the kernel fills it.
    let mut buf: RawStatx = unsafe { std::mem::zeroed() };
    retry(|| {
        // SAFETY: valid pointers for the duration of the call.
        let r = unsafe {
            libc::syscall(
                libc::SYS_statx,
                dirfd,
                path.as_ptr(),
                flags | AT_STATX_SYNC_AS_STAT,
                STATX_BASIC_STATS | STATX_BTIME,
                &mut buf as *mut RawStatx,
            )
        };
        cvt_long(r).map(|_| ())
    })?;
    let btime_ns = if buf.stx_mask & STATX_BTIME != 0 {
        ts_ns(buf.stx_btime)
    } else {
        0
    };
    Ok(Stat {
        dev: makedev(buf.stx_dev_major, buf.stx_dev_minor),
        ino: buf.stx_ino,
        btime_ns,
        mode: buf.stx_mode as u32,
        nlink: buf.stx_nlink,
        uid: buf.stx_uid,
        gid: buf.stx_gid,
        size: buf.stx_size,
        mtime_ns: ts_ns(buf.stx_mtime),
        ctime_ns: ts_ns(buf.stx_ctime),
    })
}

/// `statx(dirfd, name, AT_SYMLINK_NOFOLLOW)`.
pub fn statat(dirfd: RawFd, name: &[u8]) -> io::Result<Stat> {
    with_cstr(name, |c| statx_raw(dirfd, c, libc::AT_SYMLINK_NOFOLLOW))
}

/// `statx(fd, "", AT_EMPTY_PATH)` (works on O_PATH fds too).
pub fn fstat(fd: RawFd) -> io::Result<Stat> {
    statx_raw(fd, c"", libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW)
}

/// Mount id of the object `fd` refers to (`STATX_MNT_ID`, Linux ≥ 5.8), or None where the
/// kernel does not report it. Unlike `st_dev` it tells a bind mount of a directory on the same
/// filesystem from the directory it is mounted on.
pub fn mount_id(fd: RawFd) -> Option<u64> {
    const STATX_MNT_ID: u32 = 0x1000;
    // SAFETY: RawStatx is plain old data; the kernel fills it.
    let mut buf: RawStatx = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers for the duration of the call.
    let r = unsafe {
        libc::syscall(
            libc::SYS_statx,
            fd,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW | AT_STATX_SYNC_AS_STAT,
            STATX_MNT_ID,
            &mut buf as *mut RawStatx,
        )
    };
    // stx_mnt_id follows stx_dev_minor (the first spare u64 here).
    (r == 0 && buf.stx_mask & STATX_MNT_ID != 0).then_some(buf._spare[0])
}

/// Path stat (follows nothing at the last component).
pub fn lstat_path(path: &[u8]) -> io::Result<Stat> {
    let c = cstr(path)?;
    statx_raw(libc::AT_FDCWD, &c, libc::AT_SYMLINK_NOFOLLOW)
}

// ---------------------------------------------------------------------------------------------
// open / openat2

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const SYS_OPENAT2: libc::c_long = 437;

pub const DIR_FLAGS: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;

/// Longest relative path handed to one open call (below PATH_MAX, which counts the NUL).
const PATH_CHUNK: usize = 4000;

static OPENAT2_UNSUPPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn owned(fd: libc::c_int) -> OwnedFd {
    // SAFETY: fd was just returned by a successful open-like syscall and is owned by nobody else.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

pub fn openat(dirfd: RawFd, name: &[u8], flags: i32, mode: u32) -> io::Result<OwnedFd> {
    let c = cstr(name)?;
    retry(|| {
        // SAFETY: c is a valid C string.
        let fd = unsafe {
            libc::openat(
                dirfd,
                c.as_ptr(),
                flags | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        };
        cvt(fd).map(owned)
    })
}

/// Open `rel` (a `/`-joined sequence of already-validated components, or empty for the base
/// itself) beneath `base` without following any symlink or magic link, and without ever leaving
/// `base` (D2/§9: openat2 RESOLVE_BENEATH|NO_SYMLINKS|NO_MAGICLINKS; per-component O_NOFOLLOW
/// fallback on kernels < 5.6).
pub fn open_beneath(base: RawFd, rel: &[u8], flags: i32) -> io::Result<OwnedFd> {
    // The kernel takes at most PATH_MAX (4096) bytes per path (ENAMETOOLONG): a deeper relative
    // path is opened in pieces, each beneath the directory the previous piece opened, so the
    // walk stays beneath `base` under the same resolve rules (review: deep trees went missing).
    if rel.len() >= PATH_CHUNK {
        let cut = rel[..PATH_CHUNK]
            .iter()
            .rposition(|&b| b == b'/')
            .filter(|&i| i > 0)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENAMETOOLONG))?;
        let dir = open_beneath(base, &rel[..cut], DIR_FLAGS)?;
        return open_beneath(dir.as_raw_fd(), &rel[cut + 1..], flags);
    }
    let rel_or_dot: &[u8] = if rel.is_empty() { b"." } else { rel };
    if !OPENAT2_UNSUPPORTED.load(std::sync::atomic::Ordering::Relaxed) {
        let c = cstr(rel_or_dot)?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS,
        };
        let r = retry(|| {
            // SAFETY: valid pointers; size matches the struct.
            let r = unsafe {
                libc::syscall(
                    SYS_OPENAT2,
                    base,
                    c.as_ptr(),
                    &how as *const OpenHow,
                    std::mem::size_of::<OpenHow>(),
                )
            };
            cvt_long(r).map(|fd| owned(fd as libc::c_int))
        });
        match r {
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EPERM)) => {
                // ENOSYS: old kernel; EPERM: seccomp filters (some container runtimes).
                OPENAT2_UNSUPPORTED.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            r => return r,
        }
    }
    open_beneath_fallback(base, rel, flags)
}

/// Per-component `openat(O_NOFOLLOW)` walk. Components are validated (no `.`/`..`/empty).
pub fn open_beneath_fallback(base: RawFd, rel: &[u8], flags: i32) -> io::Result<OwnedFd> {
    let comps: Vec<&[u8]> = if rel.is_empty() {
        Vec::new()
    } else {
        rel.split(|&b| b == b'/').collect()
    };
    for c in &comps {
        if c.is_empty() || *c == b"." || *c == b".." {
            return Err(io::Error::from_raw_os_error(libc::EXDEV));
        }
    }
    if comps.is_empty() {
        return openat(base, b".", flags | libc::O_NOFOLLOW, 0);
    }
    let mut cur: Option<OwnedFd> = None;
    for (i, c) in comps.iter().enumerate() {
        let dfd = cur.as_ref().map(|f| f.as_raw_fd()).unwrap_or(base);
        let last = i + 1 == comps.len();
        let fl = if last {
            flags | libc::O_NOFOLLOW
        } else {
            DIR_FLAGS
        };
        cur = Some(openat(dfd, c, fl, 0)?);
    }
    cur.ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
}

#[cfg(test)]
pub fn force_openat2_fallback(on: bool) {
    OPENAT2_UNSUPPORTED.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn open_path(path: &[u8], flags: i32) -> io::Result<OwnedFd> {
    openat(libc::AT_FDCWD, path, flags, 0)
}

/// O_TMPFILE in `dirfd` (unnamed, linkable). Errors with EOPNOTSUPP/EISDIR/ENOENT where the
/// filesystem does not support it.
pub fn open_tmpfile(dirfd: RawFd, mode: u32) -> io::Result<OwnedFd> {
    openat(dirfd, b".", libc::O_TMPFILE | libc::O_RDWR, mode)
}

// ---------------------------------------------------------------------------------------------
// directory reading (getdents64)

pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;
pub const DT_UNKNOWN: u8 = 0;

pub struct DirEnt {
    pub name: Vec<u8>,
    pub d_type: u8,
}

/// Read every entry of an open directory fd (excluding `.` and `..`). Rewinds first.
pub fn read_dir_fd(fd: RawFd) -> io::Result<Vec<DirEnt>> {
    // SAFETY: lseek on a valid fd.
    unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = retry(|| {
            // SAFETY: buf is valid for buf.len() bytes.
            let r = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf.as_mut_ptr(), buf.len()) };
            cvt_long(r)
        })? as usize;
        if n == 0 {
            break;
        }
        let mut off = 0usize;
        while off + 19 <= n {
            let reclen = u16::from_ne_bytes([buf[off + 16], buf[off + 17]]) as usize;
            let d_type = buf[off + 18];
            if reclen == 0 || off + reclen > n {
                break;
            }
            let name_bytes = &buf[off + 19..off + reclen];
            let end = name_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_bytes.len());
            let name = &name_bytes[..end];
            if name != b"." && name != b".." {
                out.push(DirEnt {
                    name: name.to_vec(),
                    d_type,
                });
            }
            off += reclen;
        }
    }
    Ok(out)
}

/// Wait until no rename(2) is in progress in the directory open at `fd`. rename holds the
/// inode lock of both parent directories (exclusive) from before the rename until after it has
/// queued its IN_MOVED_FROM *and* IN_MOVED_TO; getdents64 takes the lock shared, so once this
/// returns, the IN_MOVED_TO of any rename out of this directory whose IN_MOVED_FROM was
/// already read is queued too. Reads at most one small buffer of entries (discarded).
pub fn dir_lock_barrier(fd: RawFd) {
    let mut buf = [0u8; 512];
    // SAFETY: lseek on a valid fd; buf is valid for buf.len() bytes.
    unsafe {
        libc::lseek(fd, 0, libc::SEEK_SET);
        libc::syscall(libc::SYS_getdents64, fd, buf.as_mut_ptr(), buf.len());
    }
}

// ---------------------------------------------------------------------------------------------
// mutations

pub fn mkdirat(dirfd: RawFd, name: &[u8], mode: u32) -> io::Result<()> {
    let c = cstr(name)?;
    // SAFETY: valid C string.
    retry(|| cvt(unsafe { libc::mkdirat(dirfd, c.as_ptr(), mode as libc::mode_t) }).map(|_| ()))
}

pub fn symlinkat(target: &[u8], dirfd: RawFd, name: &[u8]) -> io::Result<()> {
    let t = cstr(target)?;
    let c = cstr(name)?;
    // SAFETY: valid C strings.
    retry(|| cvt(unsafe { libc::symlinkat(t.as_ptr(), dirfd, c.as_ptr()) }).map(|_| ()))
}

pub fn readlinkat(dirfd: RawFd, name: &[u8]) -> io::Result<Vec<u8>> {
    with_cstr(name, |c| readlinkat_c(dirfd, c))
}

fn readlinkat_c(dirfd: RawFd, c: &CStr) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; 256];
    loop {
        // SAFETY: buf valid for len bytes.
        let n = unsafe {
            libc::readlinkat(
                dirfd,
                c.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        let n = n as usize;
        if n < buf.len() {
            buf.truncate(n);
            return Ok(buf);
        }
        buf.resize(buf.len() * 2, 0);
    }
}

pub fn unlinkat(dirfd: RawFd, name: &[u8], dir: bool) -> io::Result<()> {
    let c = cstr(name)?;
    let flags = if dir { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: valid C string.
    retry(|| cvt(unsafe { libc::unlinkat(dirfd, c.as_ptr(), flags) }).map(|_| ()))
}

pub const RENAME_NOREPLACE: u32 = 1;
pub const RENAME_EXCHANGE: u32 = 2;

pub fn renameat2(
    olddir: RawFd,
    old: &[u8],
    newdir: RawFd,
    new: &[u8],
    flags: u32,
) -> io::Result<()> {
    let o = cstr(old)?;
    let n = cstr(new)?;
    retry(|| {
        // SAFETY: valid C strings.
        let r = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                olddir,
                o.as_ptr(),
                newdir,
                n.as_ptr(),
                flags,
            )
        };
        cvt_long(r).map(|_| ())
    })
}

/// Give an O_TMPFILE (or any) fd a name: `linkat("/proc/self/fd/N", dirfd/name, FOLLOW)`.
/// (AT_EMPTY_PATH would need CAP_DAC_READ_SEARCH.)
pub fn link_fd(fd: RawFd, dirfd: RawFd, name: &[u8]) -> io::Result<()> {
    let p = cstr(format!("/proc/self/fd/{fd}").as_bytes())?;
    let c = cstr(name)?;
    retry(|| {
        // SAFETY: valid C strings.
        cvt(unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                p.as_ptr(),
                dirfd,
                c.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        })
        .map(|_| ())
    })
}

/// A new read-only open file description of the inode `fd` refers to (O_TMPFILE included),
/// through `/proc/self/fd/N`. Reading through it and closing it raise no inotify event in the
/// watched mask (IN_ACCESS / IN_CLOSE_NOWRITE are not watched).
pub fn reopen_ro(fd: RawFd) -> io::Result<OwnedFd> {
    openat(
        libc::AT_FDCWD,
        format!("/proc/self/fd/{fd}").as_bytes(),
        libc::O_RDONLY | libc::O_NONBLOCK,
        0,
    )
}

/// A lease break (someone opening the inode while we hold a lease on it) is signalled with
/// SIGIO, whose default action terminates the process.
fn ignore_sigio() {
    static IGNORE_SIGIO: std::sync::Once = std::sync::Once::new();
    // SAFETY: unlatchd uses no SIGIO-driven I/O; ignoring it changes nothing else.
    IGNORE_SIGIO.call_once(|| unsafe {
        libc::signal(libc::SIGIO, libc::SIG_IGN);
    });
}

/// `fcntl(F_SETLEASE, F_WRLCK)` on `fd`: granted only while no other open file description
/// refers to the inode (readers included) — "nobody else can write to it". Released by
/// closing `fd`. Needs the file's owner (or CAP_LEASE) and a filesystem with leases.
pub fn lease_exclusive(fd: RawFd) -> io::Result<()> {
    ignore_sigio();
    // SAFETY: plain fcntl on an fd.
    cvt(unsafe { libc::fcntl(fd, libc::F_SETLEASE, libc::F_WRLCK) }).map(|_| ())
}

/// `fcntl(F_SETLEASE, F_RDLCK)` on the read-only `fd`: granted only while the inode is open
/// for writing nowhere (our own descriptors included: EAGAIN). From then on an open of the
/// inode for writing (O_WRONLY / O_RDWR, with or without O_TRUNC) or a `truncate(2)` by anyone
/// breaks it — and waits until we let go (closing `fd` releases it). Read-only opens, `stat`,
/// `chmod`/`chown`/xattrs, `link`, `rename` (RENAME_EXCHANGE included) and `unlink` do not.
/// One content change gets past it: `open(O_RDONLY | O_TRUNC)` truncates to 0 bytes without a
/// break (Linux 6.8, tmpfs/ext4/xfs) — it always changes the size, so a stat check sees it.
/// Needs the file's owner (or CAP_LEASE) and a filesystem with leases.
pub fn lease_shared(fd: RawFd) -> io::Result<()> {
    ignore_sigio();
    // SAFETY: plain fcntl on an fd.
    cvt(unsafe { libc::fcntl(fd, libc::F_SETLEASE, libc::F_RDLCK) }).map(|_| ())
}

/// Whether the lease taken with [`lease_shared`] on `fd` is still whole: no open of the inode
/// for writing (and no truncate) was attempted since (an attempt starts a lease break, after
/// which `F_GETLEASE` reports `F_UNLCK`, the type the lease is being broken to; when the break
/// times out the lease is gone, also `F_UNLCK`).
pub fn shared_lease_intact(fd: RawFd) -> bool {
    // SAFETY: plain fcntl on an fd.
    unsafe { libc::fcntl(fd, libc::F_GETLEASE) == libc::F_RDLCK }
}

pub fn fsync(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain syscall on an fd.
    retry(|| cvt(unsafe { libc::fsync(fd) }).map(|_| ()))
}

/// Data and the metadata needed to read it back (size), not timestamps.
pub fn fdatasync(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain syscall on an fd.
    retry(|| cvt(unsafe { libc::fdatasync(fd) }).map(|_| ()))
}

pub fn fchmod(fd: RawFd, mode: u32) -> io::Result<()> {
    // SAFETY: plain syscall on an fd.
    retry(|| cvt(unsafe { libc::fchmod(fd, mode as libc::mode_t) }).map(|_| ()))
}

/// chmod through `/proc/self/fd/N` (works for O_PATH fds of files and dirs, never for symlinks
/// — callers never pass one).
pub fn chmod_fd_path(fd: RawFd, mode: u32) -> io::Result<()> {
    let p = cstr(format!("/proc/self/fd/{fd}").as_bytes())?;
    // SAFETY: valid C string.
    retry(|| cvt(unsafe { libc::chmod(p.as_ptr(), mode as libc::mode_t) }).map(|_| ()))
}

pub fn fchown(fd: RawFd, uid: u32, gid: u32) -> io::Result<()> {
    // SAFETY: plain syscall on an fd.
    retry(|| cvt(unsafe { libc::fchown(fd, uid, gid) }).map(|_| ()))
}

fn timespec_ns(ns: i64) -> libc::timespec {
    libc::timespec {
        tv_sec: ns.div_euclid(1_000_000_000) as _,
        tv_nsec: ns.rem_euclid(1_000_000_000) as _,
    }
}

fn omit() -> libc::timespec {
    libc::timespec {
        tv_sec: 0,
        tv_nsec: libc::UTIME_OMIT as _,
    }
}

/// Set mtime (atime untouched) of `dirfd/name` without following a final symlink.
pub fn set_mtime_at(dirfd: RawFd, name: &[u8], mtime_ns: i64) -> io::Result<()> {
    let c = cstr(name)?;
    let ts = [omit(), timespec_ns(mtime_ns)];
    // SAFETY: valid pointers.
    retry(|| {
        cvt(unsafe { libc::utimensat(dirfd, c.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) })
            .map(|_| ())
    })
}

/// Set mtime through an fd (O_PATH ok via /proc).
pub fn set_mtime_fd(fd: RawFd, mtime_ns: i64) -> io::Result<()> {
    let p = cstr(format!("/proc/self/fd/{fd}").as_bytes())?;
    let ts = [omit(), timespec_ns(mtime_ns)];
    // SAFETY: valid pointers.
    retry(|| {
        cvt(unsafe { libc::utimensat(libc::AT_FDCWD, p.as_ptr(), ts.as_ptr(), 0) }).map(|_| ())
    })
}

pub fn ftruncate(fd: RawFd, len: u64) -> io::Result<()> {
    // SAFETY: plain syscall on an fd.
    retry(|| cvt(unsafe { libc::ftruncate(fd, len as libc::off_t) }).map(|_| ()))
}

pub fn pread(fd: RawFd, buf: &mut [u8], off: u64) -> io::Result<usize> {
    retry(|| {
        // SAFETY: buf valid for its length.
        let r = unsafe {
            libc::pread(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                off as libc::off_t,
            )
        };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(r as usize)
        }
    })
}

pub fn write_all_fd(fd: RawFd, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = retry(|| {
            // SAFETY: buf valid for its length.
            let r = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
            if r < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(r as usize)
            }
        })?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        buf = &buf[n..];
    }
    Ok(())
}

pub fn pwrite_all(fd: RawFd, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = retry(|| {
            // SAFETY: buf valid for its length.
            let r = unsafe {
                libc::pwrite(
                    fd,
                    buf.as_ptr() as *const libc::c_void,
                    buf.len(),
                    off as libc::off_t,
                )
            };
            if r < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(r as usize)
            }
        })?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

/// Copy `user.*` xattrs from one fd to another (best effort; D-(d)3.5).
pub fn copy_user_xattrs(from: RawFd, to: RawFd) {
    let mut names = vec![0u8; 4096];
    // SAFETY: buffer valid.
    let n = unsafe { libc::flistxattr(from, names.as_mut_ptr() as *mut libc::c_char, names.len()) };
    if n <= 0 {
        return;
    }
    names.truncate(n as usize);
    for name in names.split(|&b| b == 0).filter(|n| n.starts_with(b"user.")) {
        let Ok(cn) = cstr(name) else { continue };
        let mut val = vec![0u8; 64 * 1024];
        // SAFETY: buffer valid.
        let len = unsafe {
            libc::fgetxattr(
                from,
                cn.as_ptr(),
                val.as_mut_ptr() as *mut libc::c_void,
                val.len(),
            )
        };
        if len < 0 {
            continue;
        }
        // SAFETY: buffer valid.
        unsafe {
            libc::fsetxattr(
                to,
                cn.as_ptr(),
                val.as_ptr() as *const libc::c_void,
                len as usize,
                0,
            )
        };
    }
}

// ---------------------------------------------------------------------------------------------
// flock

pub fn flock(fd: RawFd, exclusive: bool, nonblock: bool) -> io::Result<()> {
    let mut op = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    if nonblock {
        op |= libc::LOCK_NB;
    }
    // SAFETY: plain syscall on an fd.
    retry(|| cvt(unsafe { libc::flock(fd, op) }).map(|_| ()))
}

pub fn is_would_block(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EWOULDBLOCK)
}

// ---------------------------------------------------------------------------------------------
// statfs

pub struct FsInfo {
    pub f_type: i64,
    pub fsid: u64,
}

pub fn fstatfs(fd: RawFd) -> io::Result<FsInfo> {
    // SAFETY: statfs is POD.
    let mut s: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer.
    retry(|| cvt(unsafe { libc::fstatfs(fd, &mut s) }).map(|_| ()))?;
    // SAFETY: fsid_t is two i32s on Linux.
    let raw: [i32; 2] = unsafe { std::mem::transmute(s.f_fsid) };
    let fsid = ((raw[0] as u32 as u64) << 32) | raw[1] as u32 as u64;
    Ok(FsInfo {
        f_type: s.f_type as i64,
        fsid,
    })
}

/// Filesystems on which inotify misses remote writers → poll (D14/D22).
pub fn is_network_fs(f_type: i64) -> bool {
    const NFS: i64 = 0x6969;
    const SMB: i64 = 0x517B;
    const CIFS: i64 = 0xFF53_4D42;
    const SMB2: i64 = 0xFE53_4D42;
    const FUSE: i64 = 0x6573_5546;
    const V9FS: i64 = 0x0102_1997;
    const VIRTIOFS_FUSE: i64 = FUSE; // virtiofs reports the FUSE magic
    const CEPH: i64 = 0x00C3_6400;
    const AFS: i64 = 0x5346_414F;
    const LUSTRE: i64 = 0x0BD0_0BD0;
    let t = f_type & 0xffff_ffff;
    matches!(
        t,
        NFS | SMB | CIFS | SMB2 | FUSE | V9FS | CEPH | AFS | LUSTRE
    ) || t == VIRTIOFS_FUSE
}

// ---------------------------------------------------------------------------------------------
// inotify

pub const IN_MODIFY: u32 = 0x0000_0002;
pub const IN_ATTRIB: u32 = 0x0000_0004;
pub const IN_CLOSE_WRITE: u32 = 0x0000_0008;
pub const IN_MOVED_FROM: u32 = 0x0000_0040;
pub const IN_MOVED_TO: u32 = 0x0000_0080;
pub const IN_CREATE: u32 = 0x0000_0100;
pub const IN_DELETE: u32 = 0x0000_0200;
pub const IN_DELETE_SELF: u32 = 0x0000_0400;
pub const IN_MOVE_SELF: u32 = 0x0000_0800;
pub const IN_UNMOUNT: u32 = 0x0000_2000;
pub const IN_Q_OVERFLOW: u32 = 0x0000_4000;
pub const IN_IGNORED: u32 = 0x0000_8000;
pub const IN_ONLYDIR: u32 = 0x0100_0000;
pub const IN_EXCL_UNLINK: u32 = 0x0400_0000;
pub const IN_ISDIR: u32 = 0x4000_0000;

pub const WATCH_MASK: u32 = IN_MODIFY
    | IN_ATTRIB
    | IN_CLOSE_WRITE
    | IN_MOVED_FROM
    | IN_MOVED_TO
    | IN_CREATE
    | IN_DELETE
    | IN_DELETE_SELF
    | IN_MOVE_SELF
    | IN_ONLYDIR
    | IN_EXCL_UNLINK;

pub fn inotify_init() -> io::Result<OwnedFd> {
    // SAFETY: plain syscall.
    let fd = cvt(unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) })?;
    Ok(owned(fd))
}

/// Watch the directory an open fd refers to (via `/proc/self/fd`, so the watched inode is exactly
/// the one about to be read — D14 "watches added before readdir").
pub fn inotify_add_watch_fd(ifd: RawFd, dirfd: RawFd, mask: u32) -> io::Result<i32> {
    let p = cstr(format!("/proc/self/fd/{dirfd}").as_bytes())?;
    // SAFETY: valid C string.
    cvt(unsafe { libc::inotify_add_watch(ifd, p.as_ptr(), mask) })
}

pub fn inotify_rm_watch(ifd: RawFd, wd: i32) {
    // SAFETY: plain syscall; failure (already gone) is harmless.
    unsafe { libc::inotify_rm_watch(ifd, wd) };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InotifyEvent {
    pub wd: i32,
    pub mask: u32,
    pub cookie: u32,
    pub name: Vec<u8>,
}

/// Non-blocking drain of everything currently readable. Returns Ok(empty) on EAGAIN.
pub fn inotify_read(ifd: RawFd, buf: &mut [u8], out: &mut Vec<InotifyEvent>) -> io::Result<usize> {
    let mut total = 0;
    loop {
        // SAFETY: buf valid.
        let n = unsafe { libc::read(ifd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => return Ok(total),
                _ => return Err(e),
            }
        }
        if n == 0 {
            return Ok(total);
        }
        let n = n as usize;
        let mut off = 0;
        while off + 16 <= n {
            let wd = i32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
            let mask = u32::from_ne_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
            let cookie =
                u32::from_ne_bytes([buf[off + 8], buf[off + 9], buf[off + 10], buf[off + 11]]);
            let len =
                u32::from_ne_bytes([buf[off + 12], buf[off + 13], buf[off + 14], buf[off + 15]])
                    as usize;
            let name_raw = &buf[off + 16..(off + 16 + len).min(n)];
            let end = name_raw
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_raw.len());
            out.push(InotifyEvent {
                wd,
                mask,
                cookie,
                name: name_raw[..end].to_vec(),
            });
            off += 16 + len;
            total += 1;
        }
    }
}

/// poll() one fd for readability (or POLLPRI) with a timeout in ms (-1 = forever).
pub fn poll_fds(fds: &[(RawFd, i16)], timeout_ms: i32) -> io::Result<Vec<i16>> {
    let mut p: Vec<libc::pollfd> = fds
        .iter()
        .map(|&(fd, ev)| libc::pollfd {
            fd,
            events: ev,
            revents: 0,
        })
        .collect();
    loop {
        // SAFETY: p valid.
        let r = unsafe { libc::poll(p.as_mut_ptr(), p.len() as libc::nfds_t, timeout_ms) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        return Ok(p.iter().map(|x| x.revents).collect());
    }
}

// ---------------------------------------------------------------------------------------------
// misc

pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: valid array.
    cvt(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) })?;
    Ok((owned(fds[0]), owned(fds[1])))
}

pub fn geteuid() -> u32 {
    // SAFETY: always succeeds.
    unsafe { libc::geteuid() }
}

pub fn getegid() -> u32 {
    // SAFETY: always succeeds.
    unsafe { libc::getegid() }
}

pub fn getgroups() -> Vec<u32> {
    // SAFETY: first call with 0 returns the count.
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if n <= 0 {
        return Vec::new();
    }
    let mut v = vec![0 as libc::gid_t; n as usize];
    // SAFETY: v has room for n entries.
    let m = unsafe { libc::getgroups(n, v.as_mut_ptr()) };
    v.truncate(m.max(0) as usize);
    v
}

pub fn umask() -> u32 {
    // SAFETY: umask always succeeds; set it back immediately.
    unsafe {
        let old = libc::umask(0o022);
        libc::umask(old);
        old as u32
    }
}

/// glibc: cap malloc arenas. The parallel walker (16–64 threads) would otherwise leave one
/// fragmented arena per thread behind (T12 RSS). No-op elsewhere (musl has no arenas).
pub fn limit_malloc_arenas() {
    #[cfg(target_env = "gnu")]
    // SAFETY: mallopt is safe to call before threads start.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}

/// Return freed heap memory to the OS after large transient work (full scans, checkpoints).
/// glibc keeps freed memory in per-thread arenas; musl's allocator already unmaps eagerly.
pub fn trim_heap() {
    #[cfg(target_env = "gnu")]
    // SAFETY: malloc_trim has no preconditions.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Heap bytes in use / free-but-held (glibc only; diagnostics for T12).
pub fn heap_stats() -> Option<(usize, usize)> {
    #[cfg(target_env = "gnu")]
    {
        // SAFETY: mallinfo2 has no preconditions.
        let m = unsafe { libc::mallinfo2() };
        Some((m.uordblks + m.hblkhd, m.fordblks))
    }
    #[cfg(not(target_env = "gnu"))]
    {
        None
    }
}

pub fn getpid() -> i32 {
    // SAFETY: always succeeds.
    unsafe { libc::getpid() }
}

/// SO_PEERCRED of a unix socket: (pid, uid, gid).
pub fn peer_cred(fd: RawFd) -> io::Result<(i32, u32, u32)> {
    // SAFETY: ucred is POD.
    let mut c: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: valid pointers.
    cvt(unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut c as *mut _ as *mut libc::c_void,
            &mut len,
        )
    })?;
    Ok((c.pid, c.uid, c.gid))
}

pub fn os(b: &[u8]) -> &OsStr {
    OsStr::from_bytes(b)
}

pub fn is_errno(e: &io::Error, code: i32) -> bool {
    e.raw_os_error() == Some(code)
}

/// Wall clock in ns.
pub fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

pub fn now_secs() -> u64 {
    (now_ns() / 1_000_000_000).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// Relative paths beyond PATH_MAX open in pieces, and every piece keeps the resolve rules: a
    /// symlink deep in the path is never followed (a planted link must not lead out of the root).
    #[test]
    fn open_beneath_paths_beyond_path_max() {
        let t = tempfile::tempdir().unwrap();
        let name = "d".repeat(200);
        let mut rel = Vec::new();
        let base = open_path(t.path().as_os_str().as_bytes(), DIR_FLAGS).unwrap();
        let mut cur = open_path(t.path().as_os_str().as_bytes(), DIR_FLAGS).unwrap();
        for i in 0..30 {
            mkdirat(cur.as_raw_fd(), name.as_bytes(), 0o755).unwrap();
            cur = openat(cur.as_raw_fd(), name.as_bytes(), DIR_FLAGS, 0).unwrap();
            if i > 0 {
                rel.push(b'/');
            }
            rel.extend_from_slice(name.as_bytes());
        }
        assert!(rel.len() > 4096);
        let fd = open_beneath(base.as_raw_fd(), &rel, DIR_FLAGS).unwrap();
        let (a, b) = (
            fstat(fd.as_raw_fd()).unwrap(),
            fstat(cur.as_raw_fd()).unwrap(),
        );
        assert_eq!((a.dev, a.ino), (b.dev, b.ino));
        // A symlink as the last component (past the first piece) is refused.
        symlinkat(b"/", cur.as_raw_fd(), b"esc").unwrap();
        let mut bad = rel.clone();
        bad.extend_from_slice(b"/esc");
        assert!(open_beneath(base.as_raw_fd(), &bad, DIR_FLAGS).is_err());
        force_openat2_fallback(true);
        let r = open_beneath(base.as_raw_fd(), &rel, DIR_FLAGS).map(|_| ());
        let r2 = open_beneath(base.as_raw_fd(), &bad, DIR_FLAGS).map(|_| ());
        force_openat2_fallback(false);
        r.unwrap();
        assert!(r2.is_err());
    }

    #[test]
    fn statx_and_dirs() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("f"), b"hello").unwrap();
        std::fs::create_dir(t.path().join("d")).unwrap();
        std::os::unix::fs::symlink("f", t.path().join("l")).unwrap();
        let root = open_path(t.path().as_os_str().as_bytes(), DIR_FLAGS).unwrap();
        let s = statat(root.as_raw_fd(), b"f").unwrap();
        assert!(s.is_file());
        assert_eq!(s.size, 5);
        let md = std::fs::symlink_metadata(t.path().join("f")).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(s.ino, md.ino());
        assert_eq!(s.dev, md.dev());
        assert!(statat(root.as_raw_fd(), b"l").unwrap().is_symlink());
        assert_eq!(readlinkat(root.as_raw_fd(), b"l").unwrap(), b"f");
        let mut names: Vec<_> = read_dir_fd(root.as_raw_fd())
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        names.sort();
        assert_eq!(names, vec![b"d".to_vec(), b"f".to_vec(), b"l".to_vec()]);
    }

    #[test]
    fn beneath_rejects_symlinks_both_paths() {
        let t = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("a")).unwrap();
        std::os::unix::fs::symlink(out.path(), t.path().join("evil")).unwrap();
        let root = open_path(t.path().as_os_str().as_bytes(), DIR_FLAGS).unwrap();
        assert!(open_beneath(root.as_raw_fd(), b"a", DIR_FLAGS).is_ok());
        assert!(open_beneath(root.as_raw_fd(), b"evil", DIR_FLAGS).is_err());
        assert!(open_beneath_fallback(root.as_raw_fd(), b"evil", DIR_FLAGS).is_err());
        assert!(open_beneath_fallback(root.as_raw_fd(), b"..", DIR_FLAGS).is_err());
        assert!(open_beneath(root.as_raw_fd(), b"../", DIR_FLAGS).is_err());
    }

    #[test]
    fn tmpfile_link_and_exchange() {
        let t = tempfile::tempdir().unwrap();
        let root = open_path(t.path().as_os_str().as_bytes(), DIR_FLAGS).unwrap();
        let f = open_tmpfile(root.as_raw_fd(), 0o600).unwrap();
        write_all_fd(f.as_raw_fd(), b"abc").unwrap();
        link_fd(f.as_raw_fd(), root.as_raw_fd(), b"x").unwrap();
        assert_eq!(std::fs::read(t.path().join("x")).unwrap(), b"abc");
        std::fs::write(t.path().join("y"), b"yy").unwrap();
        renameat2(
            root.as_raw_fd(),
            b"x",
            root.as_raw_fd(),
            b"y",
            RENAME_EXCHANGE,
        )
        .unwrap();
        assert_eq!(std::fs::read(t.path().join("y")).unwrap(), b"abc");
        assert_eq!(std::fs::read(t.path().join("x")).unwrap(), b"yy");
        let e = renameat2(
            root.as_raw_fd(),
            b"x",
            root.as_raw_fd(),
            b"y",
            RENAME_NOREPLACE,
        )
        .unwrap_err();
        assert!(is_errno(&e, libc::EEXIST));
    }

    /// The kernel facts the Write's read lease rests on (`Ops::lease_staged`), on the test's
    /// temp dir and on tmpfs: granted on a read-only descriptor of an O_TMPFILE once its only
    /// writer is closed (not before); kept through our own chmod/chown/xattr/mtime, the
    /// publish (linkat), the replace (link + RENAME_EXCHANGE + unlink of the old name) and
    /// any read-only open; broken by an open for writing (O_WRONLY, O_RDWR, O_APPEND,
    /// O_TRUNC) and by truncate(2), whose caller waits until we let go. An
    /// `O_RDONLY | O_TRUNC` open truncates without a break: it always changes the size.
    #[test]
    fn read_lease_breaks_on_writers_only() {
        use std::time::{Duration, Instant};
        let mut dirs = vec![std::env::temp_dir()];
        if std::path::Path::new("/dev/shm").is_dir() {
            dirs.push("/dev/shm".into());
        }
        for d in dirs {
            let t = tempfile::tempdir_in(&d).unwrap();
            let root = open_path(t.path().as_os_str().as_bytes(), DIR_FLAGS).unwrap();
            let staged = |data: &[u8]| {
                let w = open_tmpfile(root.as_raw_fd(), 0o600).unwrap();
                write_all_fd(w.as_raw_fd(), data).unwrap();
                fsync(w.as_raw_fd()).unwrap();
                let ro = reopen_ro(w.as_raw_fd()).unwrap();
                (w, ro)
            };
            let (w, ro) = staged(b"ours\n");
            match lease_shared(ro.as_raw_fd()) {
                Err(e) if is_errno(&e, libc::EAGAIN) => {}
                Err(e) => {
                    eprintln!("SKIP {}: no leases ({e})", d.display());
                    continue;
                }
                Ok(()) => panic!("{}: read lease granted with our writer open", d.display()),
            }
            drop(w);
            lease_shared(ro.as_raw_fd()).unwrap();
            let fd = ro.as_raw_fd();
            assert!(shared_lease_intact(fd));
            fchmod(fd, 0o644).unwrap();
            let me = fstat(fd).unwrap();
            let _ = fchown(fd, me.uid, me.gid);
            set_mtime_fd(fd, 1_000_000_000).unwrap();
            assert!(shared_lease_intact(fd), "{}: own metadata", d.display());
            link_fd(fd, root.as_raw_fd(), b"x").unwrap();
            assert!(shared_lease_intact(fd), "{}: linkat", d.display());
            let p = t.path().join("x");
            let t0 = Instant::now();
            assert_eq!(std::fs::read(&p).unwrap(), b"ours\n");
            assert!(t0.elapsed() < Duration::from_secs(1), "a reader waited");
            assert!(shared_lease_intact(fd), "{}: read-only open", d.display());
            // Replace: link to a staging name, exchange with the target, unlink the old.
            std::fs::write(t.path().join("y"), b"v0\n").unwrap();
            let (w2, ro2) = staged(b"mac\n");
            drop(w2);
            lease_shared(ro2.as_raw_fd()).unwrap();
            link_fd(ro2.as_raw_fd(), root.as_raw_fd(), b"stage").unwrap();
            renameat2(
                root.as_raw_fd(),
                b"stage",
                root.as_raw_fd(),
                b"y",
                RENAME_EXCHANGE,
            )
            .unwrap();
            unlinkat(root.as_raw_fd(), b"stage", false).unwrap();
            assert!(
                shared_lease_intact(ro2.as_raw_fd()),
                "{}: exchange",
                d.display()
            );
            drop(ro2);
            drop(ro);
            // Writers: each breaks a fresh lease and waits for us.
            type Opener = fn(&std::path::Path);
            let writers: [(&str, Opener); 5] = [
                ("O_WRONLY", |p| {
                    std::fs::OpenOptions::new().write(true).open(p).unwrap();
                }),
                ("O_RDWR", |p| {
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(p)
                        .unwrap();
                }),
                ("O_APPEND", |p| {
                    std::fs::OpenOptions::new().append(true).open(p).unwrap();
                }),
                ("O_WRONLY|O_TRUNC", |p| {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(p)
                        .unwrap();
                }),
                ("truncate", |p| {
                    let c = cstr(p.as_os_str().as_bytes()).unwrap();
                    // SAFETY: valid C string.
                    assert_eq!(unsafe { libc::truncate(c.as_ptr(), 1) }, 0);
                }),
            ];
            for (i, (what, open)) in writers.into_iter().enumerate() {
                let n = format!("w{i}");
                let (w, ro) = staged(b"mac\n");
                drop(w);
                lease_shared(ro.as_raw_fd()).unwrap();
                link_fd(ro.as_raw_fd(), root.as_raw_fd(), n.as_bytes()).unwrap();
                let p = t.path().join(&n);
                let h = std::thread::spawn(move || open(&p));
                let t0 = Instant::now();
                while shared_lease_intact(ro.as_raw_fd()) && t0.elapsed() < Duration::from_secs(10)
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                assert!(
                    !shared_lease_intact(ro.as_raw_fd()),
                    "{}: {what} did not break",
                    d.display()
                );
                std::thread::sleep(Duration::from_millis(50));
                assert!(
                    !h.is_finished(),
                    "{}: {what} did not wait for the lease",
                    d.display()
                );
                drop(ro);
                h.join().unwrap();
            }
            // O_RDONLY | O_TRUNC: no break, but the size goes to 0.
            let (w, ro) = staged(b"mac\n");
            drop(w);
            lease_shared(ro.as_raw_fd()).unwrap();
            link_fd(ro.as_raw_fd(), root.as_raw_fd(), b"rt").unwrap();
            let c = cstr(t.path().join("rt").as_os_str().as_bytes()).unwrap();
            // SAFETY: valid C string; the fd is closed at once.
            let f = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_TRUNC) };
            assert!(f >= 0);
            // SAFETY: our fd.
            unsafe { libc::close(f) };
            assert!(shared_lease_intact(ro.as_raw_fd()));
            assert_eq!(
                fstat(ro.as_raw_fd()).unwrap().size,
                0,
                "O_RDONLY|O_TRUNC truncates"
            );
        }
    }
}
