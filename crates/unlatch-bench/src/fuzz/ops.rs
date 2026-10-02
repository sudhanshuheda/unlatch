//! The fuzzer's operations and their seeded generator.
//!
//! Targets of operations on *existing* items are picks (`u32` indexes into the sorted list of
//! current items at execution time), not paths: removing earlier ops during shrinking keeps every
//! remaining op meaningful. New names come from a small pool that deliberately contains case and
//! Unicode-normalization twins (`A.txt`/`a.txt`, `café` NFC/NFD) and a lazy directory name.

use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::fmt;

/// Names new items are drawn from.
pub const NAMES: &[&str] = &[
    "a.txt",
    "A.txt",
    "b.txt",
    "main.rs",
    "README",
    "readme",
    "caf\u{e9}.md",
    "cafe\u{301}.md",
    "notes",
    "x",
    "data.json",
    ".env",
];

/// Directory names (includes a lazy one and a case twin).
pub const DIR_NAMES: &[&str] = &["src", "Src", "docs", "lib", "node_modules", "tmp", "deep"];

/// Relative symlink targets (some in-root at depth ≥ 1, some escaping).
pub const LINK_TARGETS: &[&str] = &[
    "a.txt",
    "../a.txt",
    "src/main.rs",
    "../../x",
    "..",
    "docs/",
    "./b.txt",
];

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Op {
    // ---- agent on the VM ----------------------------------------------------------------
    /// Create or truncate `dir/name` with unique bytes.
    Write {
        dir: u32,
        name: String,
        n: u64,
    },
    Overwrite {
        file: u32,
        n: u64,
    },
    Append {
        file: u32,
        n: u64,
    },
    /// Editor/agent save: write `.name.tmp~` then rename over the target.
    AtomicReplace {
        file: u32,
        n: u64,
    },
    /// `mv` of any entry (file or dir) to `dir/name`; replaces an existing file there.
    Rename {
        src: u32,
        dir: u32,
        name: String,
    },
    Mkdir {
        dir: u32,
        name: String,
    },
    Rm {
        file: u32,
    },
    RmRf {
        dir: u32,
    },
    SymlinkIn {
        dir: u32,
        name: String,
        target: String,
    },
    /// Absolute symlink to the sentinel directory outside the root.
    SymlinkOut {
        dir: u32,
        name: String,
    },
    Hardlink {
        file: u32,
        dir: u32,
        name: String,
    },
    Chmod {
        file: u32,
        mode: u32,
    },
    /// Rapid same-size rewrites in place (coarse-mtime territory, T17).
    Churn {
        file: u32,
        n: u64,
        count: u32,
    },
    /// `git checkout`-like mass change in one directory.
    MassChange {
        dir: u32,
        n: u64,
        count: u32,
    },
    /// `mv a b; touch a` in one burst.
    MvThenTouch {
        file: u32,
        name: String,
        n: u64,
    },
    /// `rm f; mkdir f`.
    RmThenMkdir {
        file: u32,
    },
    /// Replace an intermediate directory by a symlink pointing outside the root.
    SwapDirForSymlink {
        dir: u32,
    },
    // ---- the Mac, through fpsim -----------------------------------------------------------
    MacBrowse {
        dir: u32,
    },
    MacOpen {
        file: u32,
    },
    MacEdit {
        file: u32,
        n: u64,
    },
    MacSave {
        file: u32,
        n: u64,
    },
    MacCreate {
        dir: u32,
        name: String,
        n: u64,
    },
    MacMkdir {
        dir: u32,
        name: String,
    },
    MacRename {
        item: u32,
        name: String,
    },
    MacMove {
        item: u32,
        dir: u32,
    },
    MacDelete {
        item: u32,
    },
    MacDragIn {
        dir: u32,
        n: u64,
        count: u32,
    },
    MacChmod {
        file: u32,
        exec: bool,
    },
    MacTag {
        item: u32,
    },
    MacEvict {
        file: u32,
    },
    // ---- faults and time --------------------------------------------------------------------
    DropConnection,
    KillUnlatchd,
    /// Cut the link to the VM until `GoOnline` (or the next quiesce).
    GoOffline,
    GoOnline,
    /// Restart the engine process (replica and journal persist).
    RestartEngine,
    /// `die_before_ipc_reply:<kind>` for the next op of that kind (0 create, 1 modify,
    /// 2 delete, 3 fetch).
    LoseReply {
        kind: u8,
    },
    AdvanceTime {
        secs: u32,
    },
    /// Barrier: settle, let fpsim catch up, check every invariant.
    Quiesce,
}

impl Op {
    pub fn is_mac(&self) -> bool {
        matches!(
            self,
            Op::MacBrowse { .. }
                | Op::MacOpen { .. }
                | Op::MacEdit { .. }
                | Op::MacSave { .. }
                | Op::MacCreate { .. }
                | Op::MacMkdir { .. }
                | Op::MacRename { .. }
                | Op::MacMove { .. }
                | Op::MacDelete { .. }
                | Op::MacDragIn { .. }
                | Op::MacChmod { .. }
                | Op::MacTag { .. }
                | Op::MacEvict { .. }
        )
    }

    pub fn is_fault(&self) -> bool {
        matches!(
            self,
            Op::DropConnection
                | Op::KillUnlatchd
                | Op::GoOffline
                | Op::GoOnline
                | Op::RestartEngine
                | Op::LoseReply { .. }
        )
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug is precise and re-typeable into a test; names are escaped.
        write!(f, "{self:?}")
    }
}

/// What the generator may emit.
#[derive(Clone, Copy, Debug)]
pub struct Profile {
    pub faults: bool,
    /// Kill unlatchd (real worlds only).
    pub kill_unlatchd: bool,
    /// Ops that only make sense on a real filesystem (hardlinks, out-of-root symlinks, swaps).
    pub real_fs: bool,
}

/// Unique, deterministic content for op nonce `n` (sizes vary from 0 to a few KiB).
pub fn content(n: u64, tag: &str) -> Vec<u8> {
    let mut v = format!("{tag}#{n}\n").into_bytes();
    let filler = (n.wrapping_mul(2_654_435_761) % 4096) as usize;
    v.extend(std::iter::repeat_n(b'a' + (n % 26) as u8, filler));
    v
}

/// `count` ops for `seed`. Every run ends with an implicit quiesce (the runner adds it).
pub fn generate(seed: u64, count: usize, profile: Profile) -> Vec<Op> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut ops = Vec::with_capacity(count);
    let mut nonce = seed.wrapping_mul(1_000_003) & 0xffff_ffff;
    let mut n = || {
        nonce += 1;
        nonce
    };
    // A starting tree so the Mac has something to act on.
    for d in ["src", "docs"] {
        ops.push(Op::Mkdir {
            dir: 0,
            name: d.into(),
        });
    }
    for _ in 0..6 {
        ops.push(Op::Write {
            dir: rng.gen(),
            name: pick(&mut rng, NAMES),
            n: n(),
        });
    }
    ops.push(Op::Quiesce);
    for _ in ops.len()..count {
        let roll = rng.gen_range(0..100u32);
        let op = match roll {
            0..=7 => Op::Write {
                dir: rng.gen(),
                name: pick(&mut rng, NAMES),
                n: n(),
            },
            8..=11 => Op::Overwrite {
                file: rng.gen(),
                n: n(),
            },
            12..=13 => Op::Append {
                file: rng.gen(),
                n: n(),
            },
            14..=16 => Op::AtomicReplace {
                file: rng.gen(),
                n: n(),
            },
            17..=19 => {
                let name = if rng.gen_bool(0.3) {
                    pick(&mut rng, DIR_NAMES)
                } else {
                    pick(&mut rng, NAMES)
                };
                Op::Rename {
                    src: rng.gen(),
                    dir: rng.gen(),
                    name,
                }
            }
            20..=22 => Op::Mkdir {
                dir: rng.gen(),
                name: pick(&mut rng, DIR_NAMES),
            },
            23..=24 => Op::Rm { file: rng.gen() },
            25 => Op::RmRf { dir: rng.gen() },
            26 => Op::SymlinkIn {
                dir: rng.gen(),
                name: pick(&mut rng, NAMES),
                target: pick(&mut rng, LINK_TARGETS),
            },
            27 if profile.real_fs => Op::SymlinkOut {
                dir: rng.gen(),
                name: pick(&mut rng, NAMES),
            },
            28 if profile.real_fs => Op::Hardlink {
                file: rng.gen(),
                dir: rng.gen(),
                name: pick(&mut rng, NAMES),
            },
            29 => Op::Chmod {
                file: rng.gen(),
                mode: *[0o644u32, 0o755, 0o600, 0o444]
                    .choose(&mut rng)
                    .unwrap_or(&0o644),
            },
            30 => Op::Churn {
                file: rng.gen(),
                n: n(),
                count: rng.gen_range(2..20),
            },
            31 => Op::MassChange {
                dir: rng.gen(),
                n: n(),
                count: rng.gen_range(5..40),
            },
            32 => Op::MvThenTouch {
                file: rng.gen(),
                name: pick(&mut rng, NAMES),
                n: n(),
            },
            33 => Op::RmThenMkdir { file: rng.gen() },
            34 if profile.real_fs => Op::SwapDirForSymlink { dir: rng.gen() },
            35..=40 => Op::MacBrowse { dir: rng.gen() },
            41..=45 => Op::MacOpen { file: rng.gen() },
            46..=50 => Op::MacEdit {
                file: rng.gen(),
                n: n(),
            },
            51..=54 => Op::MacSave {
                file: rng.gen(),
                n: n(),
            },
            55..=59 => Op::MacCreate {
                dir: rng.gen(),
                name: pick(&mut rng, NAMES),
                n: n(),
            },
            60..=61 => Op::MacMkdir {
                dir: rng.gen(),
                name: pick(&mut rng, DIR_NAMES),
            },
            62..=64 => Op::MacRename {
                item: rng.gen(),
                name: pick(&mut rng, NAMES),
            },
            65..=66 => Op::MacMove {
                item: rng.gen(),
                dir: rng.gen(),
            },
            67..=69 => Op::MacDelete { item: rng.gen() },
            70 => Op::MacDragIn {
                dir: rng.gen(),
                n: n(),
                count: rng.gen_range(1..6),
            },
            71 => Op::MacChmod {
                file: rng.gen(),
                exec: rng.gen(),
            },
            72 => Op::MacTag { item: rng.gen() },
            73 => Op::MacEvict { file: rng.gen() },
            74 if profile.faults => Op::DropConnection,
            75 if profile.faults => {
                if rng.gen_bool(0.5) {
                    Op::GoOffline
                } else {
                    Op::GoOnline
                }
            }
            76 if profile.faults && profile.kill_unlatchd => Op::KillUnlatchd,
            76 if profile.faults => Op::RestartEngine,
            88..=89 if profile.faults => Op::LoseReply {
                kind: rng.gen_range(0..4),
            },
            77..=79 => Op::AdvanceTime {
                secs: *[1u32, 6, 30, 120].choose(&mut rng).unwrap_or(&1),
            },
            80..=87 => Op::Quiesce,
            _ => Op::Overwrite {
                file: rng.gen(),
                n: n(),
            },
        };
        ops.push(op);
    }
    ops
}

fn pick(rng: &mut ChaCha8Rng, pool: &[&str]) -> String {
    pool.choose(rng)
        .map_or_else(|| "x".to_string(), |s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic_and_varied() {
        let p = Profile {
            faults: true,
            kill_unlatchd: true,
            real_fs: true,
        };
        let a = generate(42, 300, p);
        assert_eq!(a, generate(42, 300, p));
        assert_ne!(a, generate(43, 300, p));
        assert_eq!(a.len(), 300);
        assert!(a.iter().any(Op::is_mac));
        assert!(a.iter().any(Op::is_fault));
        assert!(a.iter().any(|o| matches!(o, Op::Quiesce)));
        let scripted = generate(
            42,
            300,
            Profile {
                faults: false,
                kill_unlatchd: false,
                real_fs: false,
            },
        );
        assert!(!scripted
            .iter()
            .any(|o| o.is_fault() || matches!(o, Op::Hardlink { .. } | Op::SymlinkOut { .. })));
    }

    #[test]
    fn content_is_unique_per_nonce() {
        assert_ne!(content(1, "agent"), content(2, "agent"));
        assert_ne!(content(1, "agent"), content(1, "mac"));
        assert!(content(7, "t").starts_with(b"t#7\n"));
    }
}
