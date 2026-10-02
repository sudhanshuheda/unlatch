//! Engine tests against the in-memory fake unlatchd (testkit).

use super::testkit::{FakeServer, ROOT_PATH};
use super::{Inner, Timing};
use crate::*;
use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;
use unlatch_proto::ipc::{fields, ConnState, LocalMeta};
use unlatch_proto::wire::{ClientMsg, Request};
use unlatch_proto::{BaseVersion, IndexId, Kind};

struct H {
    fake: FakeServer,
    engine: Engine,
    events: Arc<Mutex<Vec<EngineEvent>>>,
    /// The host takes this long (ms) to handle each event: a busy main thread in the
    /// extension, or a loaded CI runner.
    slow_host_ms: Arc<std::sync::atomic::AtomicU64>,
    dir: tempfile::TempDir,
}

fn timing() -> Timing {
    Timing {
        ping_interval: Duration::from_millis(100),
        dead_after: Duration::from_millis(500),
        backoff_min: Duration::from_millis(10),
        backoff_max: Duration::from_millis(100),
        welcome_timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(5),
        reply_timeout: Duration::from_secs(5),
    }
}

fn cfg(dir: &Path) -> EngineConfig {
    let mut c = EngineConfig::new(
        "test",
        Transport::Command {
            argv: vec![],
            env: vec![],
        },
        ROOT_PATH,
        dir.join("state"),
        "My Mac",
    );
    c.list_timeout = Duration::from_secs(5);
    c.prefetch.max_file = 0;
    c
}

fn start_in(fake: &FakeServer, dir: tempfile::TempDir, tweak: impl FnOnce(&mut EngineConfig)) -> H {
    let mut c = cfg(dir.path());
    tweak(&mut c);
    let events: Arc<Mutex<Vec<EngineEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let slow_host_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let ev = events.clone();
    let slow = slow_host_ms.clone();
    let handler: EventHandler = Arc::new(move |e| {
        let ms = slow.load(std::sync::atomic::Ordering::Acquire);
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
        ev.lock().unwrap_or_else(|p| p.into_inner()).push(e)
    });
    let inner = Inner::start_with(c, Some(handler), fake.opener(), timing()).expect("start");
    H {
        fake: fake.clone(),
        engine: Engine { inner },
        events,
        slow_host_ms,
        dir,
    }
}

fn start(fake: &FakeServer) -> H {
    let h = start_in(fake, tempfile::tempdir().expect("tempdir"), |_| {});
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    h
}

impl H {
    fn restart(self) -> H {
        let H {
            fake, engine, dir, ..
        } = self;
        engine.shutdown();
        drop(engine);
        start_in(&fake, dir, |_| {})
    }

    fn barrier(&self) {
        self.engine
            .server_barrier(Duration::from_secs(5))
            .expect("barrier");
    }

    fn id(&self, path: &str) -> ItemId {
        self.fake.with(|fs| fs.resolve(path)).expect("path")
    }

    fn names(&self, dir: ItemId) -> Vec<String> {
        let p = self.engine.list(dir, None, 1000, false).expect("list");
        p.items.into_iter().map(|i| i.display_name).collect()
    }

    fn events(&self) -> Vec<EngineEvent> {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn clear_events(&self) {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    fn requests(&self, pred: impl Fn(&Request) -> bool) -> usize {
        self.fake.with(|fs| fs.count(pred))
    }

    fn mutating_requests(&self) -> usize {
        self.requests(|r| {
            !matches!(
                r,
                Request::Ping { .. }
                    | Request::Stat { .. }
                    | Request::ListDir { .. }
                    | Request::Read { .. }
            )
        })
    }

    fn file(&self, content: &[u8]) -> std::fs::File {
        let p = self
            .dir
            .path()
            .join(format!("src-{}", rand::random::<u64>()));
        let mut f = std::fs::File::create(&p).expect("create");
        f.write_all(content).expect("write");
        std::fs::File::open(&p).expect("open")
    }

    fn create_file(
        &self,
        parent: ItemId,
        name: &str,
        content: &[u8],
        template: &str,
    ) -> Result<Modified> {
        self.engine.create(CreateRequest {
            template_id: template.into(),
            parent,
            name: name.into(),
            kind: CreateKind::File,
            content: Some(self.file(content)),
            symlink_target: None,
            mtime_ns: None,
            user_exec: None,
            changed_fields: fields::CONTENTS | fields::FILENAME,
            local: LocalMeta::default(),
            may_already_exist: false,
            deletion_conflicted: false,
        })
    }

    fn fetch(&self, id: ItemId) -> Vec<u8> {
        let d = self.dir.path().join("fetch");
        std::fs::create_dir_all(&d).expect("mkdir");
        let f = self
            .engine
            .fetch(id, None, &d, &|_, _| {}, &CancelToken::new())
            .expect("fetch");
        std::fs::read(&f.path).expect("read")
    }

    fn wait_for(&self, what: &str, f: impl Fn() -> bool) {
        let t = Instant::now();
        while !f() {
            assert!(
                t.elapsed() < Duration::from_secs(10),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn changes_all(e: &Engine, anchor: &[u8]) -> Changes {
    let mut a = anchor.to_vec();
    let mut all = Changes {
        updated: vec![],
        removed: vec![],
        anchor: vec![],
        more: false,
    };
    loop {
        let c = e.changes_since(&a, 2).expect("changes");
        all.updated.extend(c.updated);
        all.removed.extend(c.removed);
        a = c.anchor.clone();
        if !c.more {
            all.anchor = a;
            return all;
        }
    }
}

fn upd_names(c: &Changes) -> Vec<String> {
    c.updated.iter().map(|i| i.display_name.clone()).collect()
}

fn tree(fake: &FakeServer) {
    fake.with(|fs| {
        fs.vm_mkdir("src");
        fs.vm_write("src/main.rs", b"fn main() {}");
        fs.vm_write("src/lib.rs", b"pub fn x() {}");
        fs.vm_write("README.md", b"# hi");
        fs.vm_mkdir("docs");
        fs.vm_write("docs/a.txt", b"a");
    });
}

#[test]
fn snapshot_list_item_lookup_and_offline_restart() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    assert_eq!(h.names(ItemId::ROOT), vec!["README.md", "docs", "src"]);
    let src = h.id("src");
    assert_eq!(h.names(src), vec!["lib.rs", "main.rs"]);
    let it = h.engine.lookup(src, "main.rs").expect("lookup");
    assert_eq!(it.entry.kind, Kind::File);
    assert_eq!(h.engine.item(it.entry.id).expect("item"), it);
    assert!(matches!(h.engine.lookup(src, "nope"), Err(e) if e.code == ErrorCode::NotFound));
    assert_eq!(h.engine.status().state, ConnState::Live);
    // Offline restart: the replica serves immediately with no connection.
    fake.with(|fs| fs.faults.connect_error = Some(err(ErrorCode::Offline, "no route")));
    let h = h.restart();
    let t = Instant::now();
    assert_eq!(h.names(ItemId::ROOT), vec!["README.md", "docs", "src"]);
    assert!(t.elapsed() < Duration::from_secs(1));
    assert_eq!(h.engine.item(src).expect("item").display_name, "src");
    h.wait_for("offline state", || {
        matches!(h.engine.status().state, ConnState::Offline { .. })
    });
}

#[test]
fn resume_replays_changes_made_while_away() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let before = h.fake.with(|fs| fs.sessions);
    let H {
        fake, engine, dir, ..
    } = h;
    engine.shutdown();
    drop(engine);
    fake.with(|fs| {
        fs.vm_write("new.txt", b"new");
        fs.vm_rm("docs");
    });
    let h = start_in(&fake, dir, |_| {});
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    assert_eq!(h.fake.with(|fs| fs.sessions), before + 1);
    let last = h.fake.with(|fs| fs.hellos.last().cloned()).expect("hello");
    match last {
        ClientMsg::Hello {
            resume,
            expect_index,
            client_name,
            ..
        } => {
            assert!(resume.is_some(), "resumes from the persisted (index, seq)");
            assert!(expect_index.is_some());
            assert_eq!(client_name, "My Mac");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(h.names(ItemId::ROOT), vec!["README.md", "new.txt", "src"]);
}

#[test]
fn vm_change_is_committed_then_signalled() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let a0 = h.engine.anchor();
    h.clear_events();
    let id = h.fake.with(|fs| fs.vm_write("README.md", b"# changed"));
    h.barrier();
    let ev = h.events();
    let rc = ev
        .iter()
        .position(|e| matches!(e, EngineEvent::ReplicaChanged { ids, .. } if ids.contains(&id)));
    let ws = ev
        .iter()
        .position(|e| matches!(e, EngineEvent::WorkingSetChanged { .. }));
    assert!(rc.is_some() && ws.is_some(), "{ev:?}");
    // The signalled anchor is already committed: changes_since from it is empty and from the
    // old anchor carries the change (MQ-004: never empty while behind).
    let c = changes_all(&h.engine, &a0);
    assert_eq!(upd_names(&c), vec!["README.md"]);
    assert_eq!(c.anchor, h.engine.anchor());
    let c2 = h.engine.changes_since(&c.anchor, 10).expect("changes");
    assert!(c2.updated.is_empty() && c2.removed.is_empty() && !c2.more);
    // Content changed → fetch returns the new bytes.
    assert_eq!(h.fetch(id), b"# changed");
}

#[test]
fn working_set_filter_uses_materialized_set() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let (src, docs) = (h.id("src"), h.id("docs"));
    h.engine
        .materialized_changed(&[src], &[], false)
        .expect("mat");
    let a0 = h.engine.anchor();
    h.fake.with(|fs| {
        fs.vm_write("docs/b.txt", b"b"); // not in M
        fs.vm_write("src/c.rs", b"c"); // parent in M
    });
    h.barrier();
    let c = changes_all(&h.engine, &a0);
    assert_eq!(upd_names(&c), vec!["c.rs"]);
    // Move out of M: reported (old parent ∈ M).
    let a1 = c.anchor.clone();
    h.fake.with(|fs| fs.vm_rename("src/c.rs", "docs/c.rs"));
    h.barrier();
    let c = changes_all(&h.engine, &a1);
    assert_eq!(upd_names(&c), vec!["c.rs"]);
    assert_eq!(c.updated[0].entry.parent, docs);
    // Removal of a directory in M: tombstones for descendants, children first.
    let a2 = c.anchor.clone();
    let (main, lib) = (h.id("src/main.rs"), h.id("src/lib.rs"));
    h.fake.with(|fs| fs.vm_rm("src"));
    h.barrier();
    let c = changes_all(&h.engine, &a2);
    assert_eq!(c.removed.len(), 3, "{:?}", c.removed);
    assert_eq!(*c.removed.last().expect("last"), src);
    assert!(c.removed.contains(&main) && c.removed.contains(&lib));
    // Foreign anchor → AnchorExpired + Reimport.
    h.clear_events();
    let mut bad = a0.clone();
    bad[0] ^= 0xFF;
    let e = h.engine.changes_since(&bad, 10).expect_err("expired");
    assert_eq!(e.code, ErrorCode::AnchorExpired);
    h.wait_for("reimport event", || {
        h.events().contains(&EngineEvent::Reimport {
            below: ItemId::ROOT,
        })
    });
}

#[test]
fn lazy_dir_listed_on_demand() {
    let fake = FakeServer::new();
    fake.with(|fs| {
        fs.vm_lazy_dir("node_modules");
        fs.vm_write("node_modules/left-pad.js", b"x");
        fs.vm_mkdir("node_modules/react");
    });
    let h = start(&fake);
    let nm = h.id("node_modules");
    assert_eq!(h.requests(|r| matches!(r, Request::ListDir { .. })), 0);
    assert_eq!(h.names(nm), vec!["left-pad.js", "react"]);
    assert_eq!(h.requests(|r| matches!(r, Request::ListDir { .. })), 1);
    // Known now: no second ListDir.
    assert_eq!(h.names(nm).len(), 2);
    assert_eq!(
        h.requests(|r| matches!(r, Request::ListDir { dir } if *dir == nm)),
        1
    );
    // Child dir from a listing is not known-complete: listing it asks again.
    let react = h.id("node_modules/react");
    assert!(h.names(react).is_empty());
    assert_eq!(
        h.requests(|r| matches!(r, Request::ListDir { dir } if *dir == react)),
        1
    );
    // Lookup in a never-listed lazy dir works too.
    fake.with(|fs| {
        fs.vm_lazy_dir(".git");
        fs.vm_write(".git/HEAD", b"ref");
    });
    h.barrier();
    let git = h.id(".git");
    assert_eq!(
        h.engine.lookup(git, "HEAD").expect("lookup").entry.name,
        "HEAD"
    );
}

#[test]
fn create_uploads_then_fetch_is_local() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let src = h.id("src");
    let m = h
        .create_file(src, "new.rs", b"hello world", "t-1")
        .expect("create");
    assert_eq!(m.item.display_name, "new.rs");
    assert!(!m.should_fetch_content && m.conflict_copy.is_none());
    assert_eq!(
        fake.with(|fs| fs.content_of("src/new.rs")),
        Some(b"hello world".to_vec())
    );
    let reads = h.requests(|r| matches!(r, Request::Read { .. }));
    assert_eq!(h.fetch(m.item.entry.id), b"hello world");
    assert_eq!(
        h.requests(|r| matches!(r, Request::Read { .. })),
        reads,
        "served from the uploaded bytes"
    );
    // Version in the reply is the server's (MQ-013).
    let vm = fake.with(|fs| fs.nodes[&m.item.entry.id].entry.version);
    assert_eq!(m.item.entry.version, vm);
    // Dir and symlink creates.
    let d = h
        .engine
        .create(CreateRequest {
            template_id: "t-2".into(),
            parent: src,
            name: "sub".into(),
            kind: CreateKind::Dir,
            content: None,
            symlink_target: None,
            mtime_ns: None,
            user_exec: None,
            changed_fields: 0,
            local: LocalMeta::default(),
            may_already_exist: false,
            deletion_conflicted: false,
        })
        .expect("mkdir");
    assert_eq!(d.item.entry.kind, Kind::Dir);
    assert!(h.names(d.item.entry.id).is_empty());
    assert_eq!(
        h.requests(|r| matches!(r, Request::ListDir { .. })),
        0,
        "a dir we created is known empty"
    );
}

#[test]
fn own_create_echoed_by_events_commits_once() {
    // unlatchd pushes the new item's `Events` before it replies; the reply's upsert then repeats
    // what that commit already made durable and must not cost a second replica transaction
    // (an fsync on T9's critical path). The item is still committed when create returns.
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    h.barrier();
    let a0 = h.engine.anchor();
    let sh = h.engine.inner.shared.clone();
    let writes = || sh.replica_writes.load(std::sync::atomic::Ordering::Acquire);
    let before = writes();
    h.create_file(ItemId::ROOT, "echo.rs", b"echo", "t-echo")
        .expect("create");
    assert_eq!(writes() - before, 1, "one replica transaction per create");
    let c = changes_all(&h.engine, &a0);
    assert!(
        upd_names(&c).contains(&"echo.rs".to_string()),
        "in a committed anchor on return: {:?}",
        upd_names(&c)
    );
}

#[test]
fn create_never_surfaces_exists() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let root = ItemId::ROOT;
    // Same name, different content → "name 2.ext".
    let m = h
        .create_file(root, "README.md", b"other", "t-a")
        .expect("create");
    assert_eq!(m.item.display_name, "README 2.md");
    // The VM has a file the replica doesn't know yet: Exists from the daemon → next name.
    fake.with(|fs| fs.vm_write_silent("ghost.txt", b"g"));
    let m = h
        .create_file(root, "ghost.txt", b"mine", "t-b")
        .expect("create");
    assert_eq!(m.item.display_name, "ghost 2.txt");
    assert_eq!(
        fake.with(|fs| fs.content_of("ghost.txt")),
        Some(b"g".to_vec())
    );
    // Identical content → the existing item, nothing new.
    let n_before = fake.with(|fs| fs.names_in("").len());
    let m = h
        .create_file(root, "README.md", b"# hi", "t-c")
        .expect("create");
    assert_eq!(m.item.display_name, "README.md");
    assert!(!m.should_fetch_content);
    assert_eq!(fake.with(|fs| fs.names_in("").len()), n_before);
    // may_already_exist (reimport) with other bytes → the existing item at the server's
    // version (re-download) and the Mac's bytes kept as its conflict copy, never dropped.
    let m = h
        .engine
        .create(CreateRequest {
            template_id: "t-d".into(),
            parent: root,
            name: "README.md".into(),
            kind: CreateKind::File,
            content: Some(h.file(b"local version")),
            symlink_target: None,
            mtime_ns: None,
            user_exec: None,
            changed_fields: fields::CONTENTS,
            local: LocalMeta::default(),
            may_already_exist: true,
            deletion_conflicted: false,
        })
        .expect("create");
    assert_eq!(m.item.entry.id, h.id("README.md"));
    assert!(m.should_fetch_content);
    assert_eq!(
        fake.with(|fs| fs.content_of("README.md")),
        Some(b"# hi".to_vec())
    );
    let copy = m.conflict_copy.expect("conflict copy");
    assert!(
        copy.display_name.contains("conflict"),
        "{}",
        copy.display_name
    );
    assert_eq!(h.fetch(copy.entry.id), b"local version");
    assert_eq!(fake.with(|fs| fs.names_in("").len()), n_before + 1);
    // The same create replayed (lost reply): the same answer, no second copy.
    let again = h
        .engine
        .create(CreateRequest {
            template_id: "t-d".into(),
            parent: root,
            name: "README.md".into(),
            kind: CreateKind::File,
            content: Some(h.file(b"local version")),
            symlink_target: None,
            mtime_ns: None,
            user_exec: None,
            changed_fields: fields::CONTENTS,
            local: LocalMeta::default(),
            may_already_exist: true,
            deletion_conflicted: false,
        })
        .expect("replay");
    assert_eq!(again.item.entry.id, h.id("README.md"));
    assert_eq!(fake.with(|fs| fs.names_in("").len()), n_before + 1);
    // Bytes identical to the VM's: just that file.
    let m = h
        .engine
        .create(CreateRequest {
            template_id: "t-e".into(),
            parent: root,
            name: "README.md".into(),
            kind: CreateKind::File,
            content: Some(h.file(b"# hi")),
            symlink_target: None,
            mtime_ns: None,
            user_exec: None,
            changed_fields: fields::CONTENTS,
            local: LocalMeta::default(),
            may_already_exist: true,
            deletion_conflicted: false,
        })
        .expect("create");
    assert!(!m.should_fetch_content && m.conflict_copy.is_none());
    assert_eq!(fake.with(|fs| fs.names_in("").len()), n_before + 1);
}

#[test]
fn write_reply_older_than_the_item_asks_for_a_fetch() {
    // MQ-013: the system believes the version it is handed with the bytes it holds. An agent
    // appended right after unlatchd published the Mac's bytes: the reply names the Mac's bytes
    // (version A) and the item is already at B > A (its Events arrive before the reply). A
    // create and a modify must both ask for a fetch, never return B as the Mac's version.
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    fake.with(|fs| fs.faults.agent_append_after_write = Some(b"AGENT".to_vec()));
    let m = h
        .create_file(ItemId::ROOT, "new.txt", b"mac", "t-race")
        .expect("create");
    assert_eq!(
        fake.with(|fs| fs.content_of("new.txt")),
        Some(b"macAGENT".to_vec())
    );
    assert_eq!(
        Some(m.item.entry.version.content),
        fake.with(|fs| fs
            .resolve("new.txt")
            .map(|id| fs.nodes[&id].entry.version.content))
    );
    assert!(
        m.should_fetch_content,
        "create: the Mac's bytes are not the item's version"
    );
    let id = h.id("README.md");
    let it = h.engine.item(id).expect("item");
    fake.with(|fs| fs.faults.agent_append_after_write = Some(b"AGENT".to_vec()));
    let m = h
        .engine
        .modify(
            id,
            it.entry.version.into(),
            ModifyRequest {
                changed_fields: fields::CONTENTS,
                content: Some(h.file(b"v2")),
                ..Default::default()
            },
        )
        .expect("modify");
    assert_eq!(
        fake.with(|fs| fs.content_of("README.md")),
        Some(b"v2AGENT".to_vec())
    );
    assert!(
        m.should_fetch_content,
        "modify: the Mac's bytes are not the item's version"
    );
}

#[test]
fn excluded_names_and_packages() {
    let fake = FakeServer::new();
    tree(&fake);
    fake.with(|fs| fs.vm_write("._keep", b"vm has it"));
    let h = start(&fake);
    let e = h
        .create_file(ItemId::ROOT, ".DS_Store", b"x", "t-1")
        .expect_err("excluded");
    assert_eq!(e.code, ErrorCode::ExcludedFromSync);
    let pkg = h.engine.create(CreateRequest {
        template_id: "t-2".into(),
        parent: ItemId::ROOT,
        name: "Foo.app".into(),
        kind: CreateKind::Package,
        content: None,
        symlink_target: None,
        mtime_ns: None,
        user_exec: None,
        changed_fields: 0,
        local: LocalMeta::default(),
        may_already_exist: false,
        deletion_conflicted: false,
    });
    assert_eq!(pkg.expect_err("pkg").code, ErrorCode::ExcludedFromSync);
    // The VM already has it → return it, never ExcludedFromSync.
    let m = h
        .create_file(ItemId::ROOT, "._keep", b"x", "t-3")
        .expect("existing");
    assert_eq!(m.item.entry.id, h.id("._keep"));
    assert_eq!(h.mutating_requests(), 0);
}

#[test]
fn modify_content_and_conflict() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let id = h.id("README.md");
    let it = h.engine.item(id).expect("item");
    let m = h
        .engine
        .modify(
            id,
            it.entry.version.into(),
            ModifyRequest {
                changed_fields: fields::CONTENTS,
                content: Some(h.file(b"v2")),
                ..Default::default()
            },
        )
        .expect("modify");
    assert!(!m.should_fetch_content && m.conflict_copy.is_none());
    assert_eq!(
        fake.with(|fs| fs.content_of("README.md")),
        Some(b"v2".to_vec())
    );
    assert_eq!(
        m.item.entry.version,
        fake.with(|fs| fs.nodes[&id].entry.version)
    );
    // The agent writes; the Mac saves against its stale base → conflict, never an error.
    let stale: BaseVersion = m.item.entry.version.into();
    fake.with(|fs| fs.vm_write("README.md", b"agent"));
    h.barrier();
    h.clear_events();
    let m = h
        .engine
        .modify(
            id,
            stale,
            ModifyRequest {
                changed_fields: fields::CONTENTS,
                content: Some(h.file(b"mac")),
                ..Default::default()
            },
        )
        .expect("modify");
    assert!(
        m.should_fetch_content,
        "system must re-download the server content"
    );
    let copy = m.conflict_copy.expect("conflict copy");
    assert!(
        copy.display_name
            .starts_with("README (conflict from My Mac"),
        "{}",
        copy.display_name
    );
    assert_eq!(
        fake.with(|fs| fs.content_of("README.md")),
        Some(b"agent".to_vec()),
        "agent bytes survive"
    );
    assert_eq!(
        m.item.entry.version,
        fake.with(|fs| fs.nodes[&id].entry.version)
    );
    assert_eq!(h.fetch(copy.entry.id), b"mac");
    h.wait_for("signal", || {
        h.events()
            .iter()
            .any(|e| matches!(e, EngineEvent::WorkingSetChanged { .. }))
    });
    // Unknown base (beforeFirstSync) with identical bytes → no conflict.
    let m = h
        .engine
        .modify(
            id,
            BaseVersion::default(),
            ModifyRequest {
                changed_fields: fields::CONTENTS,
                content: Some(h.file(b"agent")),
                ..Default::default()
            },
        )
        .expect("modify");
    assert!(m.conflict_copy.is_none());
}

#[test]
fn local_only_fields_generate_no_traffic() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let id = h.id("README.md");
    let it = h.engine.item(id).expect("item");
    let before = h.fake.with(|fs| fs.requests.len());
    let local = LocalMeta {
        tag_data: Some(vec![1, 2, 3]),
        favorite_rank: Some(7),
        hidden: true,
        ..Default::default()
    };
    let m = h
        .engine
        .modify(
            id,
            it.entry.version.into(),
            ModifyRequest {
                changed_fields: fields::TAG_DATA
                    | fields::FAVORITE_RANK
                    | fields::FILE_SYSTEM_FLAGS
                    | (1 << 20),
                local: local.clone(),
                user_exec: Some(true), // ignored: expose_exec is off
                ..Default::default()
            },
        )
        .expect("modify");
    assert_eq!(m.still_pending, 1 << 20);
    assert_eq!(m.item.local, local);
    // Pings and the session's RTT probes (a Stat of the root while bulk data moves) are
    // background traffic; the item under test is never the root.
    let background = |r: &Request| {
        matches!(r, Request::Ping { .. })
            || matches!(r, Request::Stat { id } if *id == ItemId::ROOT)
    };
    let after = h
        .fake
        .with(|fs| fs.requests.iter().filter(|r| !background(r)).count());
    let before_np = h.fake.with(|fs| {
        fs.requests[..before]
            .iter()
            .filter(|r| !background(r))
            .count()
    });
    assert_eq!(after, before_np, "zero network traffic");
    // Persisted and merged into every item.
    let h = h.restart();
    assert_eq!(h.engine.item(id).expect("item").local, local);
    let page = h.engine.list(ItemId::ROOT, None, 100, false).expect("list");
    assert_eq!(
        page.items
            .iter()
            .find(|i| i.entry.id == id)
            .map(|i| i.local.clone()),
        Some(local)
    );
}

#[test]
fn rename_reply_with_newer_content_asks_for_a_fetch() {
    // MQ-013: the system believes the reply's version with the bytes it holds. The agent
    // rewrote the file after the system's last look: a rename reply carrying the newer
    // content version must set `should_fetch_content`; one carrying the same must not.
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let id = h.id("README.md");
    let seen: BaseVersion = h.engine.item(id).expect("item").entry.version.into();
    let rename = |base: BaseVersion, name: &str| {
        h.engine
            .modify(
                id,
                base,
                ModifyRequest {
                    changed_fields: fields::FILENAME,
                    new_name: Some(name.into()),
                    ..Default::default()
                },
            )
            .expect("rename")
    };
    let m = rename(seen, "A.md");
    assert!(!m.should_fetch_content, "nothing changed: no refetch");
    let seen: BaseVersion = m.item.entry.version.into();
    fake.with(|fs| fs.vm_write("A.md", b"agent rewrote this"));
    h.barrier();
    let m = rename(seen, "B.md");
    assert_eq!(fake.with(|fs| fs.resolve("B.md")), Some(id));
    assert_ne!(Some(m.item.entry.version.content), seen.content);
    assert!(m.should_fetch_content, "newer content in a metadata reply");
}

#[test]
fn moving_a_mapped_twin_moves_its_real_name() {
    // Rule 11: the system sends no filename for a pure move; the display name it holds is the
    // generated `foo (Unlatch 2).txt`, which must never reach the VM.
    let fake = FakeServer::new();
    tree(&fake);
    fake.with(|fs| {
        fs.vm_write("Foo.txt", b"1");
        fs.vm_write("foo.txt", b"2");
    });
    let h = start(&fake);
    let lower = h.id("foo.txt");
    let li = h.engine.item(lower).expect("item");
    assert_eq!(li.display_name, "foo (Unlatch 2).txt");
    let docs = h.id("docs");
    let m = h
        .engine
        .modify(
            lower,
            li.entry.version.into(),
            ModifyRequest {
                changed_fields: fields::PARENT,
                new_parent: Some(docs),
                ..Default::default()
            },
        )
        .expect("move");
    assert_eq!(fake.with(|fs| fs.resolve("docs/foo.txt")), Some(lower));
    assert_eq!(m.item.entry.name, "foo.txt");
    assert_eq!(m.item.display_name, "foo.txt");
}

#[test]
fn rename_base_mismatch_and_bounce() {
    let fake = FakeServer::new();
    tree(&fake);
    fake.with(|fs| {
        fs.vm_write("Foo.txt", b"1");
        fs.vm_write("foo.txt", b"2");
    });
    let h = start(&fake);
    let id = h.id("README.md");
    let it = h.engine.item(id).expect("item");
    let m = h
        .engine
        .modify(
            id,
            it.entry.version.into(),
            ModifyRequest {
                changed_fields: fields::FILENAME,
                new_name: Some("README2.md".into()),
                ..Default::default()
            },
        )
        .expect("rename");
    assert_eq!(m.item.display_name, "README2.md");
    assert_eq!(fake.with(|fs| fs.resolve("README2.md")), Some(id));
    // The VM renamed it meanwhile: metadata never errors — the server state comes back.
    let stale: BaseVersion = m.item.entry.version.into();
    fake.with(|fs| fs.vm_rename("README2.md", "VM.md"));
    h.barrier();
    let m = h
        .engine
        .modify(
            id,
            stale,
            ModifyRequest {
                changed_fields: fields::FILENAME,
                new_name: Some("Mac.md".into()),
                ..Default::default()
            },
        )
        .expect("rename");
    assert_eq!(m.item.display_name, "VM.md");
    assert_eq!(fake.with(|fs| fs.resolve("VM.md")), Some(id));
    // Collision mapping: Foo.txt keeps its name, foo.txt is shown as "foo (Unlatch 2).txt".
    let lower = h.id("foo.txt");
    let li = h.engine.item(lower).expect("item");
    assert_eq!(li.display_name, "foo (Unlatch 2).txt");
    // The system's own bounce ("foo (Unlatch 2) 2.txt") stays local.
    let muts = h.mutating_requests();
    let m = h
        .engine
        .modify(
            lower,
            li.entry.version.into(),
            ModifyRequest {
                changed_fields: fields::FILENAME,
                new_name: Some("foo (Unlatch 2) 2.txt".into()),
                ..Default::default()
            },
        )
        .expect("bounce");
    assert_eq!(m.item.display_name, "foo (Unlatch 2) 2.txt");
    assert_eq!(m.item.entry.name, "foo.txt");
    assert_eq!(h.mutating_requests(), muts, "bounce never reaches the VM");
    // A real rename of the mapped item maps display → real on the wire.
    let m = h
        .engine
        .modify(
            lower,
            m.item.entry.version.into(),
            ModifyRequest {
                changed_fields: fields::FILENAME,
                new_name: Some("bar.txt".into()),
                ..Default::default()
            },
        )
        .expect("rename");
    assert_eq!(m.item.display_name, "bar.txt");
    assert_eq!(fake.with(|fs| fs.resolve("bar.txt")), Some(lower));
    // Rename onto a name the VM already has → "name 2", never an error.
    let a = h.id("docs/a.txt");
    let ai = h.engine.item(a).expect("item");
    let m = h
        .engine
        .modify(
            a,
            ai.entry.version.into(),
            ModifyRequest {
                changed_fields: fields::FILENAME | fields::PARENT,
                new_parent: Some(ItemId::ROOT),
                new_name: Some("bar.txt".into()),
                ..Default::default()
            },
        )
        .expect("rename");
    assert_eq!(m.item.display_name, "bar 2.txt");
}

#[test]
fn delete_rules() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    // Unknown id → Ok.
    h.engine
        .delete(ItemId(987_654), BaseVersion::default(), false)
        .expect("unknown ok");
    // Base mismatch → DeletionRejected.
    let id = h.id("README.md");
    let old: BaseVersion = h.engine.item(id).expect("item").entry.version.into();
    fake.with(|fs| fs.vm_write("README.md", b"changed"));
    h.barrier();
    assert_eq!(
        h.engine.delete(id, old, false).expect_err("rejected").code,
        ErrorCode::DeletionRejected
    );
    assert!(fake.with(|fs| fs.resolve("README.md")).is_some());
    // Non-recursive delete of a non-empty dir → DeletionRejected.
    let docs = h.id("docs");
    let dv: BaseVersion = h.engine.item(docs).expect("item").entry.version.into();
    assert_eq!(
        h.engine.delete(docs, dv, false).expect_err("rejected").code,
        ErrorCode::DeletionRejected
    );
    // Recursive delete while the agent adds a file the system never saw → rejected, file kept.
    let _ = h.engine.anchor(); // system consumed up to here
    fake.with(|fs| fs.vm_write("docs/agent-new.txt", b"precious"));
    h.barrier();
    let e = h.engine.delete(docs, dv, true).expect_err("rejected");
    assert_eq!(e.code, ErrorCode::DeletionRejected);
    assert_eq!(
        fake.with(|fs| fs.content_of("docs/agent-new.txt")),
        Some(b"precious".to_vec())
    );
    // DeletionRejected restores the folder and the system enumerates it again; once it has
    // seen the new file (anchor consumed), the user's next recursive delete goes through. (The
    // same call merely retried keeps its first seen_seq: `retried_folder_delete_keeps_its_first_seen_seq`.)
    h.engine.list(docs, None, 100, true).expect("re-enumerate");
    let _ = h.engine.anchor();
    let dv: BaseVersion = h.engine.item(docs).expect("item").entry.version.into();
    h.engine.delete(docs, dv, true).expect("delete");
    assert!(fake.with(|fs| fs.resolve("docs")).is_none());
    assert!(matches!(h.engine.item(docs), Err(e) if e.code == ErrorCode::NotFound));
    // Reparent to the trash = delete.
    let lib = h.id("src/lib.rs");
    let lv: BaseVersion = h.engine.item(lib).expect("item").entry.version.into();
    let r = h.engine.modify(
        lib,
        lv,
        ModifyRequest {
            changed_fields: fields::PARENT,
            new_parent: Some(ItemId::TRASH),
            ..Default::default()
        },
    );
    assert!(matches!(r, Err(e) if e.code == ErrorCode::NotFound));
    assert!(fake.with(|fs| fs.resolve("src/lib.rs")).is_none());
}

/// Rule 6 with an unknown content base (`beforeFirstSyncComponent`): the engine must not fall
/// back to its replica's version, which may be newer than anything the system was shown.
#[test]
fn delete_with_unknown_content_base_keeps_unseen_agent_write() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let _ = h.engine.anchor(); // the system consumed everything so far
    let id = h.id("README.md");
    let meta = h.engine.item(id).expect("item").entry.version.meta;
    fake.with(|fs| fs.vm_write("README.md", b"agent work"));
    h.barrier(); // the replica has it; the system has not consumed it
    for base in [
        BaseVersion {
            content: None,
            meta: Some(meta),
        },
        BaseVersion::default(),
    ] {
        let e = h.engine.delete(id, base, false).expect_err("rejected");
        assert_eq!(e.code, ErrorCode::DeletionRejected, "{base:?}: {e:?}");
        assert_eq!(
            fake.with(|fs| fs.content_of("README.md")),
            Some(b"agent work".to_vec()),
            "{base:?}"
        );
    }
    // The rejection restored the item at its current version; deleting that goes through.
    let cur: BaseVersion = h.engine.item(id).expect("item").entry.version.into();
    h.engine.delete(id, cur, false).expect("delete");
    assert!(fake.with(|fs| fs.resolve("README.md")).is_none());
    // An unknown base of an item the system has consumed every change of is fine.
    let lib = h.id("src/lib.rs");
    h.engine
        .delete(lib, BaseVersion::default(), false)
        .expect("delete of a seen file");
    assert!(fake.with(|fs| fs.resolve("src/lib.rs")).is_none());
}

/// Rule 6, fuzz seed 186: a recursive delete whose reply was lost is retried by the system
/// after it consumed newer anchors (for items it had already deleted locally). The retry is the
/// same call: it must keep the first attempt's `seen_seq` — across an engine restart too — and
/// not delete the agent's file. Only after the system enumerates the folder again (it was
/// restored and re-learned) does a delete of it see the newer file.
#[test]
fn retried_folder_delete_keeps_its_first_seen_seq() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let docs = h.id("docs");
    let dv: BaseVersion = h.engine.item(docs).expect("item").entry.version.into();
    let _ = h.engine.anchor();
    fake.with(|fs| fs.vm_write("docs/agent-new.txt", b"precious"));
    h.barrier();
    let e = h.engine.delete(docs, dv, true).expect_err("partial");
    assert_eq!(e.code, ErrorCode::DeletionRejected);
    // The reply was lost; meanwhile the system consumed the anchor carrying agent-new.txt —
    // under a folder the user had already deleted locally — and the engine restarted.
    let _ = h.engine.anchor();
    let h = h.restart();
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    let _ = h.engine.anchor();
    let e = h
        .engine
        .delete(docs, dv, true)
        .expect_err("retry: same call");
    assert_eq!(e.code, ErrorCode::DeletionRejected);
    assert_eq!(
        fake.with(|fs| fs.content_of("docs/agent-new.txt")),
        Some(b"precious".to_vec()),
        "the retried delete removed a file the user never saw"
    );
    // DeletionRejected restored the folder; the system enumerates it and shows the new file.
    h.engine.list(docs, None, 100, true).expect("list");
    let _ = h.engine.anchor();
    h.engine.delete(docs, dv, true).expect("a new delete");
    assert!(fake.with(|fs| fs.resolve("docs")).is_none());
}

#[test]
fn replayed_ops_after_commit_before_reply() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    // create
    fake.with(|fs| fs.faults.die_after_commit = Some("write"));
    let e = h
        .create_file(ItemId::ROOT, "x.txt", b"data", "tmpl-x")
        .expect_err("killed");
    assert!(
        matches!(e.code, ErrorCode::Offline | ErrorCode::Timeout),
        "{e:?}"
    );
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    let m = h
        .create_file(ItemId::ROOT, "x.txt", b"data", "tmpl-x")
        .expect("replay");
    assert_eq!(m.item.display_name, "x.txt");
    assert_eq!(
        fake.with(|fs| fs
            .names_in("")
            .iter()
            .filter(|n| n.starts_with('x'))
            .count()),
        1,
        "no duplicate"
    );
    // modify
    let id = m.item.entry.id;
    let base: BaseVersion = m.item.entry.version.into();
    fake.with(|fs| fs.faults.die_after_commit = Some("write"));
    let req = || ModifyRequest {
        changed_fields: fields::CONTENTS,
        content: Some(h.file(b"data v2")),
        ..Default::default()
    };
    assert!(h.engine.modify(id, base, req()).is_err());
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    let m = h.engine.modify(id, base, req()).expect("replay");
    assert!(
        m.conflict_copy.is_none() && !m.should_fetch_content,
        "no self-conflict on replay"
    );
    assert!(!fake.with(|fs| fs.names_in("").iter().any(|n| n.contains("conflict"))));
    // mkdir
    fake.with(|fs| fs.faults.die_after_commit = Some("mkdir"));
    let mk = || CreateRequest {
        template_id: "tmpl-d".into(),
        parent: ItemId::ROOT,
        name: "newdir".into(),
        kind: CreateKind::Dir,
        content: None,
        symlink_target: None,
        mtime_ns: None,
        user_exec: None,
        changed_fields: 0,
        local: LocalMeta::default(),
        may_already_exist: false,
        deletion_conflicted: false,
    };
    assert!(h.engine.create(mk()).is_err());
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    let d = h.engine.create(mk()).expect("replay");
    assert_eq!(d.item.display_name, "newdir");
    assert_eq!(
        fake.with(|fs| fs
            .names_in("")
            .iter()
            .filter(|n| n.starts_with("newdir"))
            .count()),
        1
    );
    // rename
    let it = h.engine.item(id).expect("item");
    fake.with(|fs| fs.faults.die_after_commit = Some("rename"));
    let rn = || ModifyRequest {
        changed_fields: fields::FILENAME,
        new_name: Some("y.txt".into()),
        ..Default::default()
    };
    assert!(h.engine.modify(id, it.entry.version.into(), rn()).is_err());
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    let m = h
        .engine
        .modify(id, it.entry.version.into(), rn())
        .expect("replay");
    assert_eq!(m.item.display_name, "y.txt");
    // delete
    let it = h.engine.item(id).expect("item");
    fake.with(|fs| fs.faults.die_after_commit = Some("remove"));
    assert!(h.engine.delete(id, it.entry.version.into(), false).is_err());
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    h.engine
        .delete(id, it.entry.version.into(), false)
        .expect("replay");
    assert!(fake.with(|fs| fs.resolve("y.txt")).is_none());
}

#[test]
fn mass_delete_guard_pauses_until_confirmed() {
    for apply in [true, false] {
        let fake = FakeServer::new();
        let mut ids = Vec::new();
        fake.with(|fs| {
            fs.vm_mkdir("big");
            for i in 0..60 {
                ids.push(fs.vm_write(&format!("big/f{i}"), b"x"));
            }
            fs.vm_write("keep.txt", b"k");
        });
        let h = start(&fake);
        h.engine.materialized_changed(&ids, &[], true).expect("mat");
        fake.with(|fs| fs.vm_rm("big"));
        h.wait_for("paused", || {
            matches!(h.engine.status().state, ConnState::Paused { .. })
        });
        // Nothing removed while paused; later events are held too.
        fake.with(|fs| fs.vm_write("later.txt", b"l"));
        std::thread::sleep(Duration::from_millis(50));
        assert!(h.engine.item(ids[0]).is_ok());
        h.engine.confirm_paused(apply).expect("confirm");
        h.wait_for("live", || h.engine.status().state == ConnState::Live);
        h.barrier();
        assert_eq!(h.engine.item(ids[0]).is_err(), apply);
        assert!(
            h.engine.lookup(ItemId::ROOT, "later.txt").is_ok(),
            "held events applied after confirm"
        );
    }
}

#[test]
fn index_change_wipes_and_reimports() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let old_readme = h.id("README.md");
    h.clear_events();
    let old_anchor = h.engine.anchor();
    // The VM is re-imaged: new index, new ids.
    fake.with(|fs| {
        fs.index = IndexId(0x5555);
        fs.vm_rm("README.md");
        fs.vm_write("README.md", b"fresh");
    });
    h.engine.drop_connection();
    h.wait_for("reimport", || {
        h.events().contains(&EngineEvent::Reimport {
            below: ItemId::ROOT,
        })
    });
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    assert!(h.engine.item(old_readme).is_err());
    assert_eq!(
        h.engine
            .changes_since(&old_anchor, 10)
            .expect_err("expired")
            .code,
        ErrorCode::AnchorExpired
    );
    assert_eq!(h.names(ItemId::ROOT), vec!["README.md", "docs", "src"]);
}

#[test]
fn big_fetch_under_credit_with_progress_and_cancel() {
    let fake = FakeServer::new();
    let big: Vec<u8> = (0..(5 << 20)).map(|i| (i % 251) as u8).collect();
    fake.with(|fs| fs.vm_write("big.bin", &big));
    let h = start(&fake);
    let id = h.id("big.bin");
    let calls = std::sync::atomic::AtomicU64::new(0);
    let last = std::sync::atomic::AtomicU64::new(0);
    let d = h.dir.path().join("dl");
    std::fs::create_dir_all(&d).expect("mkdir");
    let f = h
        .engine
        .fetch(
            id,
            None,
            &d,
            &|done, total| {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                last.store(done, std::sync::atomic::Ordering::Relaxed);
                assert_eq!(total, big.len() as u64);
            },
            &CancelToken::new(),
        )
        .expect("fetch");
    assert_eq!(std::fs::read(&f.path).expect("read"), big);
    assert!(calls.load(std::sync::atomic::Ordering::Relaxed) >= 80);
    assert_eq!(
        last.load(std::sync::atomic::Ordering::Relaxed),
        big.len() as u64
    );
    // The server never had more than one window of unacknowledged bulk data.
    assert!(fake.with(|fs| fs.max_outstanding_read) <= 2 * 1024 * 1024);
    // Range reads from the cache.
    assert_eq!(
        h.engine.read(id, 10, 5).expect("read"),
        big[10..15].to_vec()
    );
    assert!(h
        .engine
        .read(id, big.len() as u64 + 1, 5)
        .expect("read")
        .is_empty());
    // Cancel: a new version, cancelled on the first progress callback.
    fake.with(|fs| fs.vm_write("big.bin", &big[..4 << 20]));
    h.barrier();
    let tok = CancelToken::new();
    let r = h.engine.fetch(id, None, &d, &|_, _| tok.cancel(), &tok);
    assert_eq!(r.expect_err("cancelled").code, ErrorCode::Cancelled);
    // Still works afterwards.
    assert_eq!(h.fetch(id).len(), 4 << 20);
}

#[test]
fn fetch_retries_on_version_mismatch() {
    let fake = FakeServer::new();
    fake.with(|fs| {
        fs.vm_write("f.txt", b"one");
    });
    let h = start(&fake);
    let id = h.id("f.txt");
    // Change the file on the VM without telling the engine (no broadcast): the replica's version
    // is stale, the daemon answers VersionMismatch, the engine re-stats and retries.
    fake.with(|fs| {
        let seq = fs.seq + 1;
        fs.seq = seq;
        let n = fs.nodes.get_mut(&id).expect("node");
        n.content = b"two".to_vec();
        n.entry.size = 3;
        n.entry.version.content = seq;
        n.entry.seq = seq;
    });
    let d = h.dir.path().join("dl");
    std::fs::create_dir_all(&d).expect("mkdir");
    let f = h
        .engine
        .fetch(id, None, &d, &|_, _| {}, &CancelToken::new())
        .expect("fetch");
    assert_eq!(std::fs::read(&f.path).expect("read"), b"two");
    assert_eq!(
        f.item.entry.version,
        fake.with(|fs| fs.nodes[&id].entry.version)
    );
}

#[test]
fn dead_link_detected_and_reconnected() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let s0 = fake.with(|fs| fs.sessions);
    fake.with(|fs| fs.faults.ignore_pings = true);
    h.wait_for("reconnect", || fake.with(|fs| fs.sessions) > s0);
    fake.with(|fs| fs.faults.ignore_pings = false);
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    h.wait_for("error resolved", || {
        h.events().contains(&EngineEvent::ErrorResolved)
    });
}

#[test]
fn needs_user_is_not_retried_automatically() {
    let fake = FakeServer::new();
    tree(&fake);
    fake.with(|fs| {
        fs.faults.connect_error = Some(err(
            ErrorCode::NeedsUser,
            "Host key verification failed; visit https://example.com/login",
        ))
    });
    let h = start_in(&fake, tempfile::tempdir().expect("tempdir"), |_| {});
    h.wait_for("needs user", || {
        matches!(h.engine.status().state, ConnState::NeedsUser { .. })
    });
    match h.engine.status().state {
        ConnState::NeedsUser { url, .. } => {
            assert_eq!(url.as_deref(), Some("https://example.com/login"))
        }
        s => panic!("{s:?}"),
    }
    h.wait_for("needs-user event", || {
        h.events()
            .iter()
            .any(|e| matches!(e, EngineEvent::NeedsUser { .. }))
    });
    assert_eq!(
        h.engine
            .wait_live(Duration::from_millis(100))
            .expect_err("needs user")
            .code,
        ErrorCode::NeedsUser
    );
    h.engine.network_changed();
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        matches!(h.engine.status().state, ConnState::NeedsUser { .. }),
        "no automatic retry"
    );
    fake.with(|fs| fs.faults.connect_error = None);
    h.engine.connect_interactive().expect("interactive connect");
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
}

#[test]
fn drop_connection_then_catch_up() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    fake.with(|fs| fs.faults.connect_error = Some(err(ErrorCode::Offline, "down")));
    h.engine.drop_connection();
    h.wait_for("offline", || {
        matches!(h.engine.status().state, ConnState::Offline { .. })
    });
    fake.with(|fs| {
        fs.vm_write("during.txt", b"d");
        fs.faults.connect_error = None;
    });
    h.engine.network_changed();
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    h.barrier();
    assert!(h.engine.lookup(ItemId::ROOT, "during.txt").is_ok());
}

#[test]
fn symlinks_in_root_and_blocked() {
    let fake = FakeServer::new();
    fake.with(|fs| {
        fs.vm_mkdir("a");
        fs.vm_write("a/t.txt", b"t");
        fs.vm_symlink("a/ok", "t.txt");
        fs.vm_symlink("a/up", "../a/t.txt");
        fs.vm_symlink("a/abs", &format!("{ROOT_PATH}/a/t.txt"));
        fs.vm_symlink("a/escape", "../../etc/passwd");
        fs.vm_symlink("a/etc", "/etc/passwd");
    });
    let h = start(&fake);
    let get = |n: &str| h.engine.item(h.id(n)).expect("item");
    assert_eq!(get("a/ok").entry.symlink_target.as_deref(), Some("t.txt"));
    assert_eq!(get("a/up").entry.kind, Kind::Symlink);
    let abs = get("a/abs");
    assert_eq!(abs.entry.symlink_target.as_deref(), Some("../a/t.txt"));
    for (n, t) in [("a/escape", "../../etc/passwd"), ("a/etc", "/etc/passwd")] {
        let it = get(n);
        assert!(it.symlink_blocked, "{n}");
        // IPC contract (mac fixture ipc_response_Item~blocked-symlink.json): the kind stays
        // Symlink, the flag makes the shim show a read-only plain-text file.
        assert_eq!(it.entry.kind, Kind::Symlink, "{n}");
        assert_eq!(it.entry.size, t.len() as u64, "{n}");
        assert_eq!(it.entry.symlink_target.as_deref(), Some(t), "{n}");
        assert_eq!(it.caps & unlatch_proto::ipc::caps::WRITING, 0);
        assert!(!it.user_exec, "{n}");
    }
    let esc = h.id("a/escape");
    assert_eq!(
        h.engine.read(esc, 0, 100).expect("read"),
        b"../../etc/passwd"
    );
    assert_eq!(h.fetch(esc), b"../../etc/passwd");
}

#[test]
fn exec_bit_rules() {
    let fake = FakeServer::new();
    fake.with(|fs| {
        let a = fs.vm_write("run.sh", b"#!/bin/sh");
        let b = fs.vm_write("evil.command", b"x");
        fs.vm_mkdir("X.app");
        fs.vm_mkdir("X.app/Contents");
        fs.vm_mkdir("X.app/Contents/MacOS");
        let c = fs.vm_write("X.app/Contents/MacOS/X", b"x");
        for id in [a, b, c] {
            fs.nodes.get_mut(&id).expect("node").entry.mode = 0o755;
        }
    });
    let h = start_in(&fake, tempfile::tempdir().expect("tempdir"), |c| {
        c.expose_exec = true
    });
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    assert!(h.engine.item(h.id("run.sh")).expect("item").user_exec);
    assert!(!h.engine.item(h.id("evil.command")).expect("item").user_exec);
    assert!(
        !h.engine
            .item(h.id("X.app/Contents/MacOS/X"))
            .expect("item")
            .user_exec
    );
    let h2 = start(&fake);
    assert!(
        !h2.engine.item(h2.id("run.sh")).expect("item").user_exec,
        "hidden by default"
    );
}

#[test]
fn prefetch_only_for_viewer_enumerations() {
    let fake = FakeServer::new();
    fake.with(|fs| {
        fs.vm_mkdir("d");
        for i in 0..5 {
            fs.vm_write(&format!("d/s{i}.txt"), b"small");
        }
        fs.vm_write("d/big.bin", &vec![0u8; 400 * 1024]);
    });
    let h = start_in(&fake, tempfile::tempdir().expect("tempdir"), |c| {
        c.prefetch = PrefetchConfig::default()
    });
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    let d = h.id("d");
    h.engine.list(d, None, 100, false).expect("list");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        h.requests(|r| matches!(r, Request::Read { .. })),
        0,
        "no prefetch for non-viewer"
    );
    h.engine.list(d, None, 100, true).expect("list");
    h.wait_for("prefetch", || {
        h.requests(|r| matches!(r, Request::Read { .. })) == 5
    });
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        h.requests(|r| matches!(r, Request::Read { .. })),
        5,
        "big file not prefetched"
    );
    let reads = h.requests(|r| matches!(r, Request::Read { .. }));
    let s0 = h.id("d/s0.txt");
    h.wait_for("cached", || h.engine.status().cache_bytes >= 25);
    assert_eq!(h.fetch(s0), b"small");
    assert_eq!(h.requests(|r| matches!(r, Request::Read { .. })), reads);
}

#[test]
fn snapshot_interleaving_is_last_writer_wins() {
    let fake = FakeServer::new();
    tree(&fake);
    // An event carrying a *newer* state of README arrives before the snapshot chunk that carries
    // the older one; the older must not win.
    let readme = fake.with(|fs| fs.resolve("README.md")).expect("readme");
    let newer = fake.with(|fs| {
        let mut e = fs.nodes[&readme].entry.clone();
        e.seq += 1000;
        e.name = "NEWER.md".into();
        e.version.meta += 1000;
        e
    });
    fake.with(|fs| {
        fs.faults.event_before_snapshot = Some(vec![unlatch_proto::wire::Change::Upsert(newer)])
    });
    let h = start(&fake);
    assert_eq!(
        h.engine.item(readme).expect("item").display_name,
        "NEWER.md"
    );
}

#[test]
fn pending_mutation_survives_snapshot_drop() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let id = h.id("docs/a.txt");
    // Pretend a local mutation is in flight while a resync would drop the item.
    let sh = h.engine.inner.shared.clone();
    let _g = sh.begin_mutation(&[id]);
    fake.with(|fs| {
        // Remove it on the server silently and force a full snapshot on reconnect.
        let n = fs.nodes.remove(&id).expect("node");
        if let Some(p) = fs.nodes.get_mut(&n.entry.parent) {
            p.children.remove(&n.entry.name);
        }
    });
    sh.write_state().snapshot_complete = false;
    h.engine.drop_connection();
    h.wait_for("resync", || fake.with(|fs| fs.sessions) >= 2);
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    assert!(h.engine.item(id).is_ok(), "in-flight id kept");
    drop(_g);
}

#[test]
fn status_and_idle() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let s = h.engine.status();
    assert_eq!(s.state, ConnState::Live);
    assert_eq!(s.entries, 7);
    assert_eq!(s.server.map(|i| i.root_path), Some(ROOT_PATH.to_string()));
    assert_eq!(s.anchor, h.engine.anchor());
    h.engine.wait_idle(Duration::from_secs(5)).expect("idle");
}

fn percentile(mut v: Vec<Duration>, p: f64) -> Duration {
    v.sort();
    v[((v.len() as f64 - 1.0) * p) as usize]
}

/// T1/T2 (engine API): `cargo test -p unlatch-core --release -- --ignored perf_t1_t2 --nocapture`.
#[test]
#[ignore]
fn perf_t1_t2() {
    let fake = FakeServer::new();
    fake.with(|fs| {
        fs.vm_mkdir("big");
        for i in 0..1000 {
            fs.vm_write(&format!("big/file-{i:04}.rs"), b"x");
        }
    });
    let h = start(&fake);
    let big = h.id("big");
    let ids: Vec<ItemId> = h
        .engine
        .list(big, None, 1000, false)
        .expect("list")
        .items
        .iter()
        .map(|i| i.entry.id)
        .collect();
    assert_eq!(ids.len(), 1000);
    let mut t1 = Vec::new();
    for _ in 0..500 {
        let t = Instant::now();
        let p = h.engine.list(big, None, 1000, false).expect("list");
        t1.push(t.elapsed());
        assert_eq!(p.items.len(), 1000);
    }
    let mut t2 = Vec::new();
    for i in 0..20_000 {
        let id = ids[i % ids.len()];
        let t = Instant::now();
        let it = h.engine.item(id).expect("item");
        t2.push(t.elapsed());
        std::hint::black_box(it);
    }
    let (p50_1, p99_1) = (percentile(t1.clone(), 0.5), percentile(t1, 0.99));
    let (p50_2, p99_2) = (percentile(t2.clone(), 0.5), percentile(t2, 0.99));
    println!("T1 list(1000): p50 {p50_1:?} p99 {p99_1:?}   T2 item: p50 {p50_2:?} p99 {p99_2:?}");
    assert!(p50_1 <= Duration::from_micros(500), "T1 p50 {p50_1:?}");
    assert!(p50_2 <= Duration::from_micros(20), "T2 p50 {p50_2:?}");
}

#[test]
fn working_set_signals_are_coalesced() {
    let fake = FakeServer::new();
    tree(&fake);
    let fake2 = fake.clone();
    let dir = tempfile::tempdir().expect("tempdir");
    let times: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let t2 = times.clone();
    let handler: EventHandler = Arc::new(move |e| {
        if matches!(e, EngineEvent::WorkingSetChanged { .. }) {
            t2.lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(Instant::now());
        }
    });
    let inner =
        Inner::start_with(cfg(dir.path()), Some(handler), fake2.opener(), timing()).expect("start");
    let engine = Engine { inner };
    engine.wait_live(Duration::from_secs(10)).expect("live");
    let a0 = engine.anchor();
    times.lock().unwrap_or_else(|p| p.into_inner()).clear();
    for i in 0..300 {
        fake.with(|fs| fs.vm_write(&format!("burst-{i}.txt"), b"x"));
    }
    engine
        .server_barrier(Duration::from_secs(5))
        .expect("barrier");
    let t = times.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert!(!t.is_empty());
    assert!(
        t.len() < 300,
        "coalesced: {} signals for 300 batches",
        t.len()
    );
    for w in t.windows(2) {
        assert!(
            w[1].duration_since(w[0]) >= Duration::from_millis(4),
            "≤ 1 signal per 5 ms"
        );
    }
    let mut a = a0;
    let mut n = 0;
    loop {
        let c = engine.changes_since(&a, 64).expect("changes");
        n += c.updated.len();
        a = c.anchor;
        if !c.more {
            break;
        }
    }
    assert_eq!(n, 300);
    engine.shutdown();
}

#[test]
fn commit_during_enumeration_signals_again() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let a = h.engine.anchor();
    let sh = h.engine.inner.shared.clone();
    h.barrier();
    let c0 = sh.committed.load(std::sync::atomic::Ordering::Acquire);
    let consumed0 = sh.read_state().consumed;
    // The interleaving under test: changes_since reads `committed`, then a commit lands before
    // it finishes. Holding the state lock parks it after that read, but nothing observable says
    // it got there before the commit below. Its returned anchor does: it enumerates up to the
    // `committed` it read, so `c0` means it read before the commit, `c0 + 1` after (a run
    // that tests nothing, retried). The re-signal is only asserted on a verified interleaving.
    for attempt in 1u64..=20 {
        h.clear_events();
        let guard = sh.write_state();
        let e2 = h.engine.clone();
        let a2 = a.clone();
        let t = std::thread::spawn(move || e2.changes_since(&a2, 100));
        // A head start only makes the wanted interleaving likely; the anchor check decides.
        std::thread::sleep(Duration::from_millis(20 * attempt));
        sh.committed
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel); // a commit lands meanwhile
        drop(guard);
        let c = t.join().expect("join").expect("changes");
        let (_, to) = super::decode_anchor(&c.anchor).expect("anchor");
        if to == c0 {
            h.wait_for("re-signal", || {
                h.events()
                    .iter()
                    .any(|e| matches!(e, EngineEvent::WorkingSetChanged { .. }))
            });
            sh.committed
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            return;
        }
        assert_eq!(to, c0 + 1, "changes_since saw the commit");
        sh.committed
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        sh.write_state().consumed = consumed0;
    }
    panic!("changes_since never read `committed` before the commit in 20 attempts");
}

#[test]
fn big_upload_under_credit() {
    let fake = FakeServer::new();
    let h = start(&fake);
    let big: Vec<u8> = (0..(3 << 20) + 17).map(|i| (i * 7 % 256) as u8).collect();
    let m = h
        .create_file(ItemId::ROOT, "up.bin", &big, "t-big")
        .expect("create");
    assert_eq!(m.item.entry.size, big.len() as u64);
    assert_eq!(fake.with(|fs| fs.content_of("up.bin")), Some(big.clone()));
    // Empty file: one empty last chunk.
    let m = h
        .create_file(ItemId::ROOT, "empty", b"", "t-empty")
        .expect("create");
    assert_eq!(m.item.entry.size, 0);
    assert_eq!(fake.with(|fs| fs.content_of("empty")), Some(vec![]));
    assert_eq!(h.engine.status().pending_uploads, 0);
}

#[test]
fn index_change_with_many_downloads_pauses_first() {
    let fake = FakeServer::new();
    let mut ids = Vec::new();
    fake.with(|fs| {
        for i in 0..40 {
            ids.push(fs.vm_write(&format!("f{i}"), b"x"));
        }
    });
    let h = start(&fake);
    h.engine.materialized_changed(&ids, &[], true).expect("mat");
    fake.with(|fs| fs.index = IndexId(0x7777));
    h.engine.drop_connection();
    h.wait_for("paused", || {
        matches!(h.engine.status().state, ConnState::Paused { .. })
    });
    assert!(
        h.engine.item(ids[0]).is_ok(),
        "nothing wiped before the user confirms"
    );
    h.engine.confirm_paused(true).expect("confirm");
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    h.wait_for("reimport", || {
        h.events().contains(&EngineEvent::Reimport {
            below: ItemId::ROOT,
        })
    });
    assert_eq!(h.names(ItemId::ROOT).len(), 40);
}

#[test]
fn ops_never_cross_index_ids() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let sh = h.engine.inner.shared.clone();
    let old = sh.read_state().index;
    assert!(sh.session_for(old, Duration::from_secs(1)).is_ok());
    let e = sh
        .session_for(Some(IndexId(1)), Duration::from_secs(1))
        .err()
        .expect("refused");
    assert_eq!(e.code, ErrorCode::IndexChanged);
}

#[test]
fn client_name_sanitized() {
    assert_eq!(
        super::sanitize_client_name("Sudhanshu’s MacBook Pro"),
        "Sudhanshu’s MacBook Pro"
    );
    assert_eq!(super::sanitize_client_name("a/b\0c\nd"), "abcd");
    assert_eq!(super::sanitize_client_name(""), "mac");
    let long = "é".repeat(40);
    let s = super::sanitize_client_name(&long);
    assert!(s.len() <= 32 && s.chars().all(|c| c == 'é'));
}

#[test]
fn anchor_encoding() {
    let a = super::encode_anchor(0xDEAD_BEEF, 42);
    assert_eq!(super::decode_anchor(&a), Some((0xDEAD_BEEF, 42)));
    assert_eq!(super::decode_anchor(&a[..10]), None);
}

#[test]
fn mass_delete_min_is_configurable() {
    // 3 of 5 downloaded files vanish: under the default floor (32) that is not a mass deletion;
    // with `mass_delete_min = 2` the fraction rule (60 % > 20 %) pauses it.
    for (min, pauses) in [(32u64, false), (2, true)] {
        let fake = FakeServer::new();
        let mut ids = Vec::new();
        fake.with(|fs| {
            fs.vm_mkdir("few");
            for i in 0..3 {
                ids.push(fs.vm_write(&format!("few/f{i}"), b"x"));
            }
            ids.push(fs.vm_write("a.txt", b"a"));
            ids.push(fs.vm_write("b.txt", b"b"));
        });
        let h = start_in(&fake, tempfile::tempdir().expect("tempdir"), |c| {
            c.mass_delete_min = min
        });
        h.engine.wait_live(Duration::from_secs(10)).expect("live");
        h.engine.materialized_changed(&ids, &[], true).expect("mat");
        fake.with(|fs| fs.vm_rm("few"));
        if pauses {
            h.wait_for("paused", || {
                matches!(h.engine.status().state, ConnState::Paused { .. })
            });
            assert!(
                h.engine.item(ids[0]).is_ok(),
                "nothing removed while paused"
            );
            h.engine.confirm_paused(true).expect("confirm");
            h.wait_for("live", || h.engine.status().state == ConnState::Live);
        }
        h.barrier();
        assert_eq!(h.engine.status().state, ConnState::Live, "min={min}");
        assert!(h.engine.item(ids[0]).is_err(), "min={min}: removal applied");
    }
}

/// wire.rs "Credit measure": the engine grants the frame body length it received (compressed
/// size), so a highly compressible 50 MB read never lets the server run more than one window of
/// wire bytes ahead. Granting the decompressed length would hand the server ~50 MB of credit.
#[test]
fn compressible_read_keeps_one_window_of_wire_bytes() {
    let fake = FakeServer::new();
    let size = 50usize << 20;
    fake.with(|fs| fs.vm_write("zeros.bin", &vec![0u8; size]));
    let h = start(&fake);
    let id = h.id("zeros.bin");
    let granted0 = fake.with(|fs| fs.credit_granted);
    let got = h.fetch(id);
    assert_eq!(got.len(), size);
    assert!(got.iter().all(|b| *b == 0));
    let (max_balance, wire, granted) = fake.with(|fs| {
        (
            fs.max_credit_balance,
            fs.read_wire_bytes,
            fs.credit_granted - granted0,
        )
    });
    // The data really was compressed on the wire (else the test proves nothing).
    assert!(wire < (size as u64) / 20, "wire bytes {wire}");
    let window = super::session::MAX_WINDOW as i64;
    assert!(
        max_balance <= window + frame_slack(),
        "server credit balance reached {max_balance} (window {window})"
    );
    // Grants track wire bytes, not the 50 MB the chunks decode to.
    assert!(
        granted <= wire + window as u64,
        "granted {granted} for {wire} wire bytes"
    );
}

/// One bulk frame of slack (a frame may take the balance below zero by at most itself).
fn frame_slack() -> i64 {
    unlatch_proto::frame::BULK_CHUNK as i64 + 64
}

fn create_req(h: &H, name: &str, content: &[u8], template: &str) -> CreateRequest {
    CreateRequest {
        template_id: template.into(),
        parent: ItemId::ROOT,
        name: name.into(),
        kind: CreateKind::File,
        content: Some(h.file(content)),
        symlink_target: None,
        mtime_ns: None,
        user_exec: None,
        changed_fields: fields::CONTENTS | fields::FILENAME,
        local: LocalMeta::default(),
        may_already_exist: false,
        deletion_conflicted: false,
    }
}

fn check_progress(calls: &[(u64, u64)], total: u64) {
    assert!(calls.len() >= 2, "{calls:?}");
    assert_eq!(calls[0], (0, total));
    assert_eq!(*calls.last().expect("last"), (total, total));
    assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0), "monotonic");
    assert!(calls.iter().all(|c| c.1 == total));
}

#[test]
fn create_and_modify_report_upload_progress() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let data: Vec<u8> = (0..(1 << 20) + 17).map(|i| (i % 253) as u8).collect();
    let calls = Mutex::new(Vec::new());
    let m = h
        .engine
        .create_with(
            create_req(&h, "up.bin", &data, "tmpl-up"),
            &|d, t| calls.lock().expect("lock").push((d, t)),
            &CancelToken::new(),
        )
        .expect("create");
    let c = calls.lock().expect("lock").clone();
    check_progress(&c, data.len() as u64);
    // One call per 64 KiB chunk plus the initial (0, total).
    assert!(c.len() >= 17, "{} progress calls", c.len());
    assert_eq!(fake.with(|fs| fs.content_of("up.bin")), Some(data.clone()));

    let id = m.item.entry.id;
    let base: BaseVersion = m.item.entry.version.into();
    let data2: Vec<u8> = data.iter().rev().copied().collect();
    let calls = Mutex::new(Vec::new());
    h.engine
        .modify_with(
            id,
            base,
            ModifyRequest {
                changed_fields: fields::CONTENTS,
                content: Some(h.file(&data2)),
                ..Default::default()
            },
            &|d, t| calls.lock().expect("lock").push((d, t)),
            &CancelToken::new(),
        )
        .expect("modify");
    check_progress(&calls.lock().expect("lock"), data2.len() as u64);
    assert_eq!(fake.with(|fs| fs.content_of("up.bin")), Some(data2));

    // Metadata-only and directory calls report no progress.
    let calls = Mutex::new(Vec::new());
    let mut dir = create_req(&h, "d", b"", "tmpl-d");
    dir.kind = CreateKind::Dir;
    dir.content = None;
    h.engine
        .create_with(
            dir,
            &|d, t| calls.lock().expect("lock").push((d, t)),
            &CancelToken::new(),
        )
        .expect("mkdir");
    assert!(calls.lock().expect("lock").is_empty());
}

#[test]
fn create_and_modify_honour_cancel() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let data: Vec<u8> = (0..(2 << 20)).map(|i| (i % 249) as u8).collect();

    // Cancelled before anything is sent: nothing reaches the VM.
    let tok = CancelToken::new();
    tok.cancel();
    let e = h
        .engine
        .create_with(create_req(&h, "c.bin", &data, "tmpl-c"), &|_, _| {}, &tok)
        .expect_err("cancelled");
    assert_eq!(e.code, ErrorCode::Cancelled);
    assert_eq!(h.requests(|r| matches!(r, Request::Write { .. })), 0);

    // Cancelled mid-upload (after the second chunk): the Write is aborted, no file appears.
    let tok = CancelToken::new();
    let e = h
        .engine
        .create_with(
            create_req(&h, "c.bin", &data, "tmpl-c"),
            &|d, _| {
                if d >= 2 * unlatch_proto::frame::BULK_CHUNK as u64 {
                    tok.cancel()
                }
            },
            &tok,
        )
        .expect_err("cancelled");
    assert_eq!(e.code, ErrorCode::Cancelled);
    h.barrier();
    assert!(fake.with(|fs| fs.resolve("c.bin")).is_none());
    assert!(h.engine.lookup(ItemId::ROOT, "c.bin").is_err());
    h.engine.wait_idle(Duration::from_secs(5)).expect("idle");

    // The system retries the same template: it goes through.
    h.engine
        .create_with(
            create_req(&h, "c.bin", &data, "tmpl-c"),
            &|_, _| {},
            &CancelToken::new(),
        )
        .expect("retry");
    assert_eq!(fake.with(|fs| fs.content_of("c.bin")), Some(data.clone()));

    // A cancelled modify leaves the VM's content alone.
    let id = h.id("c.bin");
    let base: BaseVersion = h.engine.item(id).expect("item").entry.version.into();
    let tok = CancelToken::new();
    let e = h
        .engine
        .modify_with(
            id,
            base,
            ModifyRequest {
                changed_fields: fields::CONTENTS,
                content: Some(h.file(b"replacement")),
                ..Default::default()
            },
            &|_, _| tok.cancel(),
            &tok,
        )
        .expect_err("cancelled");
    assert_eq!(e.code, ErrorCode::Cancelled);
    h.barrier();
    assert_eq!(fake.with(|fs| fs.content_of("c.bin")), Some(data));
}

#[test]
fn engine_name_is_the_domain() {
    let fake = FakeServer::new();
    let h = start(&fake);
    assert_eq!(h.engine.name(), "test");
    // The IPC Hello check uses it: the right domain is welcomed, any other is refused.
    let hello = |domain: &str| {
        let mut out = Vec::new();
        crate::ipc::dispatch(
            &h.engine,
            unlatch_proto::ipc::IpcFrame {
                call: 1,
                msg: unlatch_proto::ipc::IpcRequest::Hello {
                    proto: unlatch_proto::PROTO_VERSION,
                    domain: domain.into(),
                },
            },
            None,
            &mut |f| out.push(f.msg),
        );
        out
    };
    assert!(matches!(
        hello("test").as_slice(),
        [unlatch_proto::ipc::IpcResponse::Hello { domain, .. }] if domain == "test"
    ));
    assert!(matches!(
        hello("elsewhere").as_slice(),
        [unlatch_proto::ipc::IpcResponse::Error {
            code: ErrorCode::Protocol,
            ..
        }]
    ));
}

/// A failed replica commit (Mac disk full) must never lose the batch: the delta stays pending
/// and is retried, no anchor is published for it (D5), waiters are not told it is durable, the
/// status says so, and a later successful commit can never persist `server_seq` past rows that
/// were not written (restart must still show the file).
#[test]
fn failed_replica_commit_is_retried_and_never_lost() {
    use std::sync::atomic::Ordering;
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    h.barrier();
    let sh = h.engine.inner.shared.clone();
    let anchor0 = sh.committed.load(Ordering::Acquire);
    let writes0 = sh.replica_writes.load(Ordering::Acquire);

    // 1. Writes keep failing: memory has the file, but nothing is published as committed.
    sh.fail_replica_writes.store(u64::MAX, Ordering::Release);
    fake.with(|fs| fs.vm_write("lost.txt", b"l"));
    h.wait_for("lost.txt in memory", || {
        h.engine.lookup(ItemId::ROOT, "lost.txt").is_ok()
    });
    h.wait_for("an Offline-like status", || {
        matches!(h.engine.status().state, ConnState::Offline { .. })
    });
    let ConnState::Offline { error, .. } = h.engine.status().state else {
        unreachable!()
    };
    assert!(error.contains("disk"), "{error}");
    h.wait_for("the failed commit to be retried", || {
        sh.replica_writes.load(Ordering::Acquire) > writes0 + 1
    });
    assert_eq!(
        sh.committed.load(Ordering::Acquire),
        anchor0,
        "no anchor is published for an uncommitted batch (D5)"
    );
    assert!(
        h.engine.server_barrier(Duration::from_millis(300)).is_err(),
        "a barrier must not report the batch as committed"
    );

    // 2. Space frees up: the pending batch commits on its own (no new VM traffic needed).
    sh.fail_replica_writes.store(0, Ordering::Release);
    h.wait_for("the retried commit", || {
        sh.committed.load(Ordering::Acquire) > anchor0
    });
    h.engine
        .wait_live(Duration::from_secs(5))
        .expect("live again");
    h.barrier();

    // 3. One failure, then a later change commits fine (the reviewer's scenario).
    sh.fail_replica_writes.store(1, Ordering::Release);
    fake.with(|fs| fs.vm_write("lost2.txt", b"l"));
    h.wait_for("lost2.txt in memory", || {
        h.engine.lookup(ItemId::ROOT, "lost2.txt").is_ok()
    });
    h.wait_for("the fault consumed", || {
        sh.fail_replica_writes.load(Ordering::Acquire) == 0
    });
    fake.with(|fs| fs.vm_write("later.txt", b"x"));
    h.wait_for("later.txt", || {
        h.engine.lookup(ItemId::ROOT, "later.txt").is_ok()
    });
    h.barrier();
    drop(sh);

    let h = h.restart();
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    h.barrier();
    for n in ["lost.txt", "lost2.txt", "later.txt"] {
        assert!(
            h.engine.lookup(ItemId::ROOT, n).is_ok(),
            "{n} missing after restart"
        );
    }
}

/// The link drops while the mass-deletion guard waits for the user (held `Events`). The new
/// session must not hang in its handshake behind the held batch (no reader, no pings, every op
/// Offline, unlatchd queueing for a session nobody reads), and answering "Keep My Files" once
/// must be enough: the Resume replay of the same tombstones must not ask again.
#[test]
fn reconnect_while_paused_is_serviced_and_asks_once() {
    let fake = FakeServer::new();
    let mut ids = Vec::new();
    fake.with(|fs| {
        fs.vm_mkdir("big");
        for i in 0..60 {
            ids.push(fs.vm_write(&format!("big/f{i}"), b"x"));
        }
    });
    let h = start(&fake);
    h.engine
        .materialized_changed(&ids, &[], true)
        .expect("materialized");
    fake.with(|fs| fs.vm_rm("big"));
    h.wait_for("paused", || {
        matches!(h.engine.status().state, ConnState::Paused { .. })
    });
    let sessions0 = fake.with(|fs| fs.sessions);
    h.engine.drop_connection();
    h.wait_for("a new session", || fake.with(|fs| fs.sessions) > sessions0);
    // The new session runs while the question is open: pings flow and a barrier completes.
    let pings = || h.requests(|r| matches!(r, Request::Ping { .. }));
    let p0 = pings();
    h.wait_for("pings on the new session", || pings() >= p0 + 3);
    h.engine
        .server_barrier(Duration::from_secs(3))
        .expect("barrier on the new session while paused");
    h.wait_for("still asking", || {
        matches!(h.engine.status().state, ConnState::Paused { .. })
    });
    // One answer is enough.
    h.engine.confirm_paused(false).expect("keep my files");
    h.wait_for("live", || h.engine.status().state == ConnState::Live);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        h.engine.status().state,
        ConnState::Live,
        "the replayed removals asked a second time"
    );
    assert!(h.engine.item(ids[0]).is_ok(), "kept files are still there");
    // A replay of the declined tombstones (a Resume from a Hello built before the answer, e.g.
    // a reconnect racing the click) neither removes the kept files nor asks again.
    let sh = h.engine.inner.shared.clone();
    let server_seq = sh.read_state().server_seq;
    let big = h.engine.item(ids[0]).expect("f0").entry.parent;
    sh.apply(super::applier::ApplyMsg::Events {
        seq: server_seq,
        changes: vec![unlatch_proto::wire::Change::Remove {
            id: big,
            seq: server_seq,
        }],
    });
    h.engine.wait_idle(Duration::from_secs(5)).expect("idle");
    assert_eq!(h.engine.status().state, ConnState::Live, "asked twice");
    assert!(h.engine.item(ids[0]).is_ok(), "replay removed kept files");
    drop(sh);
    fake.with(|fs| fs.vm_write("after.txt", b"y"));
    h.wait_for("later changes flow", || {
        h.engine.lookup(ItemId::ROOT, "after.txt").is_ok()
    });
    // A restart does not ask again either (the resume point moved past the declined batch).
    let h = h.restart();
    h.engine
        .wait_live(Duration::from_secs(10))
        .expect("live after restart");
    assert!(h.engine.item(ids[0]).is_ok());
}

/// A file larger than the whole cache budget must still open (File Provider fetch and FUSE
/// read): the download is pinned as it enters the cache, so its own insert cannot evict it.
/// It stays over budget only until something else is cached.
#[test]
fn file_larger_than_the_cache_budget_still_opens() {
    let fake = FakeServer::new();
    let big: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let id = fake.with(|fs| fs.vm_write("big.bin", &big));
    fake.with(|fs| fs.vm_write("small.txt", b"s"));
    let h = start_in(&fake, tempfile::tempdir().expect("tempdir"), |c| {
        c.cache_budget = 1024
    });
    h.engine.wait_live(Duration::from_secs(10)).expect("live");
    assert_eq!(h.fetch(id), big, "File Provider fetch");
    assert_eq!(h.fetch(id), big, "and again");
    assert_eq!(
        h.engine.read(id, 4000, 200).expect("FUSE read"),
        big[4000..].to_vec()
    );
    // Caching something else brings the cache back under budget.
    let small = h.id("small.txt");
    assert_eq!(h.fetch(small), b"s");
    assert!(h.engine.status().cache_bytes <= 1024);
}

/// The host holds a `WorkingSetChanged` whose anchor covers `id` (MQ-013: the system believes a
/// reply at once; a conflict copy is only ever learnt through the working set).
fn signalled_covering(h: &H, id: ItemId) -> bool {
    h.events().iter().any(|e| match e {
        EngineEvent::WorkingSetChanged { anchor } => h
            .engine
            .changes_since(anchor, 1000)
            .is_ok_and(|c| !c.updated.iter().any(|i| i.entry.id == id)),
        _ => false,
    })
}

/// fpsim `mq013_returned_version_is_believed` (flaky on CI: the copy was "on the VM, missing on
/// the Mac" after the reply): a modify that keeps the Mac's bytes as a conflict copy must have
/// the working-set signal with the host before the reply returns. Before the fix the signal was
/// only requested, so a slow signal/event thread let the reply overtake it; a slow host makes
/// that ordering certain here.
#[test]
fn conflict_copy_is_signalled_before_the_modify_reply() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    let id = h.id("README.md");
    let base: BaseVersion = h.engine.item(id).expect("item").entry.version.into();
    fake.with(|fs| fs.vm_write("README.md", b"agent"));
    h.barrier();
    h.clear_events();
    h.slow_host_ms
        .store(150, std::sync::atomic::Ordering::Release);
    let m = h
        .engine
        .modify(
            id,
            base,
            ModifyRequest {
                changed_fields: fields::CONTENTS,
                content: Some(h.file(b"mac")),
                ..Default::default()
            },
        )
        .expect("modify");
    let copy = m.conflict_copy.expect("conflict copy").entry.id;
    assert!(m.should_fetch_content);
    assert!(
        signalled_covering(&h, copy),
        "the modify reply left before the host had the signal for its conflict copy: {:?}",
        h.events()
    );
    h.slow_host_ms
        .store(0, std::sync::atomic::Ordering::Release);
}

/// The same for a create that finds other bytes at its path (`may_already_exist`).
#[test]
fn create_conflict_copy_is_signalled_before_the_reply() {
    let fake = FakeServer::new();
    tree(&fake);
    let h = start(&fake);
    h.clear_events();
    h.slow_host_ms
        .store(150, std::sync::atomic::Ordering::Release);
    let m = h
        .engine
        .create(CreateRequest {
            template_id: "t-reimport".into(),
            parent: ItemId::ROOT,
            name: "README.md".into(),
            kind: CreateKind::File,
            content: Some(h.file(b"local version")),
            symlink_target: None,
            mtime_ns: None,
            user_exec: None,
            changed_fields: fields::CONTENTS,
            local: LocalMeta::default(),
            may_already_exist: true,
            deletion_conflicted: false,
        })
        .expect("create");
    let copy = m.conflict_copy.expect("conflict copy").entry.id;
    assert!(
        signalled_covering(&h, copy),
        "the create reply left before the host had the signal for its conflict copy: {:?}",
        h.events()
    );
    h.slow_host_ms
        .store(0, std::sync::atomic::Ordering::Release);
}
