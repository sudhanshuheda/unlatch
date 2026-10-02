//! Seeded random VM-side churn against one `unlatchd stdio` session — writes, appends, atomic
//! replaces, same-size churn, hard links across directories, renames (also over other files),
//! mkdir, `rm -rf`, symlinks, directory moves and swaps for a symlink, and daemon restarts
//! (clean and SIGKILL) — checked against the real tree after Ping barriers. Found: links
//! dirtied by a name retried after its directory moved were dropped, a write through a
//! short-lived hard link never reached the other link, and a barrier right after a restart ran
//! before the verify walk; a write through a link whose directory is removed, and a change
//! under a directory moved twice (nested) in one batch, were lost; a directory moved while the
//! startup verify walk was about to open it (or right after it was created) was never watched,
//! a journal replay lost a renamed-over file's inode, a link moved onto a name renamed over in
//! the same batch hid a write, and a link whose stat raced its unlink was taken for a move
//! (TESTING.md §5 #20–#23).
//!
//! Every check is two-way: each file and dir on disk is in the replica with its size, and
//! each replica entry reachable from the root is on disk with the same kind, has no sibling of
//! the same name, and is not published lazy unless lazy by name (or by a watch cap the run
//! set). Lookups by path fail on duplicate siblings instead of taking the first.
//!
//! Seeds: the default 0..60; `UNLATCHD_STRESS_SEED=<n>` runs one, `UNLATCHD_STRESS_SEEDS=<a>..<b>`
//! (or `<a>..=<b>`) a range. Every seed of a range runs; the failures are listed at the end and,
//! with `UNLATCHD_STRESS_FAIL_DIR=<dir>`, each one's panic (with its op log) is written to
//! `<dir>/seed-<n>.log` (the nightly workflow uploads them).

mod common;
use common::*;
use rand::{Rng, SeedableRng};
use std::collections::HashSet;
use std::path::Path;
use unlatch_proto::{Entry, ItemId, Kind};

fn files(r: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut st = vec![String::new()];
    while let Some(d) = st.pop() {
        for e in std::fs::read_dir(r.join(&d)).unwrap().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            let p = if d.is_empty() { n } else { format!("{d}/{n}") };
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                st.push(p.clone());
                out.push(p + "/");
            } else if ft.is_file() {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// The replica entry at `path`; a path component with two entries of that name under one
/// parent fails (taking the first would hide a stale twin).
fn lookup(c: &Client, path: &str, log: &[String]) -> Option<Entry> {
    let mut cur = ItemId::ROOT;
    for comp in path.split('/') {
        let hits: Vec<&Entry> = c
            .replica
            .values()
            .filter(|e| e.parent == cur && e.name == comp && e.id != cur)
            .collect();
        assert!(
            hits.len() <= 1,
            "duplicate replica entries at {path}: {hits:?}\n{}",
            log.join("\n")
        );
        cur = hits.first()?.id;
    }
    c.replica.get(&cur).cloned()
}

fn check(c: &mut Client, r: &Path, log: &[String]) {
    c.ping();
    for p in files(r) {
        if let Some(d) = p.strip_suffix('/') {
            assert!(
                lookup(c, d, log).is_some(),
                "dir {d} missing\n{}",
                log.join("\n")
            );
            continue;
        }
        let len = std::fs::metadata(r.join(&p)).unwrap().len();
        let e = lookup(c, &p, log);
        assert_eq!(e.map(|e| e.size), Some(len), "{p}\n{}", log.join("\n"));
    }
    strict(c, r, log);
}

/// The replica → disk direction: every entry is reachable from the root, exists on disk with
/// the same kind, has no sibling of the same name, and a dir is published lazy only when lazy
/// by name (or the run capped the watch budget).
fn strict(c: &Client, r: &Path, log: &[String]) {
    let lazy_names: HashSet<&str> = c.welcome().lazy_names.iter().map(|s| s.as_str()).collect();
    let budget_capped = std::env::var_os("UNLATCHD_MAX_WATCHES").is_some();
    let mut reached = 1usize;
    let mut st = vec![(ItemId::ROOT, String::new())];
    while let Some((d, path)) = st.pop() {
        let mut seen = HashSet::new();
        for k in c.children(d) {
            reached += 1;
            let p = if path.is_empty() {
                k.name.clone()
            } else {
                format!("{path}/{}", k.name)
            };
            assert!(
                seen.insert(k.name.clone()),
                "duplicate sibling {p}\n{}",
                log.join("\n")
            );
            let md = std::fs::symlink_metadata(r.join(&p));
            let same = match (&md, k.kind) {
                (Ok(m), Kind::Dir) => m.file_type().is_dir(),
                (Ok(m), Kind::File) => m.file_type().is_file(),
                (Ok(m), Kind::Symlink) => m.file_type().is_symlink(),
                _ => false,
            };
            assert!(
                same,
                "stale replica entry {p} ({:?}); on disk: {:?}\n{}",
                k.kind,
                md.map(|m| m.file_type()),
                log.join("\n")
            );
            if k.kind == Kind::Dir {
                assert!(
                    !k.lazy || lazy_names.contains(k.name.as_str()) || budget_capped,
                    "dir {p} published lazy\n{}",
                    log.join("\n")
                );
                st.push((k.id, p));
            }
        }
    }
    let orphans: Vec<&Entry> = c
        .replica
        .values()
        .filter(|e| e.id != ItemId::ROOT)
        .filter(|e| {
            // Reachable entries were counted above; anything else hangs off a missing parent.
            let mut cur = e.parent;
            for _ in 0..c.replica.len() {
                if cur == ItemId::ROOT {
                    return false;
                }
                match c.replica.get(&cur) {
                    Some(p) => cur = p.parent,
                    None => return true,
                }
            }
            true
        })
        .collect();
    assert!(
        orphans.is_empty() && reached == c.replica.len(),
        "replica entries unreachable from the root ({} of {}): {orphans:?}\n{}",
        c.replica.len() - reached.min(c.replica.len()),
        c.replica.len(),
        log.join("\n")
    );
}

/// Seeds to run: `UNLATCHD_STRESS_SEED`, else `UNLATCHD_STRESS_SEEDS` (`a..b` / `a..=b`), else 0..60.
fn seeds() -> Vec<u64> {
    if let Ok(s) = std::env::var("UNLATCHD_STRESS_SEED") {
        return vec![s.trim().parse().expect("UNLATCHD_STRESS_SEED")];
    }
    if let Ok(s) = std::env::var("UNLATCHD_STRESS_SEEDS") {
        let want = "UNLATCHD_STRESS_SEEDS: want <a>..<b> or <a>..=<b>";
        let num = |x: &str| -> u64 { x.parse().expect(want) };
        let (a, b) = s.trim().split_once("..").expect(want);
        return match b.strip_prefix('=') {
            Some(b) => (num(a)..=num(b)).collect(),
            None => (num(a)..num(b)).collect(),
        };
    }
    (0..60).collect()
}

#[test]
fn random_vm_churn_with_hardlinks_and_restarts_converges() {
    let fail_dir = std::env::var_os("UNLATCHD_STRESS_FAIL_DIR").map(std::path::PathBuf::from);
    let mut failed = Vec::new();
    for seed in seeds() {
        let res = std::panic::catch_unwind(|| run_seed(seed));
        if let Err(e) = res {
            let msg = e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            if let Some(d) = &fail_dir {
                let _ = std::fs::create_dir_all(d);
                let _ = std::fs::write(d.join(format!("seed-{seed}.log")), &msg);
            }
            failed.push(seed);
        }
    }
    assert!(failed.is_empty(), "failed seeds: {failed:?}");
}

fn run_seed(seed: u64) {
    let (root, state) = (tmp(), tmp());
    let r = root.path();
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    for d in ["a", "b", "c"] {
        std::fs::create_dir(r.join(d)).unwrap();
    }
    std::fs::write(r.join("a/f0"), b"x").unwrap();
    let mut c = Client::spawn(r, state.path(), Opts::default());
    c.wait_snapshot();
    let mut log = vec![format!("seed {seed}")];
    for step in 0..60 {
        if !files(r).iter().any(|p| p.ends_with('/')) {
            std::fs::create_dir(r.join(format!("fresh{step}"))).unwrap();
        }
        let fs = files(r);
        let fl: Vec<&String> = fs.iter().filter(|p| !p.ends_with('/')).collect();
        let dl: Vec<String> = fs
            .iter()
            .filter_map(|p| p.strip_suffix('/').map(|s| s.to_string()))
            .collect();
        let pickf = |rng: &mut rand::rngs::StdRng| {
            fl.get(rng.gen_range(0..fl.len().max(1)))
                .map(|s| s.to_string())
        };
        let pickd = |rng: &mut rand::rngs::StdRng| dl[rng.gen_range(0..dl.len())].clone();
        let op = rng.gen_range(0..15);
        let name = format!("n{}", rng.gen_range(0..6));
        let res: String = match op {
            0 => {
                let d = pickd(&mut rng);
                let p = format!("{d}/{name}");
                let n = rng.gen_range(0..50);
                let _ = std::fs::write(r.join(&p), vec![b'w'; n]);
                format!("write {p} {n}")
            }
            1 => {
                if let Some(f) = pickf(&mut rng) {
                    let d = pickd(&mut rng);
                    let p = format!("{d}/{name}");
                    let e = std::fs::hard_link(r.join(&f), r.join(&p));
                    format!("link {f} {p} {e:?}")
                } else {
                    "-".into()
                }
            }
            2 => {
                if let Some(f) = pickf(&mut rng) {
                    let n = rng.gen_range(0..50);
                    std::fs::write(r.join(&f), vec![b'o'; n]).unwrap();
                    format!("overwrite {f} {n}")
                } else {
                    "-".into()
                }
            }
            3 => {
                if let Some(f) = pickf(&mut rng) {
                    let n = rng.gen_range(0..50);
                    let t = r.join(format!("{f}.tmp~"));
                    std::fs::write(&t, vec![b'r'; n]).unwrap();
                    std::fs::rename(&t, r.join(&f)).unwrap();
                    format!("replace {f} {n}")
                } else {
                    "-".into()
                }
            }
            4 => {
                if let Some(f) = pickf(&mut rng) {
                    std::fs::remove_file(r.join(&f)).unwrap();
                    format!("rm {f}")
                } else {
                    "-".into()
                }
            }
            5 => {
                if let Some(f) = pickf(&mut rng) {
                    let d = pickd(&mut rng);
                    let p = format!("{d}/{name}");
                    let e = std::fs::rename(r.join(&f), r.join(&p));
                    format!("mv {f} {p} {e:?}")
                } else {
                    "-".into()
                }
            }
            6 => {
                let d = pickd(&mut rng);
                if d.is_empty() {
                    "-".into()
                } else {
                    let p = format!("{d}{}", rng.gen_range(0..3));
                    let e = std::fs::rename(r.join(&d), r.join(&p));
                    format!("mvdir {d} {p} {e:?}")
                }
            }
            7 => {
                if let Some(f) = pickf(&mut rng) {
                    use std::io::Write;
                    let mut h = std::fs::OpenOptions::new()
                        .append(true)
                        .open(r.join(&f))
                        .unwrap();
                    h.write_all(b"aa").unwrap();
                    format!("append {f}")
                } else {
                    "-".into()
                }
            }
            9 => {
                let d = pickd(&mut rng);
                let p = if d.is_empty() {
                    name.clone()
                } else {
                    format!("{d}/{name}")
                };
                let e = std::fs::create_dir(r.join(&p));
                format!("mkdir {p} {e:?}")
            }
            10 => {
                let d = pickd(&mut rng);
                if d.is_empty() || dl.len() < 3 {
                    "-".into()
                } else {
                    std::fs::remove_dir_all(r.join(&d)).unwrap();
                    format!("rmrf {d}")
                }
            }
            11 => {
                let d = pickd(&mut rng);
                let p = if d.is_empty() {
                    format!("{name}.l")
                } else {
                    format!("{d}/{name}.l")
                };
                let e = std::os::unix::fs::symlink("x/y", r.join(&p));
                format!("symlink {p} {e:?}")
            }
            12 => {
                if let Some(f) = pickf(&mut rng) {
                    let len = std::fs::metadata(r.join(&f)).unwrap().len() as usize;
                    for i in 0..5u8 {
                        std::fs::write(r.join(&f), vec![b'a' + i; len]).unwrap();
                    }
                    format!("churn {f}")
                } else {
                    "-".into()
                }
            }
            13 => {
                if fl.len() >= 2 {
                    let a = pickf(&mut rng).unwrap();
                    let b = pickf(&mut rng).unwrap();
                    let e = std::fs::rename(r.join(&a), r.join(&b));
                    format!("mv-over {a} {b} {e:?}")
                } else {
                    "-".into()
                }
            }
            14 => {
                let d = pickd(&mut rng);
                if d.is_empty() {
                    "-".into()
                } else {
                    let o = format!("{d}.old");
                    if r.join(&o).exists() {
                        "-".into()
                    } else {
                        std::fs::rename(r.join(&d), r.join(&o)).unwrap();
                        std::os::unix::fs::symlink("/nonexistent", r.join(&d)).unwrap();
                        format!("swap {d}")
                    }
                }
            }
            _ => {
                let kill = rng.gen_bool(0.5);
                if kill {
                    c.kill();
                } else {
                    c.close();
                }
                c = Client::spawn(r, state.path(), Opts::default());
                c.wait_snapshot();
                format!("restart kill={kill}")
            }
        };
        log.push(format!("{step}: {res}"));
        if rng.gen_bool(0.5) {
            check(&mut c, r, &log);
        }
    }
    check(&mut c, r, &log);
}
