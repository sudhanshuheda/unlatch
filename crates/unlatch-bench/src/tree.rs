//! Deterministic synthetic "VM" trees.
//!
//! Layout of a generated tree (`<dir>/vm` is the root Unlatch / sshfs export):
//!
//! ```text
//! vm/repo/…              ~100k entries: nested source-like dirs, lognormal file sizes
//!                        (median 2 KiB) plus a few multi-MB blobs
//! vm/repo/node_modules/  ~30k entries: package dirs (lazy by name, DEFAULT_LAZY_NAMES)
//! vm/flat/               exactly 1000 entries (T3 `ls -la`, T6/T7 small files)
//! vm/big/big.bin         256 MiB incompressible (T8)
//! vm/scratch/            empty; scenarios create their own subdirs here
//! ```
//!
//! Everything is derived from `seed` with ChaCha8 (stable across `rand` versions), so the same
//! spec always produces byte-identical trees. Generation is parallel and cached: a finished
//! tree has `manifest.json` next to `vm/`, and [`ensure`] reuses it when the spec matches.

use anyhow::{bail, Context, Result};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rand_distr::{Distribution, LogNormal};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bump when the generator's output changes for an unchanged spec.
const GENERATOR_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeSpec {
    pub seed: u64,
    /// Entries (files + dirs) in `repo/`, excluding `repo/node_modules/`.
    pub repo_entries: u64,
    /// Entries inside `repo/node_modules/`.
    pub lazy_entries: u64,
    /// Entries in `flat/` (T3).
    pub flat_entries: u64,
    /// Size of `big/big.bin`.
    pub big_bytes: u64,
    /// Number of multi-MB blobs sprinkled into `repo/`.
    pub mb_files: u32,
}

impl TreeSpec {
    /// The DESIGN §6 reference tree.
    pub fn full() -> TreeSpec {
        TreeSpec {
            seed: 0x4a7c_4b3e_2026_0930,
            repo_entries: 100_000,
            lazy_entries: 30_000,
            flat_entries: 1000,
            big_bytes: 256 << 20,
            mb_files: 16,
        }
    }

    /// A small tree for unit/integration tests.
    pub fn tiny() -> TreeSpec {
        TreeSpec {
            seed: 7,
            repo_entries: 400,
            lazy_entries: 200,
            flat_entries: 50,
            big_bytes: 1 << 20,
            mb_files: 1,
        }
    }

    fn cache_key(&self) -> String {
        format!(
            "v{GENERATOR_VERSION}-s{:x}-r{}-l{}-f{}-b{}-m{}",
            self.seed,
            self.repo_entries,
            self.lazy_entries,
            self.flat_entries,
            self.big_bytes,
            self.mb_files
        )
    }
}

/// What was generated (paths relative to the tree's `vm/` root).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TreeManifest {
    pub spec: TreeSpec,
    pub generator_version: u32,
    /// Absolute path of the VM root.
    pub root: PathBuf,
    pub repo_dirs: u64,
    pub repo_files: u64,
    pub lazy_dirs: u64,
    pub lazy_files: u64,
    pub total_bytes: u64,
    /// Entries a non-lazy scan of the root sees (everything except node_modules' contents),
    /// including `repo/node_modules` itself and the top-level dirs, excluding the root.
    pub eager_entries: u64,
    /// Relative paths of `flat/` files ≤ 12 KiB (T6/T7 small).
    pub flat_small: Vec<String>,
    /// Relative paths of `flat/` files of exactly 256 KiB (T7 upper bound).
    pub flat_256k: Vec<String>,
    /// `big/big.bin`.
    pub big: String,
    /// A handful of package dirs under `repo/node_modules` (lazy-listing probes, T13).
    pub lazy_probe_dirs: Vec<String>,
}

impl TreeManifest {
    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

#[derive(Clone, Debug)]
enum Node {
    Dir(String),
    File {
        path: String,
        size: u64,
        seed: u64,
        text: bool,
    },
}

const EXTS: &[(&str, bool)] = &[
    ("rs", true),
    ("ts", true),
    ("tsx", true),
    ("py", true),
    ("go", true),
    ("md", true),
    ("json", true),
    ("yaml", true),
    ("txt", true),
    ("png", false),
    ("wasm", false),
];

const WORDS: &[&str] = &[
    "fn", "let", "mut", "self", "impl", "struct", "return", "match", "Some", "None", "Ok", "Err",
    "const", "async", "await", "import", "from", "export", "default", "class", "def", "if", "else",
    "for", "while", "in", "value", "result", "config", "request", "response", "item", "entry",
    "parent", "name", "size", "version", "cache", "engine", "daemon", "=", "{", "}", "(", ")", ";",
    "->", "=>", "::", ".", ",", "0", "1", "42", "true", "false", "null",
];

/// A pseudo-text corpus (compresses roughly like source code) and a random corpus.
struct Corpus {
    text: Vec<u8>,
    random: Vec<u8>,
}

impl Corpus {
    fn new(seed: u64) -> Corpus {
        let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0xc0de);
        let mut text = Vec::with_capacity(1 << 20);
        let mut col = 0;
        while text.len() < (1 << 20) {
            let w = WORDS[rng.gen_range(0..WORDS.len())];
            text.extend_from_slice(w.as_bytes());
            col += w.len() + 1;
            if col > 60 + rng.gen_range(0..40) {
                text.push(b'\n');
                col = 0;
                let indent = rng.gen_range(0..4) * 4;
                text.extend(std::iter::repeat_n(b' ', indent));
            } else {
                text.push(b' ');
            }
        }
        let mut random = vec![0u8; 1 << 20];
        rng.fill(&mut random[..]);
        Corpus { text, random }
    }

    fn write_content<W: Write>(&self, w: &mut W, size: u64, seed: u64, text: bool) -> Result<()> {
        let src = if text { &self.text } else { &self.random };
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let mut left = size;
        let mut off = rng.gen_range(0..src.len());
        while left > 0 {
            let k = (left as usize).min(src.len() - off);
            w.write_all(&src[off..off + k])?;
            left -= k as u64;
            off = (off + k) % src.len();
        }
        Ok(())
    }
}

fn plan(spec: &TreeSpec) -> Result<(Vec<Node>, PlanCounts)> {
    let mut rng = ChaCha8Rng::seed_from_u64(spec.seed);
    let sizes = LogNormal::new((2048f64).ln(), 1.2).context("lognormal")?;
    let mut nodes = Vec::new();
    let mut c = PlanCounts::default();
    for top in ["repo", "flat", "big", "scratch"] {
        nodes.push(Node::Dir(top.into()));
    }
    c.top_dirs = 4;

    // repo/: breadth-first growth until the entry budget is used.
    let mut queue: std::collections::VecDeque<(String, u32)> = std::collections::VecDeque::new();
    queue.push_back(("repo".into(), 0));
    let mut made: u64 = 0;
    let mut file_no: u64 = 0;
    while made < spec.repo_entries {
        let Some((dir, depth)) = queue.pop_front() else {
            break;
        };
        let files = rng.gen_range(4..28u64);
        let subdirs: u64 = match depth {
            0 => 12,
            1..=2 => rng.gen_range(3..9),
            3..=5 => rng.gen_range(0..5),
            _ => rng.gen_range(0..2),
        };
        // Keep growing: never let the queue run dry before the budget is reached.
        let subdirs = if queue.is_empty() {
            subdirs.max(1)
        } else {
            subdirs
        };
        for _ in 0..files {
            if made >= spec.repo_entries {
                break;
            }
            let (ext, text) = EXTS[rng.gen_range(0..EXTS.len())];
            let size = (sizes.sample(&mut rng) as u64).min(4 << 20);
            nodes.push(Node::File {
                path: format!("{dir}/f{file_no:06}.{ext}"),
                size,
                seed: rng.gen(),
                text,
            });
            file_no += 1;
            made += 1;
            c.repo_files += 1;
        }
        for i in 0..subdirs {
            if made >= spec.repo_entries {
                break;
            }
            let d = format!("{dir}/d{depth}_{i}");
            nodes.push(Node::Dir(d.clone()));
            queue.push_back((d, depth + 1));
            made += 1;
            c.repo_dirs += 1;
        }
    }
    if made < spec.repo_entries {
        bail!("tree plan ran out of directories at {made} entries");
    }
    // Multi-MB blobs replace the sizes of some existing files (entry count unchanged).
    let file_idx: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n, Node::File { path, .. } if path.starts_with("repo/")))
        .map(|(i, _)| i)
        .collect();
    for _ in 0..spec.mb_files {
        if file_idx.is_empty() {
            break;
        }
        let i = file_idx[rng.gen_range(0..file_idx.len())];
        if let Node::File { size, text, .. } = &mut nodes[i] {
            *size = rng.gen_range(1u64 << 20..8 << 20);
            *text = false;
        }
    }

    // repo/node_modules/: packages of ~16 entries.
    nodes.push(Node::Dir("repo/node_modules".into()));
    c.repo_dirs += 1; // node_modules itself is visible to an eager scan
    let mut lazy: u64 = 0;
    let mut pkg = 0u64;
    while lazy < spec.lazy_entries {
        let scope = if pkg.checked_rem(9) == Some(0) {
            format!("@scope{}/", pkg % 7)
        } else {
            String::new()
        };
        if !scope.is_empty() {
            let sd = format!("repo/node_modules/{}", scope.trim_end_matches('/'));
            if !c.scopes.contains(&sd) {
                c.scopes.push(sd.clone());
                nodes.push(Node::Dir(sd));
                lazy += 1;
                c.lazy_dirs += 1;
            }
        }
        let p = format!("repo/node_modules/{scope}pkg-{pkg:05}");
        if c.lazy_probes.len() < 64 && pkg % 17 == 3 {
            c.lazy_probes.push(p.clone());
        }
        for d in [p.clone(), format!("{p}/lib"), format!("{p}/dist")] {
            nodes.push(Node::Dir(d));
            lazy += 1;
            c.lazy_dirs += 1;
        }
        let mut files: Vec<String> = vec![
            "package.json".into(),
            "README.md".into(),
            "LICENSE".into(),
            "index.js".into(),
        ];
        files.extend((0..6).map(|i| format!("lib/m{i}.js")));
        files.extend((0..3).map(|i| format!("dist/b{i}.min.js")));
        for f in files {
            let size = (sizes.sample(&mut rng) as u64).min(256 << 10);
            nodes.push(Node::File {
                path: format!("{p}/{f}"),
                size,
                seed: rng.gen(),
                text: true,
            });
            lazy += 1;
            c.lazy_files += 1;
        }
        pkg += 1;
    }

    // flat/: exactly flat_entries; the first min(10, n/10) are 256 KiB, the rest ≤ 12 KiB.
    let big_ones = (spec.flat_entries / 10).min(10);
    for i in 0..spec.flat_entries {
        let (path, size) = if i < big_ones {
            (format!("flat/m{i:04}.bin"), 256 << 10)
        } else {
            (format!("flat/f{i:04}.txt"), rng.gen_range(512u64..12 << 10))
        };
        if size == 256 << 10 {
            c.flat_256k.push(path.clone());
        } else {
            c.flat_small.push(path.clone());
        }
        nodes.push(Node::File {
            path,
            size,
            seed: rng.gen(),
            text: size != 256 << 10,
        });
    }
    c.flat_files = spec.flat_entries;

    nodes.push(Node::File {
        path: "big/big.bin".into(),
        size: spec.big_bytes,
        seed: rng.gen(),
        text: false,
    });
    Ok((nodes, c))
}

#[derive(Default)]
struct PlanCounts {
    top_dirs: u64,
    repo_dirs: u64,
    repo_files: u64,
    lazy_dirs: u64,
    lazy_files: u64,
    flat_files: u64,
    scopes: Vec<String>,
    lazy_probes: Vec<String>,
    flat_small: Vec<String>,
    flat_256k: Vec<String>,
}

/// Generate `spec` under `dir` (creating `dir/vm` and `dir/manifest.json`). `dir/vm` must not
/// exist yet.
pub fn generate(spec: &TreeSpec, dir: &Path) -> Result<TreeManifest> {
    let root = dir.join("vm");
    if root.exists() {
        bail!("{} already exists", root.display());
    }
    let (nodes, c) = plan(spec)?;
    let corpus = Corpus::new(spec.seed);
    std::fs::create_dir_all(&root)?;
    // Directories first (sequential, cheap), then files in parallel.
    for n in &nodes {
        if let Node::Dir(d) = n {
            std::fs::create_dir_all(root.join(d)).with_context(|| format!("mkdir {d}"))?;
        }
    }
    let files: Vec<&Node> = nodes
        .iter()
        .filter(|n| matches!(n, Node::File { .. }))
        .collect();
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(32);
    let chunk = files.len().div_ceil(threads).max(1);
    let total_bytes = std::thread::scope(|s| -> Result<u64> {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| {
                let root = &root;
                let corpus = &corpus;
                s.spawn(move || -> Result<u64> {
                    let mut bytes = 0;
                    for n in part {
                        if let Node::File {
                            path,
                            size,
                            seed,
                            text,
                        } = n
                        {
                            let p = root.join(path);
                            let f = std::fs::File::create(&p)
                                .with_context(|| format!("create {}", p.display()))?;
                            let mut w = std::io::BufWriter::with_capacity(256 << 10, f);
                            corpus.write_content(&mut w, *size, *seed, *text)?;
                            w.flush()?;
                            bytes += size;
                        }
                    }
                    Ok(bytes)
                })
            })
            .collect();
        let mut total = 0;
        for h in handles {
            total += h
                .join()
                .map_err(|_| anyhow::anyhow!("generator thread panicked"))??;
        }
        Ok(total)
    })?;

    let eager_entries = c.top_dirs + c.repo_dirs + c.repo_files + c.flat_files + 1 /* big.bin */;
    let m = TreeManifest {
        spec: spec.clone(),
        generator_version: GENERATOR_VERSION,
        root: root.canonicalize()?,
        repo_dirs: c.repo_dirs,
        repo_files: c.repo_files,
        lazy_dirs: c.lazy_dirs,
        lazy_files: c.lazy_files,
        total_bytes,
        eager_entries,
        flat_small: c.flat_small,
        flat_256k: c.flat_256k,
        big: "big/big.bin".into(),
        lazy_probe_dirs: c.lazy_probes,
    };
    let tmp = dir.join("manifest.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&m)?)?;
    std::fs::rename(&tmp, dir.join("manifest.json"))?;
    Ok(m)
}

/// Reuse `<cache>/tree-<key>/` when complete, else (re)generate it.
pub fn ensure(spec: &TreeSpec, cache: &Path) -> Result<TreeManifest> {
    let dir = cache.join(format!("tree-{}", spec.cache_key()));
    let mpath = dir.join("manifest.json");
    if let Ok(bytes) = std::fs::read(&mpath) {
        if let Ok(m) = serde_json::from_slice::<TreeManifest>(&bytes) {
            if m.spec == *spec && m.generator_version == GENERATOR_VERSION && m.root.is_dir() {
                clean_scratch(&m)?;
                return Ok(m);
            }
        }
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("remove stale {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir)?;
    generate(spec, &dir)
}

/// Empty `vm/scratch/` (scenario leftovers from an interrupted run).
pub fn clean_scratch(m: &TreeManifest) -> Result<()> {
    let s = m.root.join("scratch");
    if s.exists() {
        for e in std::fs::read_dir(&s)? {
            let p = e?.path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p)?;
            } else {
                std::fs::remove_file(&p)?;
            }
        }
    } else {
        std::fs::create_dir_all(&s)?;
    }
    Ok(())
}

/// A wide lazy fixture for T15: `<dir>/vm/node_modules/pkg-NNNNN/{a..}` with `files` files in
/// total, `per_pkg` files per package. Cached like [`ensure`].
pub fn ensure_wide_lazy(cache: &Path, files: u64, per_pkg: u64) -> Result<(PathBuf, u64)> {
    let dir = cache.join(format!("wide-v{GENERATOR_VERSION}-{files}-{per_pkg}"));
    let root = dir.join("vm");
    let done = dir.join("done");
    let pkgs = files.div_ceil(per_pkg);
    if done.is_file() {
        return Ok((root, pkgs));
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    let nm = root.join("node_modules");
    std::fs::create_dir_all(&nm)?;
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(32) as u64;
    std::thread::scope(|s| -> Result<()> {
        let hs: Vec<_> = (0..threads)
            .map(|t| {
                let nm = &nm;
                s.spawn(move || -> Result<()> {
                    let mut p = t;
                    while p < pkgs {
                        let pd = nm.join(format!("pkg-{p:05}"));
                        std::fs::create_dir(&pd)?;
                        for f in 0..per_pkg {
                            std::fs::write(pd.join(format!("f{f:04}.js")), b"module.exports=1;\n")?;
                        }
                        p += threads;
                    }
                    Ok(())
                })
            })
            .collect();
        for h in hs {
            h.join()
                .map_err(|_| anyhow::anyhow!("wide fixture thread panicked"))??;
        }
        Ok(())
    })?;
    std::fs::write(&done, b"")?;
    Ok((root.canonicalize()?, pkgs))
}

/// Count entries below `root` (not including `root`), not descending into `skip` names.
pub fn count_entries(root: &Path, skip: &[&str]) -> Result<u64> {
    let mut n = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            n += 1;
            let ft = e.file_type()?;
            if ft.is_dir() && !skip.iter().any(|s| e.file_name() == *s) {
                stack.push(e.path());
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_deterministic_and_sized() {
        let spec = TreeSpec::tiny();
        let (a, ca) = plan(&spec).unwrap();
        let (b, _) = plan(&spec).unwrap();
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        assert_eq!(ca.repo_dirs + ca.repo_files, spec.repo_entries + 1);
        assert!(ca.lazy_dirs + ca.lazy_files >= spec.lazy_entries);
        assert_eq!(
            ca.flat_small.len() + ca.flat_256k.len(),
            spec.flat_entries as usize
        );
        let other = TreeSpec { seed: 8, ..spec };
        let (c, _) = plan(&other).unwrap();
        assert_ne!(format!("{a:?}"), format!("{c:?}"));
    }

    #[test]
    fn full_plan_median_size() {
        let (nodes, c) = plan(&TreeSpec::full()).unwrap();
        let mut sizes: Vec<u64> = nodes
            .iter()
            .filter_map(|n| match n {
                Node::File { path, size, .. }
                    if path.starts_with("repo/") && !path.contains("node_modules") =>
                {
                    Some(*size)
                }
                _ => None,
            })
            .collect();
        sizes.sort_unstable();
        let med = sizes[sizes.len() / 2];
        assert!((1500..2800).contains(&med), "median {med}");
        assert!(sizes.iter().filter(|s| **s >= 1 << 20).count() >= 10);
        assert_eq!(c.repo_dirs + c.repo_files, 100_001);
        assert!(c.lazy_dirs + c.lazy_files >= 30_000 && c.lazy_dirs + c.lazy_files < 30_100);
    }
}
