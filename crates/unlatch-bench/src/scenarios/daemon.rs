//! Daemon-level scenarios (T15, T16, T17): `unlatchd stdio` driven directly over the wire
//! protocol, no engine and no shaping.

use super::Ctx;
use crate::measure::{Better, Measurement, Status, System};
use crate::targets::metric as m;
use crate::wire_client::{inotify_watches, WireClient};
use anyhow::{anyhow, bail, Result};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};
use unlatch_proto::wire::{Resume, ServerMsg, WelcomeMode};
use unlatch_proto::{Entry, ItemId, Kind};

const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(300);
const REQ_TIMEOUT: Duration = Duration::from_secs(30);

fn unlatchd_argv(unlatchd: &Path, root: &Path, state: &Path) -> Vec<String> {
    vec![
        unlatchd.display().to_string(),
        "stdio".into(),
        "--root".into(),
        root.display().to_string(),
        "--state".into(),
        state.display().to_string(),
    ]
}

pub fn run(ctx: &Ctx, id: &str) -> Result<Vec<Measurement>> {
    let row = |metric: &str, unit: &str, better| ctx.row(id, metric, System::Unlatch, unit, better);
    let Some(unlatchd) = ctx.bins.unlatchd.clone().filter(|p| p.is_file()) else {
        return Ok(vec![row("*", "", Better::Lower).status(
            Status::Unavailable,
            "unlatchd binary not found (cargo build --release -p unlatchd)",
        )]);
    };
    let spawn = |root: &Path, state: &Path| -> Result<WireClient> {
        WireClient::spawn(&unlatchd_argv(&unlatchd, root, state), &[], Some(&ctx.log))
            .map_err(|e| anyhow!("unlatchd stdio not available: {e:#}"))
    };
    let unavailable = |e: anyhow::Error| {
        vec![row("*", "", Better::Lower).status(Status::Unavailable, format!("{e:#}"))]
    };
    Ok(match id {
        "T15" => {
            let files = if ctx.quick { 50_000 } else { 500_000 };
            let (root, pkgs) = crate::tree::ensure_wide_lazy(&ctx.cache, files, 250)?;
            let state = ctx.local_scratch("t15-state")?;
            let mut c = match spawn(&root, &state.path) {
                Ok(c) => c,
                Err(e) => return Ok(unavailable(e)),
            };
            let w = c.hello(&root.display().to_string(), None, REQ_TIMEOUT)?;
            let mut snap: Vec<Entry> = Vec::new();
            if w.mode == WelcomeMode::Snapshot {
                c.drain_snapshot(SNAPSHOT_TIMEOUT, Some(&mut snap))?;
            }
            let nm = snap
                .iter()
                .find(|e| e.parent == ItemId::ROOT && e.name == "node_modules")
                .ok_or_else(|| anyhow!("node_modules not in the root snapshot"))?
                .id;
            c.ping(REQ_TIMEOUT)?;
            let w0 = inotify_watches(c.pid())?;
            let t = Instant::now();
            let (_, entries) = c.list_dir(nm, Duration::from_secs(120))?;
            let el = t.elapsed();
            let w1 = inotify_watches(c.pid())?;
            let child_dirs = entries.iter().filter(|e| e.kind == Kind::Dir).count() as u64;
            let deeper = entries.iter().filter(|e| e.parent != nm).count();
            let all_lazy = entries
                .iter()
                .filter(|e| e.kind == Kind::Dir)
                .all(|e| e.lazy);
            let added = w1.saturating_sub(w0);
            c.close(Duration::from_secs(5)).ok();
            vec![
                row(m::LISTDIR_ENTRIES_SENT, "entries", Better::Lower)
                    .value(entries.len() as f64, 1)
                    .detail(format!(
                        "fixture {files} files in {pkgs} packages; {deeper} entries below the first level; child dirs lazy: {all_lazy}; {:.0} ms",
                        el.as_secs_f64() * 1e3
                    ))
                    .judged(format!("= 1 level ({pkgs} entries), child dirs lazy"), entries.len() as u64 == pkgs && deeper == 0 && all_lazy),
                row(m::WATCHES_ADDED, "watches", Better::Lower)
                    .value(added as f64, 1)
                    .detail(format!("inotify watches {w0} → {w1}"))
                    .judged(format!("≤ 1 + child dirs of that level ({})", 1 + child_dirs), added <= 1 + child_dirs),
            ]
        }
        "T16" => {
            let state = ctx.local_scratch("t16-state")?;
            let root = ctx.tree.root.display().to_string();
            let (index, seq) = {
                let mut c = match spawn(&ctx.tree.root, &state.path) {
                    Ok(c) => c,
                    Err(e) => return Ok(unavailable(e)),
                };
                let w = c.hello(&root, None, SNAPSHOT_TIMEOUT)?;
                let seq = if w.mode == WelcomeMode::Snapshot {
                    c.drain_snapshot(SNAPSHOT_TIMEOUT, None)?.1.max(w.seq)
                } else {
                    w.seq
                };
                let seq = seq.max(c.ping(REQ_TIMEOUT)?);
                c.close(Duration::from_secs(10))?;
                (w.index, seq)
            };
            let t = Instant::now();
            let mut c = spawn(&ctx.tree.root, &state.path)?;
            let w = c.hello(&root, Some(Resume { index, seq }), REQ_TIMEOUT)?;
            let welcome_ms = t.elapsed().as_secs_f64() * 1e3;
            // Anything snapshot-shaped in the next 500 ms counts against "0 snapshot bytes".
            let mut snap_bytes = 0u64;
            let until = Instant::now() + Duration::from_millis(500);
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                match c.recv(left) {
                    Ok(r) => {
                        if matches!(
                            r.msg,
                            ServerMsg::SnapshotChunk { .. } | ServerMsg::SnapshotDone { .. }
                        ) {
                            snap_bytes += r.frame_bytes as u64;
                        }
                    }
                    Err(_) => break,
                }
            }
            c.close(Duration::from_secs(5)).ok();
            let resumed = w.mode == WelcomeMode::Resume && w.index == index;
            vec![
                row(m::RESTART_WELCOME_MS, "ms", Better::Lower)
                    .value(welcome_ms, 1)
                    .detail(format!(
                        "spawn → Welcome({:?}) over {} entries",
                        w.mode, w.entries
                    ))
                    .judged(
                        "≤ 200 ms, Resume mode, same index".into(),
                        resumed && welcome_ms <= 200.0,
                    ),
                row(m::RESTART_SNAPSHOT_BYTES, "bytes", Better::Lower).value(snap_bytes as f64, 1),
            ]
        }
        "T17" => {
            let root = ctx.local_scratch("t17-root")?;
            let state = ctx.local_scratch("t17-state")?;
            let file = root.path.join("f.txt");
            std::fs::write(&file, vec![b'0'; 4096])?;
            let mut c = match spawn(&root.path, &state.path) {
                Ok(c) => c,
                Err(e) => return Ok(unavailable(e)),
            };
            let w = c.hello(&root.path.display().to_string(), None, REQ_TIMEOUT)?;
            let mut snap = Vec::new();
            if w.mode == WelcomeMode::Snapshot {
                c.drain_snapshot(SNAPSHOT_TIMEOUT, Some(&mut snap))?;
            }
            let fid = snap
                .iter()
                .find(|e| e.name == "f.txt")
                .ok_or_else(|| anyhow!("f.txt not in snapshot"))?
                .id;
            let trials = if ctx.quick { 200 } else { 1000 };
            let mut ok = 0u32;
            let mut f = std::fs::OpenOptions::new().write(true).open(&file)?;
            for i in 0..trials {
                let before = c.stat(fid, REQ_TIMEOUT)?.version.content;
                // Same size, new bytes, immediately (well within one timestamp tick).
                f.seek(SeekFrom::Start(0))?;
                f.write_all(&[b'a' + (i % 26) as u8; 4096])?;
                f.flush()?;
                let deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    if c.stat(fid, REQ_TIMEOUT)?.version.content != before {
                        ok += 1;
                        break;
                    }
                    if Instant::now() > deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            c.close(Duration::from_secs(5)).ok();
            vec![row(m::REWRITE_NEW_VERSION_PCT, "%", Better::Higher)
                .value(f64::from(ok) * 100.0 / f64::from(trials), trials as usize)
                .detail(format!("{ok}/{trials} same-size in-place rewrites got a new content version within 1 s"))]
        }
        other => bail!("{other}: not a daemon-level scenario"),
    })
}
