//! Synthetic tree generation on disk.

use std::collections::BTreeMap;
use std::path::Path;
use unlatch_bench::tree::{self, TreeSpec};

fn snapshot(root: &Path) -> BTreeMap<String, (bool, u64, u64)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let md = e.metadata().unwrap();
            let rel = e.path().strip_prefix(root).unwrap().display().to_string();
            let hash = if md.is_file() {
                std::fs::read(e.path())
                    .unwrap()
                    .iter()
                    .fold(0xcbf29ce484222325u64, |h, b| {
                        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
                    })
            } else {
                stack.push(e.path());
                0
            };
            out.insert(rel, (md.is_dir(), md.len(), hash));
        }
    }
    out
}

#[test]
fn tiny_tree_matches_manifest_and_is_deterministic() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let spec = TreeSpec::tiny();
    let ma = tree::ensure(&spec, a.path()).unwrap();
    let mb = tree::ensure(&spec, b.path()).unwrap();
    let sa = snapshot(&ma.root);
    assert_eq!(
        sa,
        snapshot(&mb.root),
        "same seed must give identical trees"
    );

    let lazy = tree::count_entries(&ma.root.join("repo/node_modules"), &[]).unwrap();
    assert_eq!(lazy, ma.lazy_dirs + ma.lazy_files);
    let eager = tree::count_entries(&ma.root, &["node_modules"]).unwrap();
    assert_eq!(eager, ma.eager_entries);
    assert_eq!(
        tree::count_entries(&ma.root.join("flat"), &[]).unwrap(),
        spec.flat_entries
    );
    assert_eq!(
        std::fs::metadata(ma.path(&ma.big)).unwrap().len(),
        spec.big_bytes
    );
    for p in &ma.flat_small {
        assert!(std::fs::metadata(ma.path(p)).unwrap().len() <= 12 << 10);
    }
    for p in &ma.flat_256k {
        assert_eq!(std::fs::metadata(ma.path(p)).unwrap().len(), 256 << 10);
    }
    for p in &ma.lazy_probe_dirs {
        assert!(ma.path(p).is_dir(), "{p}");
    }
}

#[test]
fn ensure_reuses_and_cleans_scratch() {
    let c = tempfile::tempdir().unwrap();
    let spec = TreeSpec::tiny();
    let m1 = tree::ensure(&spec, c.path()).unwrap();
    std::fs::write(m1.root.join("scratch/leftover.txt"), b"x").unwrap();
    std::fs::create_dir(m1.root.join("scratch/dir")).unwrap();
    let marker = m1.root.join("repo/marker-not-regenerated");
    std::fs::write(&marker, b"").unwrap();
    let m2 = tree::ensure(&spec, c.path()).unwrap();
    assert_eq!(m1, m2);
    assert!(marker.exists(), "cached tree must be reused");
    assert_eq!(
        std::fs::read_dir(m2.root.join("scratch")).unwrap().count(),
        0
    );
    // A different seed is a different cache entry.
    let m3 = tree::ensure(&TreeSpec { seed: 99, ..spec }, c.path()).unwrap();
    assert_ne!(m3.root, m1.root);
}

#[test]
fn wide_lazy_fixture() {
    let c = tempfile::tempdir().unwrap();
    let (root, pkgs) = tree::ensure_wide_lazy(c.path(), 1000, 100).unwrap();
    assert_eq!(pkgs, 10);
    assert_eq!(tree::count_entries(&root, &[]).unwrap(), 1 + 10 + 1000);
    let (root2, _) = tree::ensure_wide_lazy(c.path(), 1000, 100).unwrap();
    assert_eq!(root, root2);
}
