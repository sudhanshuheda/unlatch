//! FUSE frontend tests against the in-memory backend: the operation layer directly, the
//! invalidation planner, and real kernel mounts (skipped when /dev/fuse or fusermount3 is
//! missing).

use super::mem::MemBackend;
use super::notify::Action;
use super::*;
use crate::inode::ROOT_INO;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::time::Instant;

fn opts(dir: &Path, ttl: Duration) -> FsOptions {
    FsOptions {
        ttl,
        prefetch: false,
        workers: 8,
        scratch_dir: dir.join("scratch"),
        unsynced_dir: dir.join("unsynced"),
        // SAFETY: getuid/getgid cannot fail.
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
    }
}

struct Env {
    mem: Arc<MemBackend>,
    sh: Arc<Shared<MemBackend>>,
    queue: EventQueue,
    _tmp: tempfile::TempDir,
}

fn env(ttl: Duration) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let mem = Arc::new(MemBackend::new());
    let (sink, queue) = event_channel();
    sink.set_live(true);
    mem.set_sink(sink.clone());
    let sh = Shared::new(Arc::clone(&mem), opts(tmp.path(), ttl), sink).unwrap();
    Env {
        mem,
        sh,
        queue,
        _tmp: tmp,
    }
}

impl Env {
    fn drain(&self) -> Vec<Inval> {
        let mut v = Vec::new();
        while let Ok(i) = self.queue.rx.try_recv() {
            v.push(i);
        }
        v
    }
    fn plan(&self) -> Vec<Action> {
        let batch = self.drain();
        self.sh.plan(batch)
    }
}

const HOUR: Duration = Duration::from_secs(3600);

// ---- operation layer ----------------------------------------------------------------------

fn unsynced_files(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| {
                    (
                        e.file_name().into_string().unwrap(),
                        fs::read(e.path()).unwrap(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Unmounting while a descriptor still holds acknowledged writes uploads them (or, if the VM is
/// unreachable, keeps them in unsynced/); once stopping, further writes are refused instead of
/// acknowledged and dropped.
#[test]
fn stopping_uploads_dirty_open_files_and_refuses_new_writes() {
    let e = env(HOUR);
    e.mem.vm_write("", "log.txt", b"old\n");
    let (a, _) = e.sh.op_lookup(ROOT_INO, OsStr::new("log.txt")).unwrap();
    let (fh, _) =
        e.sh.op_open(a.ino, libc::O_WRONLY | libc::O_APPEND)
            .unwrap();
    e.sh.op_write(a.ino, fh, 4, b"USER-APPEND").unwrap();
    // A new file, created and written, never closed.
    let (b, fh2) =
        e.sh.op_create(ROOT_INO, OsStr::new("new.txt"), 0o100644, 0o022)
            .unwrap();
    e.sh.op_write(b.ino, fh2, 0, b"fresh").unwrap();
    e.sh.stop_and_flush();
    assert_eq!(e.mem.vm_content("log.txt").unwrap(), b"old\nUSER-APPEND");
    assert_eq!(e.mem.vm_content("new.txt").unwrap(), b"fresh");
    assert!(
        e.sh.op_write(a.ino, fh, 0, b"late").is_err(),
        "not acknowledged"
    );
    assert!(e
        .sh
        .op_create(ROOT_INO, OsStr::new("late.txt"), 0o100644, 0o022)
        .is_err());
    e.sh.op_release(a.ino, fh);
    e.sh.op_release(b.ino, fh2);
    assert_eq!(e.mem.vm_content("log.txt").unwrap(), b"old\nUSER-APPEND");
}

#[test]
fn stopping_while_the_vm_is_unreachable_keeps_content_in_unsynced() {
    let e = env(HOUR);
    e.mem.vm_write("", "log.txt", b"old\n");
    let (a, _) = e.sh.op_lookup(ROOT_INO, OsStr::new("log.txt")).unwrap();
    let (fh, _) =
        e.sh.op_open(a.ino, libc::O_WRONLY | libc::O_APPEND)
            .unwrap();
    e.sh.op_write(a.ino, fh, 4, b"USER-APPEND").unwrap();
    e.mem.fail_uploads.store(true, Ordering::SeqCst);
    e.sh.stop_and_flush();
    assert_eq!(e.mem.vm_content("log.txt").unwrap(), b"old\n");
    let kept = unsynced_files(&e.sh.opts.unsynced_dir);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(kept[0].0.ends_with("-log.txt"));
    assert_eq!(kept[0].1, b"old\nUSER-APPEND");
}

/// `unlatch mount` killed (crash, SIGKILL) with dirty scratch content: the next start must not
/// delete the only copy of acknowledged writes; it moves it to unsynced/. Clean scratch files
/// (plain reads/loads) are still cleared.
#[test]
fn dirty_scratch_of_a_crashed_run_is_recovered_into_unsynced() {
    let tmp = tempfile::tempdir().unwrap();
    let mem = Arc::new(MemBackend::new());
    mem.vm_write("", "log.txt", b"old\n");
    mem.vm_write("", "clean.txt", b"c");
    {
        let (sink, _queue) = event_channel();
        sink.set_live(true);
        let sh = Shared::new(Arc::clone(&mem), opts(tmp.path(), HOUR), sink).unwrap();
        let (a, _) = sh.op_lookup(ROOT_INO, OsStr::new("log.txt")).unwrap();
        let (fh, _) = sh.op_open(a.ino, libc::O_WRONLY | libc::O_APPEND).unwrap();
        sh.op_write(a.ino, fh, 4, b"USER-APPEND").unwrap();
        // A clean loaded scratch (opened for write, never written).
        let (c, _) = sh.op_lookup(ROOT_INO, OsStr::new("clean.txt")).unwrap();
        let (fh2, _) = sh.op_open(c.ino, libc::O_RDWR).unwrap();
        assert_eq!(sh.op_read(c.ino, fh2, 0, 10).unwrap(), b"c");
        // Crash: no flush, no release, no destructors.
        std::mem::forget(sh);
    }
    let scratch = tmp.path().join("scratch");
    assert!(fs::read_dir(&scratch).unwrap().count() >= 1);
    let (sink, _queue) = event_channel();
    let _sh = Shared::new(Arc::clone(&mem), opts(tmp.path(), HOUR), sink).unwrap();
    assert_eq!(
        fs::read_dir(&scratch).unwrap().count(),
        0,
        "scratch cleared"
    );
    let kept = unsynced_files(&tmp.path().join("unsynced"));
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(kept[0].0.ends_with("-log.txt"), "{kept:?}");
    assert_eq!(kept[0].1, b"old\nUSER-APPEND");
}

#[test]
fn negative_lookup_is_cached_unless_a_change_raced() {
    let e = env(HOUR);
    let (a, ttl) = e.sh.op_lookup(ROOT_INO, OsStr::new("missing")).unwrap();
    assert_eq!(a.ino, 0, "negative entry");
    assert_eq!(ttl, HOUR);
    // A change between our read of the epoch and the reply must zero the TTL.
    let e0 = e.sh.epoch();
    e.mem.vm_write("", "other", b"x");
    assert_eq!(e.sh.ttl_since(e0), Duration::ZERO);
}

#[test]
fn negative_lookups_are_not_cached_before_the_engine_is_live() {
    let e = env(HOUR);
    e.sh.sink.set_live(false);
    assert_eq!(
        e.sh.op_lookup(ROOT_INO, OsStr::new("missing")).unwrap_err(),
        libc::ENOENT
    );
}

#[test]
fn root_getattr_before_first_sync_is_an_uncached_empty_dir() {
    let e = env(HOUR);
    e.mem.vm_remove_root_for_test();
    let (a, ttl) = e.sh.op_getattr(ROOT_INO).unwrap();
    assert_eq!(a.kind, fuser::FileType::Directory);
    assert_eq!(ttl, Duration::ZERO);
}

#[test]
fn lookup_remembers_and_forget_releases() {
    let e = env(HOUR);
    let id = e.mem.vm_write("", "f.txt", b"hello");
    let (a, ttl) = e.sh.op_lookup(ROOT_INO, OsStr::new("f.txt")).unwrap();
    assert_eq!(ttl, HOUR);
    assert_eq!(a.size, 5);
    assert_eq!(lock(&e.sh.inodes).id_of(a.ino), Some(id));
    e.sh.op_forget(a.ino, 1);
    assert_eq!(lock(&e.sh.inodes).id_of(a.ino), None);
}

#[test]
fn new_file_is_created_once_at_close_with_its_content() {
    let e = env(HOUR);
    let (a, fh) =
        e.sh.op_create(ROOT_INO, OsStr::new("new.txt"), 0o100644, 0o022)
            .unwrap();
    assert_eq!(
        e.mem.creates.load(Ordering::SeqCst),
        0,
        "create is deferred"
    );
    // Visible to lookups by name before upload.
    let (la, _) = e.sh.op_lookup(ROOT_INO, OsStr::new("new.txt")).unwrap();
    assert_eq!(la.ino, a.ino);
    e.sh.op_write(a.ino, fh, 0, b"abc").unwrap();
    assert_eq!(e.sh.op_getattr(a.ino).unwrap().0.size, 3);
    e.sh.op_flush(a.ino, fh).unwrap();
    e.sh.op_release(a.ino, fh);
    assert_eq!(e.mem.creates.load(Ordering::SeqCst), 1);
    assert_eq!(e.mem.modifies.load(Ordering::SeqCst), 0);
    assert_eq!(e.mem.vm_content("new.txt").unwrap(), b"abc");
    assert_eq!(lock(&e.sh.inodes).id_of(a.ino), e.mem.vm_path("new.txt"));
}

#[test]
fn unlink_before_close_never_reaches_the_vm() {
    let e = env(HOUR);
    let (a, fh) =
        e.sh.op_create(ROOT_INO, OsStr::new("tmp.swp"), 0o100600, 0)
            .unwrap();
    e.sh.op_write(a.ino, fh, 0, b"junk").unwrap();
    e.sh.op_unlink(ROOT_INO, OsStr::new("tmp.swp")).unwrap();
    e.sh.op_flush(a.ino, fh).unwrap();
    e.sh.op_release(a.ino, fh);
    assert_eq!(e.mem.creates.load(Ordering::SeqCst), 0);
    assert!(e.mem.vm_names("").is_empty());
}

#[test]
fn stale_base_becomes_conflict_copy_and_vm_content_wins() {
    let e = env(HOUR);
    e.mem.vm_write("", "doc.txt", b"base");
    let (a, _) = e.sh.op_lookup(ROOT_INO, OsStr::new("doc.txt")).unwrap();
    let (fh, _) = e.sh.op_open(a.ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
    e.mem.vm_write("", "doc.txt", b"from the agent");
    e.sh.op_write(a.ino, fh, 0, b"from the mac").unwrap();
    e.sh.op_flush(a.ino, fh).unwrap();
    e.sh.op_release(a.ino, fh);
    assert_eq!(e.mem.vm_content("doc.txt").unwrap(), b"from the agent");
    let names = e.mem.vm_names("");
    let copy = names
        .iter()
        .find(|n| n.contains("conflict"))
        .expect("conflict copy");
    assert_eq!(e.mem.vm_content(copy).unwrap(), b"from the mac");
    assert!(
        e.drain().contains(&Inval::Inode { ino: a.ino }),
        "page cache of the loser must be dropped"
    );
}

#[test]
fn non_truncating_write_loads_current_content_first() {
    let e = env(HOUR);
    e.mem.vm_write("", "log", b"0123456789");
    let (a, _) = e.sh.op_lookup(ROOT_INO, OsStr::new("log")).unwrap();
    let (fh, _) = e.sh.op_open(a.ino, libc::O_RDWR).unwrap();
    e.sh.op_write(a.ino, fh, 2, b"ab").unwrap();
    assert_eq!(e.sh.op_read(a.ino, fh, 0, 100).unwrap(), b"01ab456789");
    e.sh.op_release(a.ino, fh);
    assert_eq!(e.mem.vm_content("log").unwrap(), b"01ab456789");
}

#[test]
fn rename_replaces_an_existing_destination() {
    let e = env(HOUR);
    e.mem.vm_write("", "a", b"new");
    e.mem.vm_write("", "b", b"old");
    e.sh.op_rename(ROOT_INO, OsStr::new("a"), ROOT_INO, OsStr::new("b"), 0)
        .unwrap();
    assert_eq!(e.mem.vm_names(""), vec!["b"]);
    assert_eq!(e.mem.vm_content("b").unwrap(), b"new");
    e.mem.vm_write("", "c", b"c");
    assert_eq!(
        e.sh.op_rename(
            ROOT_INO,
            OsStr::new("b"),
            ROOT_INO,
            OsStr::new("c"),
            libc::RENAME_NOREPLACE
        ),
        Err(libc::EEXIST)
    );
    e.mem.vm_mkdir("", "d");
    assert_eq!(
        e.sh.op_rename(ROOT_INO, OsStr::new("b"), ROOT_INO, OsStr::new("d"), 0),
        Err(libc::EISDIR)
    );
}

#[test]
fn rmdir_of_non_empty_dir_is_enotempty() {
    let e = env(HOUR);
    e.mem.vm_mkdir("", "d");
    e.mem.vm_write("d", "f", b"x");
    assert_eq!(
        e.sh.op_rmdir(ROOT_INO, OsStr::new("d")),
        Err(libc::ENOTEMPTY)
    );
    assert_eq!(e.sh.op_unlink(ROOT_INO, OsStr::new("d")), Err(libc::EISDIR));
}

#[test]
fn truncate_by_path_uploads_immediately() {
    let e = env(HOUR);
    e.mem.vm_write("", "t", b"abcdef");
    let (a, _) = e.sh.op_lookup(ROOT_INO, OsStr::new("t")).unwrap();
    let (attr, _) =
        e.sh.op_setattr(a.ino, None, None, None, Some(2), None)
            .unwrap();
    assert_eq!(attr.size, 2);
    assert_eq!(e.mem.vm_content("t").unwrap(), b"ab");
}

#[test]
fn chmod_exec_maps_to_user_exec_and_foreign_chown_is_refused() {
    let e = env(HOUR);
    e.mem.vm_write("", "run.sh", b"#!/bin/sh\n");
    let (a, _) = e.sh.op_lookup(ROOT_INO, OsStr::new("run.sh")).unwrap();
    let (attr, _) =
        e.sh.op_setattr(a.ino, Some(0o755), None, None, None, None)
            .unwrap();
    assert_eq!(attr.perm & 0o100, 0o100);
    let uid = e.sh.opts.uid;
    assert_eq!(
        e.sh.op_setattr(a.ino, None, Some(uid.wrapping_add(1)), None, None, None),
        Err(libc::EPERM)
    );
}

#[test]
fn mkdir_collision_is_eexist_and_leaves_nothing_behind() {
    let e = env(HOUR);
    e.mem.vm_mkdir("", "d");
    assert_eq!(
        e.sh.op_mkdir(ROOT_INO, OsStr::new("d")).unwrap_err(),
        libc::EEXIST
    );
    assert_eq!(e.mem.vm_names(""), vec!["d"]);
}

#[test]
fn invalid_names_are_rejected() {
    let e = env(HOUR);
    assert_eq!(
        e.sh.op_mkdir(ROOT_INO, OsStr::new("..")).unwrap_err(),
        libc::EINVAL
    );
    assert_eq!(
        e.sh.op_create(ROOT_INO, OsStr::new("a/b"), 0o644, 0)
            .unwrap_err(),
        libc::EINVAL
    );
}

// ---- invalidation planning ----------------------------------------------------------------

#[test]
fn vm_rename_invalidates_old_and_new_names() {
    let e = env(HOUR);
    e.mem.vm_mkdir("", "d");
    e.mem.vm_write("", "f", b"x");
    let (d, _) = e.sh.op_lookup(ROOT_INO, OsStr::new("d")).unwrap();
    let (f, _) = e.sh.op_lookup(ROOT_INO, OsStr::new("f")).unwrap();
    e.drain();
    e.mem.vm_rename("f", "d", "g");
    let actions = e.plan();
    assert!(
        actions.contains(&Action::Entry {
            parent: ROOT_INO,
            name: "f".into()
        }),
        "{actions:?}"
    );
    assert!(
        actions.contains(&Action::Entry {
            parent: d.ino,
            name: "g".into()
        }),
        "{actions:?}"
    );
    assert!(actions.contains(&Action::Inode {
        ino: ROOT_INO,
        data: true
    }));
    assert!(actions.contains(&Action::Inode {
        ino: d.ino,
        data: true
    }));
    // Never opened, so no pages to drop: attributes only.
    assert!(
        actions.contains(&Action::Inode {
            ino: f.ino,
            data: false
        }),
        "{actions:?}"
    );
}

#[test]
fn new_vm_file_kills_negative_dentry_and_listing() {
    let e = env(HOUR);
    e.sh.op_lookup(ROOT_INO, OsStr::new("later.txt")).unwrap();
    assert!(e.sh.root_ready.load(Ordering::SeqCst));
    e.drain();
    e.mem.vm_write("", "later.txt", b"x");
    let actions = e.plan();
    assert!(
        actions.contains(&Action::Entry {
            parent: ROOT_INO,
            name: "later.txt".into()
        }),
        "{actions:?}"
    );
    assert!(actions.contains(&Action::Inode {
        ino: ROOT_INO,
        data: true
    }));
}

#[test]
fn content_change_drops_pages_but_own_upload_does_not() {
    let e = env(HOUR);
    e.mem.vm_write("", "f", b"one");
    let (f, _) = e.sh.op_lookup(ROOT_INO, OsStr::new("f")).unwrap();
    let (fh, _) = e.sh.op_open(f.ino, libc::O_RDONLY).unwrap();
    e.sh.op_release(f.ino, fh);
    e.drain();
    e.mem.vm_write("", "f", b"two!");
    assert!(e.plan().contains(&Action::Inode {
        ino: f.ino,
        data: true
    }));

    let (fh, _) = e.sh.op_open(f.ino, libc::O_WRONLY | libc::O_TRUNC).unwrap();
    e.sh.op_write(f.ino, fh, 0, b"mine").unwrap();
    e.sh.op_release(f.ino, fh);
    let actions = e.plan();
    assert!(
        actions.contains(&Action::Inode {
            ino: f.ino,
            data: false
        }),
        "{actions:?}"
    );
    assert!(
        !actions.contains(&Action::Inode {
            ino: f.ino,
            data: true
        }),
        "{actions:?}"
    );
}

#[test]
fn root_inode_is_not_invalidated_before_the_kernel_used_its_attributes() {
    // An inval_inode racing the kernel's first GETATTR of the root makes it drop the reply and
    // check permissions against the placeholder root mode 0 → EACCES (seen on 6.8).
    let e = env(HOUR);
    e.mem.vm_write("", "x", b"x");
    assert!(!e.plan().contains(&Action::Inode {
        ino: ROOT_INO,
        data: true
    }));
    e.sh.op_opendir(ROOT_INO).unwrap();
    e.mem.vm_write("", "y", b"y");
    assert!(e.plan().contains(&Action::Inode {
        ino: ROOT_INO,
        data: true
    }));
}

#[test]
fn vm_delete_invalidates_the_name() {
    let e = env(HOUR);
    e.mem.vm_write("", "gone", b"x");
    let (g, _) = e.sh.op_lookup(ROOT_INO, OsStr::new("gone")).unwrap();
    e.drain();
    e.mem.vm_remove("gone");
    let actions = e.plan();
    assert!(actions.contains(&Action::Entry {
        parent: ROOT_INO,
        name: "gone".into()
    }));
    assert!(actions
        .iter()
        .any(|a| matches!(a, Action::Inode { ino, .. } if *ino == g.ino)));
}

#[test]
fn reimport_drops_every_dentry_and_mapping() {
    let e = env(HOUR);
    e.mem.vm_write("", "a", b"x");
    let (a, _) = e.sh.op_lookup(ROOT_INO, OsStr::new("a")).unwrap();
    let actions = e.sh.plan(vec![Inval::Reimport]);
    assert!(actions.contains(&Action::Entry {
        parent: ROOT_INO,
        name: "a".into()
    }));
    assert_eq!(e.sh.op_getattr(a.ino).unwrap_err(), libc::ESTALE);
    assert_eq!(e.sh.generation(), 1);
}

// ---- real kernel mounts -------------------------------------------------------------------

fn fuse_available() -> bool {
    Path::new("/dev/fuse").exists()
        && std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .any(|d| d.join("fusermount3").exists() || d.join("fusermount").exists())
            })
            .unwrap_or(false)
}

struct MountEnv {
    mem: Arc<MemBackend>,
    mnt: PathBuf,
    mounted: Option<Mounted<MemBackend>>,
    _tmp: tempfile::TempDir,
}

impl Drop for MountEnv {
    fn drop(&mut self) {
        if let Some(m) = self.mounted.take() {
            m.unmount();
        }
    }
}

fn mount_env(ttl: Duration) -> Option<MountEnv> {
    if !fuse_available() {
        eprintln!("skipping: FUSE not available");
        return None;
    }
    let tmp = tempfile::tempdir().unwrap();
    let mnt = tmp.path().join("mnt");
    fs::create_dir(&mnt).unwrap();
    let mem = Arc::new(MemBackend::new());
    let (sink, queue) = event_channel();
    sink.set_live(true);
    mem.set_sink(sink.clone());
    let mounted = mount(
        Arc::clone(&mem),
        opts(tmp.path(), ttl),
        sink,
        queue,
        &mnt,
        "unlatch:test",
    )
    .unwrap();
    Some(MountEnv {
        mem,
        mnt,
        mounted: Some(mounted),
        _tmp: tmp,
    })
}

/// Poll (1 ms) until `f` holds; returns the elapsed time or panics after `limit`.
fn eventually(limit: Duration, what: &str, mut f: impl FnMut() -> bool) -> Duration {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
    t0.elapsed()
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

#[test]
fn mount_basic_operations() {
    let Some(m) = mount_env(HOUR) else { return };
    m.mem.vm_write("", "hello.txt", b"hi there");
    m.mem.vm_mkdir("", "src");
    assert_eq!(names(&m.mnt), vec!["hello.txt", "src"]);
    assert_eq!(
        fs::read_to_string(m.mnt.join("hello.txt")).unwrap(),
        "hi there"
    );

    fs::write(m.mnt.join("new.txt"), "abc").unwrap();
    assert_eq!(m.mem.vm_content("new.txt").unwrap(), b"abc");
    assert_eq!(
        m.mem.creates.load(Ordering::SeqCst),
        1,
        "one create, no extra modify"
    );
    assert_eq!(m.mem.modifies.load(Ordering::SeqCst), 0);

    fs::rename(m.mnt.join("new.txt"), m.mnt.join("src/moved.txt")).unwrap();
    assert_eq!(m.mem.vm_names("src"), vec!["moved.txt"]);
    assert_eq!(
        fs::read_to_string(m.mnt.join("src/moved.txt")).unwrap(),
        "abc"
    );
    assert!(!m.mnt.join("new.txt").exists());

    // Editor-style save: write temp, rename over.
    fs::write(m.mnt.join("src/.moved.txt.tmp"), "v2").unwrap();
    fs::rename(
        m.mnt.join("src/.moved.txt.tmp"),
        m.mnt.join("src/moved.txt"),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(m.mnt.join("src/moved.txt")).unwrap(),
        "v2"
    );
    assert_eq!(m.mem.vm_names("src"), vec!["moved.txt"]);

    fs::remove_file(m.mnt.join("src/moved.txt")).unwrap();
    assert!(m.mem.vm_names("src").is_empty());
    fs::create_dir(m.mnt.join("d")).unwrap();
    assert!(m.mem.vm_path("d").is_some());
    fs::remove_dir(m.mnt.join("d")).unwrap();
    assert!(m.mem.vm_path("d").is_none());

    std::os::unix::fs::symlink("hello.txt", m.mnt.join("link")).unwrap();
    assert_eq!(
        fs::read_link(m.mnt.join("link")).unwrap(),
        PathBuf::from("hello.txt")
    );
    assert_eq!(fs::read_to_string(m.mnt.join("link")).unwrap(), "hi there");

    // Append through a descriptor keeps existing content.
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(m.mnt.join("hello.txt"))
        .unwrap();
    f.write_all(b"!").unwrap();
    drop(f);
    assert_eq!(m.mem.vm_content("hello.txt").unwrap(), b"hi there!");

    // chmod +x.
    fs::set_permissions(m.mnt.join("hello.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        fs::metadata(m.mnt.join("hello.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o100,
        0o100
    );

    // ftruncate through a descriptor, uploaded at close.
    let f = fs::OpenOptions::new()
        .write(true)
        .open(m.mnt.join("hello.txt"))
        .unwrap();
    f.set_len(2).unwrap();
    drop(f);
    assert_eq!(m.mem.vm_content("hello.txt").unwrap(), b"hi");

    // Empty file via create + close.
    drop(fs::File::create(m.mnt.join("empty")).unwrap());
    assert_eq!(m.mem.vm_content("empty").unwrap(), b"");
}

#[test]
fn mount_vm_changes_are_pushed_despite_hour_long_ttls() {
    let Some(m) = mount_env(HOUR) else { return };
    m.mem.vm_write("", "f.txt", b"one");
    assert_eq!(fs::read_to_string(m.mnt.join("f.txt")).unwrap(), "one");
    assert!(
        fs::metadata(m.mnt.join("later.txt")).is_err(),
        "negative dentry now cached"
    );
    let _ = names(&m.mnt); // cache the listing

    m.mem.vm_write("", "f.txt", b"two, longer");
    let t = eventually(Duration::from_secs(2), "content change", || {
        fs::read_to_string(m.mnt.join("f.txt"))
            .map(|s| s == "two, longer")
            .unwrap_or(false)
    });
    eprintln!("content change visible after {t:?}");

    m.mem.vm_write("", "later.txt", b"hello");
    eventually(Duration::from_secs(2), "new file", || {
        m.mnt.join("later.txt").exists()
    });
    eventually(Duration::from_secs(2), "listing", || {
        names(&m.mnt).contains(&"later.txt".to_string())
    });

    m.mem.vm_rename("later.txt", "", "renamed.txt");
    eventually(Duration::from_secs(2), "rename", || {
        !m.mnt.join("later.txt").exists() && m.mnt.join("renamed.txt").exists()
    });

    m.mem.vm_remove("renamed.txt");
    eventually(Duration::from_secs(2), "delete", || {
        !m.mnt.join("renamed.txt").exists()
    });
    assert_eq!(names(&m.mnt), vec!["f.txt"]);
}

#[test]
fn mount_caches_listings_and_pages_until_invalidated() {
    let Some(m) = mount_env(HOUR) else { return };
    for i in 0..50 {
        m.mem
            .vm_write("", &format!("f{i:02}"), format!("content {i}").as_bytes());
    }
    // Let the 50 creation events drain first: each one (correctly) drops the cached listing.
    eventually(Duration::from_secs(2), "listing", || {
        names(&m.mnt).len() == 50
    });
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(names(&m.mnt).len(), 50);
    let lists = m.mem.lists.load(Ordering::SeqCst);
    assert_eq!(names(&m.mnt).len(), 50);
    assert_eq!(
        m.mem.lists.load(Ordering::SeqCst),
        lists,
        "second listing served from the kernel cache"
    );

    assert_eq!(fs::read_to_string(m.mnt.join("f07")).unwrap(), "content 7");
    let reads = m.mem.reads.load(Ordering::SeqCst);
    assert_eq!(fs::read_to_string(m.mnt.join("f07")).unwrap(), "content 7");
    assert_eq!(
        m.mem.reads.load(Ordering::SeqCst),
        reads,
        "second read served from the page cache (KEEP_CACHE)"
    );

    m.mem.vm_write("", "f07", b"changed");
    eventually(Duration::from_secs(2), "changed content", || {
        fs::read_to_string(m.mnt.join("f07"))
            .map(|s| s == "changed")
            .unwrap_or(false)
    });
}

#[test]
fn mount_conflicting_edit_keeps_both_versions() {
    let Some(m) = mount_env(HOUR) else { return };
    m.mem.vm_write("", "notes.md", b"base");
    assert_eq!(fs::read_to_string(m.mnt.join("notes.md")).unwrap(), "base");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(m.mnt.join("notes.md"))
        .unwrap();
    m.mem.vm_write("", "notes.md", b"agent edit");
    f.write_all(b"mac edit").unwrap();
    drop(f);
    assert_eq!(m.mem.vm_content("notes.md").unwrap(), b"agent edit");
    let copy = m
        .mem
        .vm_names("")
        .into_iter()
        .find(|n| n.contains("conflict"))
        .expect("conflict copy on the VM");
    assert_eq!(m.mem.vm_content(&copy).unwrap(), b"mac edit");
    eventually(
        Duration::from_secs(2),
        "local view converges to the VM",
        || {
            fs::read_to_string(m.mnt.join("notes.md"))
                .map(|s| s == "agent edit")
                .unwrap_or(false)
        },
    );
}

/// Real kernel mount: a process holds a file open with an acknowledged append when the mount
/// stops (SIGTERM path = `Mounted::unmount`). The bytes must reach the VM, and writes through
/// the still-open descriptor afterwards must fail rather than be acknowledged and dropped.
#[test]
fn mount_unmount_with_an_open_dirty_file_uploads_it() {
    let Some(mut m) = mount_env(HOUR) else { return };
    m.mem.vm_write("", "log.txt", b"old\n");
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(m.mnt.join("log.txt"))
        .unwrap();
    f.write_all(b"USER-APPEND").unwrap();
    f.flush().unwrap();
    m.mounted.take().unwrap().unmount();
    assert_eq!(m.mem.vm_content("log.txt").unwrap(), b"old\nUSER-APPEND");
    assert!(
        f.write_all(b"late").is_err(),
        "a write after unmount must not be acknowledged"
    );
    drop(f);
    assert_eq!(m.mem.vm_content("log.txt").unwrap(), b"old\nUSER-APPEND");
}

#[test]
fn mount_large_file_round_trip() {
    let Some(m) = mount_env(HOUR) else { return };
    let data: Vec<u8> = (0..3_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    fs::write(m.mnt.join("big.bin"), &data).unwrap();
    assert_eq!(m.mem.vm_content("big.bin").unwrap(), data);
    let mut back = Vec::new();
    fs::File::open(m.mnt.join("big.bin"))
        .unwrap()
        .read_to_end(&mut back)
        .unwrap();
    assert_eq!(back, data);
}
