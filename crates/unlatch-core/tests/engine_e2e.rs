//! End-to-end: the engine's public API against a real `unlatchd stdio` via `Transport::Command`.
//!
//! Needs the `unlatchd` binary: `UNLATCHD_BIN`, else `<target>/<profile>/unlatchd` next to this test's
//! `deps/` dir (build it first: `cargo build -p unlatchd`). Skipped (with a message) when missing.

use unlatch_core::*;
use unlatch_proto::ipc::{fields, ConnState, LocalMeta};
use unlatch_proto::{BaseVersion, ErrorCode, ItemId, Kind};

type Result<T> = std::result::Result<T, unlatch_proto::ProtoError>;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn unlatchd() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("UNLATCHD_BIN") {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let p = exe.parent()?.parent()?.join("unlatchd");
    p.exists().then_some(p)
}

struct E2e {
    engine: Engine,
    root: PathBuf,
    work: PathBuf,
    events: Arc<Mutex<Vec<EngineEvent>>>,
    _tmp: tempfile::TempDir,
}

fn start() -> Option<E2e> {
    let Some(bin) = unlatchd() else {
        eprintln!("SKIP: unlatchd binary not found (set UNLATCHD_BIN or cargo build -p unlatchd)");
        return None;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("root");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::create_dir_all(&work).expect("mkdir");
    std::fs::write(root.join("README.md"), b"# hello").expect("write");
    std::fs::write(root.join("src/main.rs"), b"fn main() {}").expect("write");
    Some(start_at(bin, tmp, root, work))
}

fn start_at(bin: PathBuf, tmp: tempfile::TempDir, root: PathBuf, work: PathBuf) -> E2e {
    let argv = vec![
        bin.display().to_string(),
        "stdio".into(),
        "--root".into(),
        root.display().to_string(),
        "--state".into(),
        tmp.path().join("unlatchd-state").display().to_string(),
    ];
    let mut cfg = EngineConfig::new(
        "e2e",
        Transport::Command {
            argv,
            env: vec![("UNLATCHD_DEBOUNCE_MS".into(), "2".into())],
        },
        &root.display().to_string(),
        tmp.path().join("engine"),
        "Test Mac",
    );
    cfg.prefetch.max_file = 0;
    let events: Arc<Mutex<Vec<EngineEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let ev = events.clone();
    let engine = Engine::start(
        cfg,
        Some(Arc::new(move |e| {
            ev.lock().unwrap_or_else(|p| p.into_inner()).push(e)
        })),
    )
    .expect("engine start");
    engine.wait_live(Duration::from_secs(20)).expect("live");
    // The tests below resume against this daemon's index. A fresh index is checkpointed by the
    // daemon's actor just after Welcome; a daemon killed before that (every reconnect kills the
    // `stdio` daemon) loses it by design and the next one builds a new index whose ids never
    // match the old ones (review (c)7) — a reimport, not the resume these tests measure.
    let index = tmp.path().join("unlatchd-state").join("index.bin");
    let t = Instant::now();
    while !index.exists() {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "the daemon never checkpointed its index"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    E2e {
        engine,
        root,
        work,
        events,
        _tmp: tmp,
    }
}

impl E2e {
    fn names(&self, dir: ItemId) -> Vec<String> {
        self.engine
            .list(dir, None, 1000, false)
            .expect("list")
            .items
            .into_iter()
            .map(|i| i.display_name)
            .collect()
    }

    fn wait_visible(&self, parent: ItemId, name: &str) -> unlatch_proto::ipc::IpcItem {
        let t = Instant::now();
        loop {
            if let Ok(it) = self.engine.lookup(parent, name) {
                return it;
            }
            assert!(
                t.elapsed() < Duration::from_secs(10),
                "{name} never became visible"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn file(&self, content: &[u8]) -> std::fs::File {
        let p = self.work.join(format!("c-{}", rand_u64()));
        std::fs::File::create(&p)
            .and_then(|mut f| f.write_all(content))
            .expect("write");
        std::fs::File::open(&p).expect("open")
    }

    fn fetch(&self, id: ItemId) -> Vec<u8> {
        let f = self
            .engine
            .fetch(id, None, &self.work, &|_, _| {}, &CancelToken::new())
            .expect("fetch");
        std::fs::read(f.path).expect("read")
    }
}

fn rand_u64() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (t as u64) ^ (std::process::id() as u64) << 32
}

fn create(e: &E2e, parent: ItemId, name: &str, content: &[u8], template: &str) -> Modified {
    e.engine
        .create(CreateRequest {
            template_id: template.into(),
            parent,
            name: name.into(),
            kind: CreateKind::File,
            content: Some(e.file(content)),
            symlink_target: None,
            mtime_ns: None,
            user_exec: None,
            changed_fields: fields::CONTENTS,
            local: LocalMeta::default(),
            may_already_exist: false,
            deletion_conflicted: false,
        })
        .expect("create")
}

#[test]
fn e2e_browse_push_fetch_and_mutate() {
    let Some(e) = start() else { return };
    assert_eq!(e.names(ItemId::ROOT), vec!["README.md", "src"]);
    let src = e.engine.lookup(ItemId::ROOT, "src").expect("src").entry.id;
    assert_eq!(e.names(src), vec!["main.rs"]);
    let readme = e.engine.lookup(ItemId::ROOT, "README.md").expect("readme");
    assert_eq!(e.fetch(readme.entry.id), b"# hello");

    // Agent writes on the VM → pushed, visible, signalled.
    let t = Instant::now();
    std::fs::write(e.root.join("src/agent.rs"), b"// agent").expect("write");
    let it = e.wait_visible(src, "agent.rs");
    eprintln!("VM write → visible in engine: {:?}", t.elapsed());
    assert_eq!(e.fetch(it.entry.id), b"// agent");
    assert!(e
        .events
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .any(|ev| matches!(ev, EngineEvent::WorkingSetChanged { .. })));

    // Mac creates: lands on disk; a same-name create never errors.
    let m = create(&e, src, "mac.rs", b"// mac", "tmpl-1");
    assert_eq!(
        std::fs::read(e.root.join("src/mac.rs")).expect("read"),
        b"// mac"
    );
    let m2 = create(&e, src, "mac.rs", b"// different", "tmpl-2");
    assert_eq!(m2.item.display_name, "mac 2.rs");
    assert_eq!(
        std::fs::read(e.root.join("src/mac 2.rs")).expect("read"),
        b"// different"
    );
    // Replay of the first create (same template) → same item, no duplicate.
    let again = create(&e, src, "mac.rs", b"// mac", "tmpl-1");
    assert_eq!(again.item.entry.id, m.item.entry.id);

    // Modify content.
    let id = m.item.entry.id;
    let base: BaseVersion = m.item.entry.version.into();
    let r = e
        .engine
        .modify(
            id,
            base,
            ModifyRequest {
                changed_fields: fields::CONTENTS,
                content: Some(e.file(b"// v2")),
                ..Default::default()
            },
        )
        .expect("modify");
    assert!(!r.should_fetch_content && r.conflict_copy.is_none());
    assert_eq!(
        std::fs::read(e.root.join("src/mac.rs")).expect("read"),
        b"// v2"
    );

    // Conflict: agent rewrites, then the Mac saves on the stale base → agent bytes survive.
    let stale: BaseVersion = r.item.entry.version.into();
    std::fs::write(e.root.join("src/mac.rs"), b"// agent wins").expect("write");
    let t = Instant::now();
    loop {
        let cur = e.engine.item(id).expect("item");
        if cur.entry.version.content != r.item.entry.version.content {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "agent write not seen"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let c = e
        .engine
        .modify(
            id,
            stale,
            ModifyRequest {
                changed_fields: fields::CONTENTS,
                content: Some(e.file(b"// mac late")),
                ..Default::default()
            },
        )
        .expect("modify never errors on conflict");
    assert!(c.should_fetch_content);
    let copy = c.conflict_copy.expect("conflict copy");
    assert_eq!(
        std::fs::read(e.root.join("src/mac.rs")).expect("read"),
        b"// agent wins"
    );
    assert_eq!(
        std::fs::read(e.root.join("src").join(&copy.entry.name)).expect("read copy"),
        b"// mac late"
    );

    // Rename + delete.
    let cur = e.engine.item(id).expect("item");
    let r = e
        .engine
        .modify(
            id,
            cur.entry.version.into(),
            ModifyRequest {
                changed_fields: fields::FILENAME,
                new_name: Some("renamed.rs".into()),
                ..Default::default()
            },
        )
        .expect("rename");
    assert_eq!(r.item.display_name, "renamed.rs");
    assert!(e.root.join("src/renamed.rs").exists());
    e.engine
        .delete(id, r.item.entry.version.into(), false)
        .expect("delete");
    assert!(!e.root.join("src/renamed.rs").exists());
    e.engine
        .delete(id, r.item.entry.version.into(), false)
        .expect("unknown id → Ok");

    // Mac-only metadata: no VM change.
    let before = std::fs::metadata(e.root.join("README.md"))
        .expect("stat")
        .modified()
        .expect("mtime");
    let rd = e.engine.item(readme.entry.id).expect("item");
    let local = LocalMeta {
        tag_data: Some(vec![7]),
        ..Default::default()
    };
    let r = e
        .engine
        .modify(
            rd.entry.id,
            rd.entry.version.into(),
            ModifyRequest {
                changed_fields: fields::TAG_DATA,
                local: local.clone(),
                ..Default::default()
            },
        )
        .expect("tags");
    assert_eq!(r.item.local, local);
    assert_eq!(
        std::fs::metadata(e.root.join("README.md"))
            .expect("stat")
            .modified()
            .expect("mtime"),
        before
    );
    e.engine.shutdown();
}

#[test]
fn e2e_reconnect_resumes_and_catches_up() {
    let Some(e) = start() else { return };
    let src = e.engine.lookup(ItemId::ROOT, "src").expect("src").entry.id;
    e.engine.drop_connection();
    std::fs::write(e.root.join("src/while-away.rs"), b"x").expect("write");
    std::fs::remove_file(e.root.join("README.md")).expect("rm");
    let t = Instant::now();
    e.wait_visible(src, "while-away.rs");
    loop {
        if e.engine.lookup(ItemId::ROOT, "README.md").is_err() {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "removal not seen");
        std::thread::sleep(Duration::from_millis(2));
    }
    eprintln!("reconnect + catch-up: {:?}", t.elapsed());
    e.engine
        .wait_live(Duration::from_secs(10))
        .expect("live after catch-up");
    assert_eq!(e.engine.status().state, ConnState::Live);
    e.engine.shutdown();
}

#[test]
fn e2e_restart_offline_then_resume() {
    let Some(e) = start() else { return };
    let src = e.engine.lookup(ItemId::ROOT, "src").expect("src").entry.id;
    let E2e {
        engine,
        root,
        work,
        _tmp,
        ..
    } = e;
    engine.shutdown();
    drop(engine);
    std::fs::write(root.join("src/new.rs"), b"n").expect("write");
    let Some(bin) = unlatchd() else { return };
    let e = start_at(bin, _tmp, root, work);
    e.wait_visible(src, "new.rs");
    assert_eq!(e.engine.item(src).expect("item").entry.kind, Kind::Dir);
    e.engine.shutdown();
}

#[test]
fn e2e_lazy_dirs_and_big_file() {
    let Some(e) = start() else { return };
    let nm = e.root.join("node_modules/pkg");
    std::fs::create_dir_all(&nm).expect("mkdir");
    std::fs::write(nm.join("index.js"), b"module.exports = 1").expect("write");
    let big: Vec<u8> = (0..(3 << 20)).map(|i| (i % 253) as u8).collect();
    std::fs::write(e.root.join("big.bin"), &big).expect("write");
    let nmi = e.wait_visible(ItemId::ROOT, "node_modules");
    assert_eq!(e.names(nmi.entry.id), vec!["pkg"]);
    let pkg = e.engine.lookup(nmi.entry.id, "pkg").expect("pkg").entry.id;
    assert_eq!(e.names(pkg), vec!["index.js"]);
    let b = e.wait_visible(ItemId::ROOT, "big.bin");
    let t = Instant::now();
    loop {
        if e.engine.item(b.entry.id).expect("item").entry.size == big.len() as u64 {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(e.fetch(b.entry.id), big);
    assert_eq!(
        e.engine.read(b.entry.id, 1000, 10).expect("read"),
        big[1000..1010].to_vec()
    );
    let err = e
        .engine
        .list(b.entry.id, None, 10, false)
        .expect_err("not a dir");
    assert_eq!(err.code, ErrorCode::NotDir);
    e.engine.shutdown();
}

fn percentile(mut v: Vec<Duration>, p: f64) -> Duration {
    v.sort();
    v[((v.len() as f64 - 1.0) * p) as usize]
}

/// T1/T2/T4 against real unlatchd: `cargo test --release -p unlatch-core --test engine_e2e -- --ignored --nocapture`.
#[test]
#[ignore]
fn e2e_perf() {
    let Some(e) = start() else { return };
    let dir = e.root.join("thousand");
    std::fs::create_dir_all(&dir).expect("mkdir");
    for i in 0..1000 {
        std::fs::write(dir.join(format!("f{i:04}.txt")), b"x").expect("write");
    }
    let d = e.wait_visible(ItemId::ROOT, "thousand").entry.id;
    let t = Instant::now();
    while e
        .engine
        .list(d, None, 2000, false)
        .expect("list")
        .items
        .len()
        < 1000
    {
        assert!(t.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut t1 = Vec::new();
    for _ in 0..300 {
        let t = Instant::now();
        let p = e.engine.list(d, None, 1000, false).expect("list");
        t1.push(t.elapsed());
        assert_eq!(p.items.len(), 1000);
    }
    let ids: Vec<ItemId> = e
        .engine
        .list(d, None, 1000, false)
        .expect("list")
        .items
        .iter()
        .map(|i| i.entry.id)
        .collect();
    let mut t2 = Vec::new();
    for i in 0..20_000 {
        let t = Instant::now();
        std::hint::black_box(e.engine.item(ids[i % ids.len()]).expect("item"));
        t2.push(t.elapsed());
    }
    let mut t4 = Vec::new();
    for i in 0..50 {
        let name = format!("t4-{i}.txt");
        let t = Instant::now();
        std::fs::write(e.root.join(&name), b"x").expect("write");
        e.wait_visible(ItemId::ROOT, &name);
        t4.push(t.elapsed());
    }
    println!(
        "E2E T1 list(1000) p50 {:?} p99 {:?} | T2 item p50 {:?} p99 {:?} | T4 VM write→visible (RTT 0) p50 {:?} p99 {:?}",
        percentile(t1.clone(), 0.5),
        percentile(t1, 0.99),
        percentile(t2.clone(), 0.5),
        percentile(t2, 0.99),
        percentile(t4.clone(), 0.5),
        percentile(t4, 0.99)
    );
    e.engine.shutdown();
}

/// Crash/replay matrix (review §2(f)3), wire hop: unlatchd exits right after durably recording the
/// op, before replying. The engine reports the failure; the system's replay (same call) must not
/// duplicate, conflict-copy or get stuck.
#[test]
fn e2e_die_after_commit_replays_cleanly() {
    let Some(bin) = unlatchd() else {
        eprintln!("SKIP: unlatchd binary not found");
        return;
    };
    for op in ["write", "mkdir", "rename", "remove"] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("root");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(&work).expect("mkdir");
        std::fs::write(root.join("f.txt"), b"one").expect("write");
        let state = tmp.path().join("unlatchd-state");
        let argv = vec![
            bin.display().to_string(),
            "stdio".into(),
            "--root".into(),
            root.display().to_string(),
            "--state".into(),
            state.display().to_string(),
        ];
        // Only the first unlatchd process dies: a marker file flips the env for later spawns.
        let wrapper = tmp.path().join("unlatchd-wrapper.sh");
        let marker = tmp.path().join("died-once");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nif [ -e '{m}' ]; then exec \"$@\"; fi\ntouch '{m}'\nUNLATCH_FAULT=die_after_commit:{op} exec \"$@\"\n",
                m = marker.display()
            ),
        )
        .expect("wrapper");
        let _ = std::process::Command::new("chmod")
            .arg("+x")
            .arg(&wrapper)
            .status();
        let mut full = vec![wrapper.display().to_string()];
        full.extend(argv);
        let mut cfg = EngineConfig::new(
            "e2e-crash",
            Transport::Command {
                argv: full,
                env: vec![],
            },
            &root.display().to_string(),
            tmp.path().join("engine"),
            "Test Mac",
        );
        cfg.prefetch.max_file = 0;
        let engine = Engine::start(cfg, None).expect("start");
        engine.wait_live(Duration::from_secs(20)).expect("live");
        let f = engine.lookup(ItemId::ROOT, "f.txt").expect("f");
        let e = E2e {
            engine,
            root: root.clone(),
            work,
            events: Arc::new(Mutex::new(vec![])),
            _tmp: tmp,
        };
        let run = |e: &E2e| -> Result<()> {
            match op {
                "write" => e
                    .engine
                    .create(CreateRequest {
                        template_id: "tmpl-crash".into(),
                        parent: ItemId::ROOT,
                        name: "new.txt".into(),
                        kind: CreateKind::File,
                        content: Some(e.file(b"payload")),
                        symlink_target: None,
                        mtime_ns: None,
                        user_exec: None,
                        changed_fields: fields::CONTENTS,
                        local: LocalMeta::default(),
                        may_already_exist: false,
                        deletion_conflicted: false,
                    })
                    .map(|_| ()),
                "mkdir" => e
                    .engine
                    .create(CreateRequest {
                        template_id: "tmpl-dir".into(),
                        parent: ItemId::ROOT,
                        name: "d".into(),
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
                    .map(|_| ()),
                "rename" => e
                    .engine
                    .modify(
                        f.entry.id,
                        f.entry.version.into(),
                        ModifyRequest {
                            changed_fields: fields::FILENAME,
                            new_name: Some("g.txt".into()),
                            ..Default::default()
                        },
                    )
                    .map(|_| ()),
                _ => e.engine.delete(f.entry.id, f.entry.version.into(), false),
            }
        };
        let first = run(&e);
        assert!(
            first.is_err(),
            "{op}: the reply was never sent, got {first:?}"
        );
        e.engine
            .wait_live(Duration::from_secs(20))
            .expect("reconnected");
        run(&e).unwrap_or_else(|err| panic!("{op}: replay failed: {err:?}"));
        let names: Vec<String> = {
            let mut v: Vec<String> = std::fs::read_dir(&root)
                .expect("ls")
                .flatten()
                .map(|d| d.file_name().to_string_lossy().into_owned())
                .filter(|n| !n.starts_with(".unlatch"))
                .collect();
            v.sort();
            v
        };
        let want: Vec<&str> = match op {
            "write" => vec!["f.txt", "new.txt"],
            "mkdir" => vec!["d", "f.txt"],
            "rename" => vec!["g.txt"],
            _ => vec![],
        };
        assert_eq!(names, want, "{op}: no duplicates, no conflict copies");
        e.engine.shutdown();
    }
}

/// Initial sync of 100k entries (T11 shape, RTT 0): `cargo test --release -p unlatch-core --test
/// engine_e2e e2e_initial_sync_100k -- --ignored --nocapture`.
#[test]
#[ignore]
fn e2e_initial_sync_100k() {
    let Some(bin) = unlatchd() else {
        eprintln!("SKIP: unlatchd binary not found");
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("root");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).expect("mkdir");
    for d in 0..1000 {
        let dir = root.join(format!("d{d:04}"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        for f in 0..99 {
            std::fs::write(dir.join(format!("f{f:02}.txt")), b"").expect("write");
        }
    }
    let t = Instant::now();
    let e = start_at(bin, tmp, root, work);
    let took = t.elapsed();
    let n = e.engine.status().entries;
    println!("initial sync: {n} entries live in {took:?}");
    assert!(n >= 100_000);
    let t = Instant::now();
    e.engine.shutdown();
    println!("shutdown (flush) {:?}", t.elapsed());
}

/// Real unlatchd charges bulk credit by the frame body as sent and the engine grants exactly that
/// (wire.rs "Credit measure"): a highly compressible 50 MB read must neither stall nor corrupt.
#[test]
fn e2e_compressible_50mb_read() {
    let Some(e) = start() else { return };
    let size = 50usize << 20;
    std::fs::write(e.root.join("zeros.bin"), vec![0u8; size]).expect("write");
    let it = e.wait_visible(ItemId::ROOT, "zeros.bin");
    let t = Instant::now();
    let got = e.fetch(it.entry.id);
    eprintln!("50 MB compressible fetch: {:?}", t.elapsed());
    assert_eq!(got.len(), size);
    assert!(got.iter().all(|b| *b == 0));
    e.engine.shutdown();
}

/// Upload progress and cancellation against real unlatchd (`Engine::create_with`/`modify_with`).
#[test]
fn e2e_upload_progress_and_cancel() {
    let Some(e) = start() else { return };
    let data: Vec<u8> = (0..(3u32 << 20))
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let req = |name: &str, template: &str, content: &[u8]| CreateRequest {
        template_id: template.into(),
        parent: ItemId::ROOT,
        name: name.into(),
        kind: CreateKind::File,
        content: Some(e.file(content)),
        symlink_target: None,
        mtime_ns: None,
        user_exec: None,
        changed_fields: fields::CONTENTS | fields::FILENAME,
        local: LocalMeta::default(),
        may_already_exist: false,
        deletion_conflicted: false,
    };
    // Cancelled after the first few chunks: unlatchd discards the staged upload.
    let tok = CancelToken::new();
    let r = e.engine.create_with(
        req("up.bin", "tmpl-up", &data),
        &|done, _| {
            if done >= 256 * 1024 {
                tok.cancel()
            }
        },
        &tok,
    );
    assert_eq!(r.expect_err("cancelled").code, ErrorCode::Cancelled);
    e.engine
        .server_barrier(Duration::from_secs(10))
        .expect("barrier");
    assert!(
        !e.root.join("up.bin").exists(),
        "cancelled upload published"
    );
    // The system retries the same template with progress: it lands, progress ends at 100 %.
    let seen = Mutex::new(Vec::new());
    let m = e
        .engine
        .create_with(
            req("up.bin", "tmpl-up", &data),
            &|d, t| seen.lock().expect("lock").push((d, t)),
            &CancelToken::new(),
        )
        .expect("create");
    let seen = seen.into_inner().expect("lock");
    let total = data.len() as u64;
    assert_eq!(seen.first(), Some(&(0, total)));
    assert_eq!(seen.last(), Some(&(total, total)));
    assert!(seen.len() >= 48, "{} progress calls", seen.len());
    assert_eq!(std::fs::read(e.root.join("up.bin")).expect("read"), data);
    // modify_with reports progress for the new content.
    let seen = Mutex::new(Vec::new());
    let base: BaseVersion = m.item.entry.version.into();
    e.engine
        .modify_with(
            m.item.entry.id,
            base,
            ModifyRequest {
                changed_fields: fields::CONTENTS,
                content: Some(e.file(b"short")),
                ..Default::default()
            },
            &|d, t| seen.lock().expect("lock").push((d, t)),
            &CancelToken::new(),
        )
        .expect("modify");
    assert_eq!(seen.into_inner().expect("lock").last(), Some(&(5, 5)));
    assert_eq!(
        std::fs::read(e.root.join("up.bin")).expect("read"),
        b"short"
    );
    e.engine.shutdown();
}
