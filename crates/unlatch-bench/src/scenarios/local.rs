//! "Local" baselines: the same operation directly on the VM-side directory (the native target).

use super::{ls_la, mbit, read_file, t8_file, Ctx};
use crate::measure::{Better, Measurement, Samples, Status, System};
use crate::targets::metric as m;
use anyhow::Result;
use std::io::Write;
use std::time::Instant;

pub fn run(ctx: &Ctx, id: &str) -> Result<Vec<Measurement>> {
    let row = |metric: &str, unit: &str, better| ctx.row(id, metric, System::Local, unit, better);
    let flat = ctx.tree.root.join("flat");
    let n = if ctx.quick { 50 } else { 200 };
    Ok(match id {
        "T1" => {
            // Engine `list` returns metadata for every child: the local analogue is
            // readdir + lstat of each entry.
            let mut s = Samples::new();
            for _ in 0..n {
                let t = Instant::now();
                let mut k = 0;
                for e in std::fs::read_dir(&flat)? {
                    let e = e?;
                    std::fs::symlink_metadata(e.path())?;
                    k += 1;
                }
                s.push(t.elapsed());
                debug_assert_eq!(k, ctx.tree.spec.flat_entries);
            }
            vec![row(m::LIST_1000_P50_MS, "ms", Better::Lower)
                .value(s.p50().unwrap_or(0.0), s.len())]
        }
        "T2" => {
            let paths: Vec<_> = ctx
                .tree
                .flat_small
                .iter()
                .map(|p| ctx.tree.path(p))
                .collect();
            let mut s = Samples::new();
            for i in 0..10_000 {
                let p = &paths[i % paths.len()];
                let t = Instant::now();
                std::fs::symlink_metadata(p)?;
                s.push(t.elapsed());
            }
            vec![row(m::STAT_P50_US, "us", Better::Lower)
                .value(s.p50().unwrap_or(0.0) * 1e3, s.len())]
        }
        "T3" => {
            ls_la(&flat)?; // warm the dentry/inode caches, like the other systems' warm runs
            let mut s = Samples::new();
            for _ in 0..(if ctx.quick { 10 } else { 30 }) {
                s.push(ls_la(&flat)?);
            }
            vec![row(m::LS_LA_MS, "ms", Better::Lower).value(s.p50().unwrap_or(0.0), s.len())]
        }
        "T6" => {
            let paths: Vec<_> = ctx
                .tree
                .flat_small
                .iter()
                .take(n)
                .map(|p| ctx.tree.path(p))
                .collect();
            for p in &paths {
                read_file(p)?;
            }
            let mut s = Samples::new();
            for p in &paths {
                s.push(read_file(p)?.1);
            }
            vec![row(m::OPEN_SMALL_WARM_P50_MS, "ms", Better::Lower)
                .value(s.p50().unwrap_or(0.0), s.len())]
        }
        "T8" => {
            let (p, size) = t8_file(ctx)?;
            let (got, d) = read_file(&p)?;
            if got != size {
                anyhow::bail!("short read {got} != {size}");
            }
            vec![row(m::THROUGHPUT_MBIT, "Mbit/s", Better::Higher)
                .value(mbit(size, d), 1)
                .detail(format!("{} MiB from page cache", size >> 20))]
        }
        "T9" => {
            let dir = ctx.vm_scratch("t9-local")?;
            let data = vec![0x42u8; 4096];
            let mut s = Samples::new();
            for i in 0..(if ctx.quick { 10 } else { 30 }) {
                let t = Instant::now();
                let p = dir.path.join(format!("u{i}.txt"));
                let mut f = std::fs::File::create(&p)?;
                f.write_all(&data)?;
                f.sync_all()?;
                std::fs::File::open(&dir.path)?.sync_all()?;
                s.push(t.elapsed());
            }
            vec![row(m::UPLOAD_4K_P50_MS, "ms", Better::Lower)
                .value(s.p50().unwrap_or(0.0), s.len())
                .detail("write + fsync(file) + fsync(dir)")]
        }
        "T11" => {
            let t = Instant::now();
            let n = walk(&ctx.tree.root, None)?.0;
            vec![row(m::FULL_TREE_KNOWN_S, "s", Better::Lower)
                .value(t.elapsed().as_secs_f64(), 1)
                .detail(format!(
                    "recursive readdir + lstat of {n} entries (page cache warm)"
                ))]
        }
        other => vec![row("*", "", Better::Lower)
            .status(Status::Skipped, format!("{other}: no local baseline"))],
    })
}

/// Recursive readdir + lstat (like `find -ls`). With a deadline, stops early and returns
/// `(entries seen, finished)`.
pub fn walk(root: &std::path::Path, deadline: Option<Instant>) -> Result<(u64, bool)> {
    let mut n = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let md = std::fs::symlink_metadata(e.path())?;
            n += 1;
            if md.is_dir() {
                stack.push(e.path());
            }
            if let Some(dl) = deadline {
                if Instant::now() > dl {
                    return Ok((n, false));
                }
            }
        }
    }
    Ok((n, true))
}
