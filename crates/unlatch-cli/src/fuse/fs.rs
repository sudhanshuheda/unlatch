//! `fuser::Filesystem` adapter: decides inline vs. worker, converts replies.

use super::attr::file_type_of;
use super::pool::Pool;
use super::{lock, Shared};
use crate::backend::Backend;
use fuser::consts::{
    FUSE_ATOMIC_O_TRUNC, FUSE_CACHE_SYMLINKS, FUSE_DO_READDIRPLUS, FUSE_PARALLEL_DIROPS,
};
use fuser::{
    FileType, Filesystem, KernelConfig, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
    TimeOrNow,
};
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::debug;

pub struct UnlatchFs<B: Backend> {
    sh: Arc<Shared<B>>,
    pool: Pool,
}

impl<B: Backend> UnlatchFs<B> {
    pub fn new(sh: Arc<Shared<B>>) -> std::io::Result<UnlatchFs<B>> {
        let pool = Pool::new(sh.opts.workers, "unlatch-fuse")?;
        Ok(UnlatchFs { sh, pool })
    }

    fn run(&self, f: impl FnOnce(&Shared<B>) + Send + 'static) {
        let sh = Arc::clone(&self.sh);
        self.pool.spawn(move || f(&sh));
    }
}

fn mtime_ns_of(t: Option<TimeOrNow>) -> Option<i64> {
    t.map(|t| match t {
        TimeOrNow::SpecificTime(st) => super::attr::ns_of_time(st),
        TimeOrNow::Now => super::attr::ns_of_time(SystemTime::now()),
    })
}

impl<B: Backend> Filesystem for UnlatchFs<B> {
    fn init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        // Each capability separately: an older kernel lacking one must not cost us the others.
        for cap in [
            FUSE_DO_READDIRPLUS,
            FUSE_ATOMIC_O_TRUNC,
            FUSE_PARALLEL_DIROPS,
            FUSE_CACHE_SYMLINKS,
        ] {
            if let Err(missing) = config.add_capabilities(cap) {
                debug!(missing, "kernel lacks FUSE capability");
            }
        }
        let _ = config.set_max_write(1 << 20);
        if let Err(nearest) = config.set_max_readahead(1 << 20) {
            let _ = config.set_max_readahead(nearest);
        }
        let _ = config.set_max_background(64);
        Ok(())
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name = name.to_owned();
        self.run(move |sh| match sh.op_lookup(parent, &name) {
            Ok((attr, ttl)) => reply.entry(&ttl, &attr, sh.generation()),
            Err(e) => reply.error(e),
        });
    }

    fn forget(&mut self, _req: &Request<'_>, ino: u64, nlookup: u64) {
        self.sh.op_forget(ino, nlookup);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let reply_with = move |sh: &Shared<B>, reply: ReplyAttr| match sh.op_getattr(ino) {
            Ok((attr, ttl)) => reply.attr(&ttl, &attr),
            Err(e) => reply.error(e),
        };
        // Replica reads are local and cheap: answer inline, unless an upload may be holding the
        // inode's write state.
        if self.sh.has_open_file(ino) {
            self.run(move |sh| reply_with(sh, reply));
        } else {
            reply_with(&self.sh, reply);
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let mtime_ns = mtime_ns_of(mtime);
        self.run(
            move |sh| match sh.op_setattr(ino, mode, uid, gid, size, mtime_ns) {
                Ok((attr, ttl)) => reply.attr(&ttl, &attr),
                Err(e) => reply.error(e),
            },
        );
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        match self.sh.op_readlink(ino) {
            Ok(t) => reply.data(&t),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let name = name.to_owned();
        self.run(move |sh| match sh.op_mkdir(parent, &name) {
            Ok((attr, ttl)) => reply.entry(&ttl, &attr, sh.generation()),
            Err(e) => reply.error(e),
        });
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name = name.to_owned();
        self.run(move |sh| match sh.op_unlink(parent, &name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        });
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name = name.to_owned();
        self.run(move |sh| match sh.op_rmdir(parent, &name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        });
    }

    fn symlink(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let name = link_name.to_owned();
        let target = target.to_owned();
        self.run(move |sh| match sh.op_symlink(parent, &name, &target) {
            Ok((attr, ttl)) => reply.entry(&ttl, &attr, sh.generation()),
            Err(e) => reply.error(e),
        });
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        let name = name.to_owned();
        let newname = newname.to_owned();
        self.run(
            move |sh| match sh.op_rename(parent, &name, newparent, &newname, flags) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            },
        );
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        // Never touches the network (content loads lazily at the first write): inline, which
        // saves a thread hand-off on every open.
        match self.sh.op_open(ino, flags) {
            Ok((fh, open_flags)) => reply.opened(fh, open_flags),
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        self.run(move |sh| match sh.op_read(ino, fh, offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        });
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let data = data.to_vec();
        self.run(move |sh| match sh.op_write(ino, fh, offset, &data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e),
        });
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        // close() of a clean descriptor (every read-only open) is a no-op: answer inline.
        if !self.sh.handle_may_upload(fh) {
            return reply.ok();
        }
        self.run(move |sh| match sh.op_flush(ino, fh) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        });
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if !self.sh.handle_may_upload(fh) {
            self.sh.op_release(ino, fh);
            return reply.ok();
        }
        self.run(move |sh| {
            sh.op_release(ino, fh);
            reply.ok();
        });
    }

    fn fsync(&mut self, _req: &Request<'_>, ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        self.run(move |sh| match sh.op_fsync(ino, fh) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        });
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.sh.op_opendir(ino) {
            Ok((fh, flags)) => reply.opened(fh, flags),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        self.run(move |sh| {
            let listing = match sh.dir_listing(ino, fh, offset) {
                Ok(l) => l,
                Err(e) => return reply.error(e),
            };
            let start = usize::try_from(offset).unwrap_or(0);
            let total = listing.items.len() + 2;
            for idx in start..total {
                let next = (idx + 1) as i64;
                let full = match idx {
                    0 => reply.add(ino, next, FileType::Directory, "."),
                    1 => reply.add(sh.parent_ino(ino), next, FileType::Directory, ".."),
                    _ => {
                        let item = &listing.items[idx - 2];
                        let child = lock(&sh.inodes).ino_for(item.entry.id);
                        reply.add(child, next, file_type_of(item), &item.display_name)
                    }
                };
                if full {
                    break;
                }
            }
            reply.ok();
        });
    }

    fn readdirplus(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectoryPlus,
    ) {
        self.run(move |sh| {
            let listing = match sh.dir_listing(ino, fh, offset) {
                Ok(l) => l,
                Err(e) => return reply.error(e),
            };
            let ttl = sh.ttl_since(listing.epoch);
            let gen = sh.generation();
            let dir_attr = match sh.dir_attr(ino) {
                Ok(a) => a,
                Err(e) => return reply.error(e),
            };
            let pid = match lock(&sh.inodes).id_of(ino) {
                Some(id) => id,
                None => return reply.error(libc::ESTALE),
            };
            let start = usize::try_from(offset).unwrap_or(0);
            let total = listing.items.len() + 2;
            for idx in start..total {
                let next = (idx + 1) as i64;
                let full = match idx {
                    // "." and ".." are never linked by the kernel (no lookup count).
                    0 => reply.add(ino, next, ".", &Duration::ZERO, &dir_attr, gen),
                    1 => {
                        let mut a = dir_attr;
                        a.ino = sh.parent_ino(ino);
                        reply.add(a.ino, next, "..", &Duration::ZERO, &a, gen)
                    }
                    _ => {
                        let item = &listing.items[idx - 2];
                        // Count the lookup before adding (the kernel links every entry it
                        // receives); undo it if the entry did not fit in this reply.
                        let child =
                            lock(&sh.inodes).remember(item.entry.id, pid, &item.display_name);
                        let attr = super::attr::attr_of(child, item, sh.opts.uid, sh.opts.gid);
                        let full = reply.add(child, next, &item.display_name, &ttl, &attr, gen);
                        if full {
                            lock(&sh.inodes).forget(child, 1);
                        }
                        full
                    }
                };
                if full {
                    break;
                }
            }
            reply.ok();
        });
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        self.sh.op_releasedir(fh);
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        // The VM's free space is not part of the protocol; report a large, plausible volume so
        // tools that check free space before writing do not refuse.
        const BLOCKS: u64 = 1 << 32;
        reply.statfs(
            BLOCKS,
            BLOCKS / 2,
            BLOCKS / 2,
            1 << 32,
            1 << 31,
            4096,
            255,
            4096,
        );
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        // Local only (the VM create is deferred to the first upload): answer inline.
        match self.sh.op_create(parent, name, mode, umask) {
            Ok((attr, fh)) => reply.created(&Duration::ZERO, &attr, self.sh.generation(), fh, 0),
            Err(e) => reply.error(e),
        }
    }
}
