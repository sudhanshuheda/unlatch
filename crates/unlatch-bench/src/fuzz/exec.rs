//! Executing fuzz ops against a world, and the invariant sweep at every quiesce.

use super::ops::{content, Op, NAMES};
use super::tracker::{hash, MacKnows, Tracker};
use crate::fpsim::check::materialize_and_check_realpaths;
use crate::fpsim::e2e::RealWorld;
use crate::fpsim::scripted::ReplyFault;
use crate::fpsim::vmfs::{VmFs, VmKind};
use crate::fpsim::world::{ScriptedWorld, World};
use anyhow::Result;
use std::time::Duration;
use unlatch_proto::Kind;

/// World operations the fuzzer needs beyond [`World`].
pub trait FuzzWorld: World {
    /// `Engine::drop_connection()` (the engine reconnects by itself).
    fn drop_connection(&mut self) -> Result<()>;
    /// SIGKILL the daemon; the engine respawns it.
    fn kill_unlatchd(&mut self) -> Result<()>;
    /// Arm `die_after_commit:<op>` on the next daemon. `Ok(false)` = unsupported here.
    fn arm_wire_fault(&mut self, op: &str) -> Result<bool>;
    /// Restart the daemon with an arbitrary `UNLATCH_FAULT` token (e.g.
    /// `overflow_after_events:<n>`). `Ok(false)` = unsupported here.
    fn arm_daemon_fault(&mut self, _token: &str) -> Result<bool> {
        Ok(false)
    }
    /// Restart the engine from an empty state dir (forces a full snapshot) without waiting for
    /// it to finish. `Ok(false)` = unsupported here.
    fn restart_engine_fresh(&mut self) -> Result<bool>;
    /// Absolute path outside the root (target of out-of-root symlinks).
    fn outside_target(&self) -> String;
    /// Nothing outside the root changed; every materialized realpath stays in the domain.
    fn extra_checks(&mut self) -> Result<Vec<String>>;
    /// Provider-side log lines for failure reports (verbose replays).
    fn provider_log(&self) -> Vec<String> {
        Vec::new()
    }
}

impl FuzzWorld for ScriptedWorld {
    fn drop_connection(&mut self) -> Result<()> {
        self.engine.set_online(false);
        self.engine.set_online(true);
        Ok(())
    }

    fn kill_unlatchd(&mut self) -> Result<()> {
        self.drop_connection()
    }

    fn arm_wire_fault(&mut self, _op: &str) -> Result<bool> {
        Ok(false)
    }

    fn restart_engine_fresh(&mut self) -> Result<bool> {
        Ok(false)
    }

    fn outside_target(&self) -> String {
        "/nonexistent-unlatch-outside/dir".into()
    }

    fn provider_log(&self) -> Vec<String> {
        self.engine.log()
    }

    fn extra_checks(&mut self) -> Result<Vec<String>> {
        let stamp = self.sim().now().as_nanos();
        let dest = self.scratch().join(format!("mat-{stamp}"));
        let visible = self.sim().visible();
        let p = materialize_and_check_realpaths(&visible, &dest)?;
        let _ = std::fs::remove_dir_all(&dest);
        Ok(p)
    }
}

impl FuzzWorld for RealWorld {
    fn drop_connection(&mut self) -> Result<()> {
        RealWorld::drop_connection(self)
    }

    fn kill_unlatchd(&mut self) -> Result<()> {
        RealWorld::kill_unlatchd(self)?;
        Ok(())
    }

    fn arm_wire_fault(&mut self, op: &str) -> Result<bool> {
        RealWorld::arm_wire_fault(self, op)?;
        Ok(true)
    }

    fn arm_daemon_fault(&mut self, token: &str) -> Result<bool> {
        RealWorld::arm_daemon_fault(self, token)?;
        Ok(true)
    }

    fn restart_engine_fresh(&mut self) -> Result<bool> {
        RealWorld::restart_engine_fresh(self)?;
        Ok(true)
    }

    fn outside_target(&self) -> String {
        self.outside().join("dir").to_string_lossy().into_owned()
    }

    fn extra_checks(&mut self) -> Result<Vec<String>> {
        let mut v = self.outside_changes()?;
        v.extend(self.realpath_problems()?);
        Ok(v)
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn parent(p: &str) -> &str {
    p.rsplit_once('/').map_or("", |(d, _)| d)
}

/// Current VM entries: (regular files, symlinks, dirs including the root "").
pub fn vm_entries(vm: &mut dyn VmFs) -> (Vec<String>, Vec<String>, Vec<String>) {
    let (mut files, mut links, mut dirs) = (Vec::new(), Vec::new(), vec![String::new()]);
    let mut stack = vec![String::new()];
    while let Some(d) = stack.pop() {
        let Ok(names) = vm.list(&d) else { continue };
        for n in names {
            let p = join(&d, &n);
            match vm.kind(&p) {
                Some(VmKind::File) => files.push(p),
                Some(VmKind::Symlink) => links.push(p),
                Some(VmKind::Dir) => {
                    dirs.push(p.clone());
                    stack.push(p);
                }
                _ => {}
            }
        }
    }
    files.sort();
    links.sort();
    dirs.sort();
    (files, links, dirs)
}

fn pick(v: &[String], i: u32) -> Option<String> {
    (!v.is_empty()).then(|| v[i as usize % v.len()].clone())
}

/// Every regular file at or below `path` (for "the agent destroys these bytes").
fn files_below(vm: &mut dyn VmFs, path: &str) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    match vm.kind(path) {
        Some(VmKind::File) => out.extend(vm.read(path).ok()),
        Some(VmKind::Dir) => {
            for n in vm.list(path).unwrap_or_default() {
                out.extend(files_below(vm, &join(path, &n)));
            }
        }
        _ => {}
    }
    out
}

fn agent_write<W: FuzzWorld>(
    w: &mut W,
    t: &mut Tracker,
    path: &str,
    bytes: &[u8],
    idx: usize,
) -> Result<()> {
    match w.vm().kind(path) {
        Some(VmKind::File) => {
            if let Ok(old) = w.vm().read(path) {
                t.agent_destroys(&old);
            }
        }
        None => {}
        Some(_) => return Ok(()),
    }
    if w.vm().write(path, bytes).is_ok() {
        let v = mac_version(w, path);
        let sets = w.sim().change_sets_applied();
        t.agent_wrote(path, bytes, idx, v, sets);
    }
    Ok(())
}

/// The version the Mac currently holds for the item at (VM) `path`, if it knows the item.
fn mac_version<W: World>(w: &mut W, path: &str) -> Option<unlatch_proto::Version> {
    let sim = w.sim();
    let k = sim.disk().resolve(path)?;
    sim.disk().get(k)?.version
}

/// The VM has an entry at the VM path corresponding to Mac path `p` (display markers removed).
fn vm_has<W: World>(w: &mut W, p: &str) -> bool {
    let real: Vec<String> = p
        .split('/')
        .map(|c| crate::fpsim::names::strip_unlatch_marker(c).unwrap_or_else(|| c.to_string()))
        .collect();
    w.vm().kind(&real.join("/")).is_some()
}

/// Enumerate every folder at and below `path` on the Mac.
fn expand<W: World>(w: &mut W, path: &str) {
    let prefix = format!("{path}/");
    for _ in 0..64 {
        let todo: Vec<String> = w
            .sim()
            .visible()
            .into_iter()
            .filter(|v| {
                v.kind == Kind::Dir
                    && !v.enumerated
                    && v.id.is_some()
                    && (v.path == path || v.path.starts_with(&prefix))
            })
            .map(|v| v.path)
            .collect();
        if todo.is_empty() {
            return;
        }
        for d in todo {
            let _ = w.sim().browse(&d);
        }
    }
}

/// The provider's path for a Mac path: every component under the name the provider last
/// reported (not a local bounce, MQ-016, nor a still-pending local rename), so the tracker
/// compares like with like.
fn provider_path<W: World>(w: &mut W, mac_path: &str) -> String {
    let disk = w.sim().disk();
    let Some(mut k) = disk.resolve(mac_path) else {
        return mac_path.to_string();
    };
    let mut parts = Vec::new();
    while let Some(n) = disk.get(k) {
        let Some(p) = n.parent else { break };
        let name = n.server_name.clone().or_else(|| n.bounced_from.clone());
        parts.push(name.unwrap_or_else(|| n.name.clone()));
        k = p;
    }
    parts.reverse();
    parts.join("/")
}

/// What the Mac holds at and below `path` (for write-loss permissions).
fn mac_knows<W: World>(w: &mut W, path: &str) -> Vec<MacKnows> {
    let prefix = format!("{path}/");
    w.sim()
        .visible()
        .into_iter()
        .filter(|v| v.path == path || v.path.starts_with(&prefix))
        .map(|v| {
            let version = w
                .sim()
                .disk()
                .resolve(&v.path)
                .and_then(|k| w.sim().disk().get(k))
                .and_then(|n| n.version);
            MacKnows {
                path: provider_path(w, &v.path),
                version,
                content: v.content.as_deref().map(hash),
            }
        })
        .collect()
}

/// Mac-side view used to resolve picks.
struct MacView {
    files: Vec<String>,
    dirs: Vec<String>,
    open_dirs: Vec<String>,
    items: Vec<String>,
}

fn mac_view<W: World>(w: &mut W) -> MacView {
    let vis = w.sim().visible();
    let mut v = MacView {
        files: Vec::new(),
        dirs: vec![String::new()],
        open_dirs: vec![String::new()],
        items: Vec::new(),
    };
    for i in vis.into_iter().filter(|i| !i.local_only) {
        if i.kind == Kind::Dir {
            v.dirs.push(i.path.clone());
            if i.enumerated {
                v.open_dirs.push(i.path.clone());
            }
        } else {
            v.files.push(i.path.clone());
        }
        v.items.push(i.path);
    }
    v
}

/// Execute one op. Ops whose target does not exist (any more) are no-ops. Returns a log line.
pub fn exec_op<W: FuzzWorld>(w: &mut W, op: &Op, idx: usize, t: &mut Tracker) -> Result<String> {
    let mut note = String::new();
    match op {
        Op::Write { dir, name, n } => {
            let (_, _, dirs) = vm_entries(w.vm());
            if let Some(d) = pick(&dirs, *dir) {
                let p = join(&d, name);
                agent_write(w, t, &p, &content(*n, "agent"), idx)?;
                note = p;
            }
        }
        Op::Overwrite { file, n } => {
            let (files, _, _) = vm_entries(w.vm());
            if let Some(p) = pick(&files, *file) {
                agent_write(w, t, &p, &content(*n, "agent"), idx)?;
                note = p;
            }
        }
        Op::Append { file, n } => {
            let (files, _, _) = vm_entries(w.vm());
            if let Some(p) = pick(&files, *file) {
                let old = w.vm().read(&p).unwrap_or_default();
                let extra = content(*n, "append");
                if w.vm().append(&p, &extra).is_ok() {
                    t.agent_destroys(&old);
                    let mut all = old;
                    all.extend_from_slice(&extra);
                    {
                        let v = mac_version(w, &p);
                        let sets = w.sim().change_sets_applied();
                        t.agent_wrote(&p, &all, idx, v, sets);
                    }
                }
                note = p;
            }
        }
        Op::AtomicReplace { file, n } => {
            let (files, _, _) = vm_entries(w.vm());
            if let Some(p) = pick(&files, *file) {
                let name = p.rsplit('/').next().unwrap_or(&p).to_string();
                let tmp = join(parent(&p), &format!(".{name}.tmp~"));
                let bytes = content(*n, "agent");
                if w.vm().kind(&tmp).is_none() && w.vm().write(&tmp, &bytes).is_ok() {
                    let old = w.vm().read(&p).ok();
                    if w.vm().rename(&tmp, &p).is_ok() {
                        if let Some(old) = old {
                            t.agent_destroys(&old);
                        }
                        {
                            let v = mac_version(w, &p);
                            let sets = w.sim().change_sets_applied();
                            t.agent_wrote(&p, &bytes, idx, v, sets);
                        }
                    } else {
                        let _ = w.vm().remove_file(&tmp);
                    }
                }
                note = p;
            }
        }
        Op::Rename { src, dir, name } => {
            let (files, links, dirs) = vm_entries(w.vm());
            let all: Vec<String> = files
                .iter()
                .chain(&links)
                .chain(dirs.iter().filter(|d| !d.is_empty()))
                .cloned()
                .collect();
            if let (Some(s), Some(d)) = (pick(&all, *src), pick(&dirs, *dir)) {
                let to = join(&d, name);
                let into_self = to == s
                    || to.starts_with(&format!("{s}/"))
                    || d == s
                    || d.starts_with(&format!("{s}/"));
                let dest_kind = w.vm().kind(&to);
                let src_kind = w.vm().kind(&s);
                let ok = !into_self
                    && match dest_kind {
                        None => true,
                        Some(VmKind::Dir) => {
                            src_kind == Some(VmKind::Dir)
                                && w.vm().list(&to).map(|l| l.is_empty()).unwrap_or(false)
                        }
                        Some(_) => src_kind != Some(VmKind::Dir),
                    };
                if ok {
                    let doomed = if dest_kind == Some(VmKind::File) {
                        w.vm().read(&to).ok()
                    } else {
                        None
                    };
                    if w.vm().rename(&s, &to).is_ok() {
                        if let Some(d) = doomed {
                            t.agent_destroys(&d);
                        }
                        t.agent_moved(&s, &to);
                    }
                    note = format!("{s} -> {to}");
                }
            }
        }
        Op::Mkdir { dir, name } => {
            let (_, _, dirs) = vm_entries(w.vm());
            if let Some(d) = pick(&dirs, *dir) {
                let p = join(&d, name);
                if w.vm().kind(&p).is_none() && w.vm().mkdir(&p).is_ok() {
                    t.vm_touch(&p);
                }
                note = p;
            }
        }
        Op::Rm { file } => {
            let (files, links, _) = vm_entries(w.vm());
            let all: Vec<String> = files.into_iter().chain(links).collect();
            if let Some(p) = pick(&all, *file) {
                let old = w.vm().read(&p).ok();
                if w.vm().remove_file(&p).is_ok() {
                    if let Some(o) = old {
                        t.agent_destroys(&o);
                    }
                    t.vm_touch(&p);
                }
                note = p;
            }
        }
        Op::RmRf { dir } => {
            let (_, _, dirs) = vm_entries(w.vm());
            let dirs: Vec<String> = dirs.into_iter().filter(|d| !d.is_empty()).collect();
            if let Some(d) = pick(&dirs, *dir) {
                let doomed = files_below(w.vm(), &d);
                if w.vm().remove_dir_all(&d).is_ok() {
                    for b in doomed {
                        t.agent_destroys(&b);
                    }
                    t.vm_touch(&d);
                }
                note = d;
            }
        }
        Op::SymlinkIn { dir, name, target } => note = make_symlink(w, t, *dir, name, target),
        Op::SymlinkOut { dir, name } => {
            let target = w.outside_target();
            note = make_symlink(w, t, *dir, name, &target);
        }
        Op::Hardlink { file, dir, name } => {
            let (files, _, dirs) = vm_entries(w.vm());
            if let (Some(f), Some(d)) = (pick(&files, *file), pick(&dirs, *dir)) {
                let p = join(&d, name);
                if w.vm().kind(&p).is_none() && w.vm().hard_link(&f, &p).is_ok() {
                    // link(2) changes the inode's ctime: a content version of `f` too (D3).
                    t.vm_touch(&f);
                    t.vm_touch(&p);
                }
                note = format!("{f} => {p}");
            }
        }
        Op::Chmod { file, mode } => {
            let (files, _, _) = vm_entries(w.vm());
            if let Some(p) = pick(&files, *file) {
                let _ = w.vm().set_mode(&p, *mode);
                t.vm_touch(&p);
                note = format!("{p} {mode:o}");
            }
        }
        Op::Churn { file, n, count } => {
            let (files, _, _) = vm_entries(w.vm());
            if let Some(p) = pick(&files, *file) {
                let old = w.vm().read(&p).unwrap_or_default();
                let len = old.len().max(24);
                let mut last = Vec::new();
                for i in 0..u64::from(*count) {
                    let mut b = content(n.wrapping_mul(64) + i, "churn");
                    b.resize(len, b'.');
                    if w.vm().write(&p, &b).is_err() {
                        break;
                    }
                    last = b;
                }
                if !last.is_empty() {
                    t.agent_destroys(&old);
                    {
                        let v = mac_version(w, &p);
                        let sets = w.sim().change_sets_applied();
                        t.agent_wrote(&p, &last, idx, v, sets);
                    }
                }
                note = p;
            }
        }
        Op::MassChange { dir, n, count } => {
            let (_, _, dirs) = vm_entries(w.vm());
            if let Some(d) = pick(&dirs, *dir) {
                for i in 0..u64::from(*count) {
                    let p = join(&d, &format!("gen{}.txt", (n + i) % 23));
                    t.note_name(p.rsplit('/').next().unwrap_or(&p));
                    if i % 3 == 0 && w.vm().kind(&p) == Some(VmKind::File) {
                        if let Ok(old) = w.vm().read(&p) {
                            if w.vm().remove_file(&p).is_ok() {
                                t.agent_destroys(&old);
                                t.vm_touch(&p);
                            }
                        }
                    } else {
                        agent_write(w, t, &p, &content(n.wrapping_mul(1000) + i, "mass"), idx)?;
                    }
                }
                note = d;
            }
        }
        Op::MvThenTouch { file, name, n } => {
            let (files, _, _) = vm_entries(w.vm());
            if let Some(a) = pick(&files, *file) {
                let b = join(parent(&a), name);
                let bk = w.vm().kind(&b);
                if b != a && bk != Some(VmKind::Dir) {
                    let doomed = if bk == Some(VmKind::File) {
                        w.vm().read(&b).ok()
                    } else {
                        None
                    };
                    if w.vm().rename(&a, &b).is_ok() {
                        if let Some(d) = doomed {
                            t.agent_destroys(&d);
                        }
                        t.agent_moved(&a, &b);
                        agent_write(w, t, &a, &content(*n, "touch"), idx)?;
                    }
                }
                note = format!("{a} -> {b}; touch {a}");
            }
        }
        Op::RmThenMkdir { file } => {
            let (files, _, _) = vm_entries(w.vm());
            if let Some(p) = pick(&files, *file) {
                let old = w.vm().read(&p).ok();
                if w.vm().remove_file(&p).is_ok() {
                    if let Some(o) = old {
                        t.agent_destroys(&o);
                    }
                    let _ = w.vm().mkdir(&p);
                    t.vm_touch(&p);
                }
                note = p;
            }
        }
        Op::SwapDirForSymlink { dir } => {
            let (_, _, dirs) = vm_entries(w.vm());
            let dirs: Vec<String> = dirs.into_iter().filter(|d| !d.is_empty()).collect();
            if let Some(d) = pick(&dirs, *dir) {
                let old = format!("{d}.old");
                if w.vm().kind(&old).is_none() && w.vm().rename(&d, &old).is_ok() {
                    t.agent_moved(&d, &old);
                    t.note_name(old.rsplit('/').next().unwrap_or(&old));
                    let target = w.outside_target();
                    let _ = w.vm().symlink(&target, &d);
                }
                note = d;
            }
        }
        Op::MacBrowse { dir } => {
            let v = mac_view(w);
            if let Some(d) = pick(&v.dirs, *dir) {
                note = describe(w.sim().browse(&d).map(|_| d.clone()));
            }
        }
        Op::MacOpen { file } => {
            let v = mac_view(w);
            if let Some(p) = pick(&v.files, *file) {
                note = describe(w.sim().open(&p).map(|b| format!("{p} ({} bytes)", b.len())));
            }
        }
        Op::MacEdit { file, n } | Op::MacSave { file, n } => {
            let v = mac_view(w);
            if let Some(p) = pick(&v.files, *file) {
                // Apps read before they write: what the Mac overwrites is what it just fetched.
                // An app whose read fails does not save (a lost fetch reply, LoseReply{3}, is
                // retried once — the fault is one-shot). Writing anyway would materialize the
                // file inside the write, *after* `knows` was taken, and look like destroying
                // agent bytes the Mac was never shown (seed 67).
                if w.sim().open(&p).is_err() && w.sim().open(&p).is_err() {
                    note = format!("{p} (read failed twice: not edited)");
                    w.sim().pump(Duration::ZERO);
                    return Ok(note);
                }
                let knows = mac_knows(w, &p);
                let pp = provider_path(w, &p);
                let sets = w.sim().change_sets_applied();
                t.mac_destroys(&pp, &knows, sets);
                t.mac_wrote(&pp, *n, false, false);
                let bytes = content(*n, "mac");
                let r = if matches!(op, Op::MacEdit { .. }) {
                    w.sim().write(&p, &bytes)
                } else {
                    w.sim().save_atomic(&p, &bytes)
                };
                note = describe(r.map(|_| p));
            }
        }
        Op::MacCreate { dir, name, n } => {
            let v = mac_view(w);
            if let Some(d) = pick(&v.open_dirs, *dir) {
                let r = w.sim().create_file(&d, name, &content(*n, "mac"));
                if let Ok(p) = &r {
                    let taken = vm_has(w, p);
                    let pp = provider_path(w, p);
                    t.mac_wrote(&pp, *n, taken, true);
                }
                note = describe(r);
            }
        }
        Op::MacMkdir { dir, name } => {
            let v = mac_view(w);
            if let Some(d) = pick(&v.open_dirs, *dir) {
                let r = w.sim().mkdir(&d, name);
                if let Ok(p) = &r {
                    t.mac_touch(p);
                }
                note = describe(r);
            }
        }
        Op::MacRename { item, name } => {
            let v = mac_view(w);
            if let Some(p) = pick(&v.items, *item) {
                let to = join(parent(&p), name);
                let from = provider_path(w, &p);
                // Where the rename lands on the VM: the provider's path of the parent plus the
                // new name. (provider_path of the renamed item itself still reports the old
                // name until the engine acknowledges the rename.)
                let dest = join(&provider_path(w, parent(&p)), name);
                let r = w.sim().rename(&p, name);
                if r.is_ok() {
                    t.mac_moved(&from, &dest);
                }
                note = describe(r.map(|_| format!("{p} -> {to}")));
            }
        }
        Op::MacMove { item, dir } => {
            let v = mac_view(w);
            if let (Some(p), Some(d)) = (pick(&v.items, *item), pick(&v.open_dirs, *dir)) {
                let to = join(&d, p.rsplit('/').next().unwrap_or(&p));
                let from = provider_path(w, &p);
                let r = w.sim().move_to(&p, &d);
                if r.is_ok() {
                    let to = provider_path(w, &to);
                    t.mac_moved(&from, &to);
                }
                note = describe(r.map(|_| format!("{p} -> {d}/")));
            }
        }
        Op::MacDelete { item } => {
            let v = mac_view(w);
            if let Some(p) = pick(&v.items, *item) {
                // Finder deletes what the user sees: expand the folder first, so what the Mac
                // knew is well defined (everything it did not list must survive, D7).
                expand(w, &p);
                let knows = mac_knows(w, &p);
                let pp = provider_path(w, &p);
                let sets = w.sim().change_sets_applied();
                t.mac_destroys(&pp, &knows, sets);
                note = describe(w.sim().delete(&p).map(|_| p));
            }
        }
        Op::MacDragIn { dir, n, count } => {
            let v = mac_view(w);
            if let Some(d) = pick(&v.open_dirs, *dir) {
                let files: Vec<(String, Vec<u8>)> = (0..u64::from(*count))
                    .map(|i| {
                        (
                            NAMES[((n + i) % NAMES.len() as u64) as usize].to_string(),
                            content(n.wrapping_mul(97) + i, "drag"),
                        )
                    })
                    .collect();
                let r = w.sim().drag_in(&d, &files);
                if let Ok(ps) = &r {
                    for (i, p) in ps.iter().enumerate() {
                        let taken = vm_has(w, p);
                        let pp = provider_path(w, p);
                        t.mac_wrote(&pp, n.wrapping_mul(97) + i as u64, taken, true);
                    }
                }
                note = describe(r.map(|ps| ps.join(", ")));
            }
        }
        Op::MacChmod { file, exec } => {
            let v = mac_view(w);
            if let Some(p) = pick(&v.files, *file) {
                note = describe(w.sim().chmod_exec(&p, *exec).map(|_| p));
            }
        }
        Op::MacTag { item } => {
            let v = mac_view(w);
            if let Some(p) = pick(&v.items, *item) {
                note = describe(w.sim().set_tags(&p, b"bplist00-fuzz-tag").map(|_| p));
            }
        }
        Op::MacEvict { file } => {
            let v = mac_view(w);
            if let Some(p) = pick(&v.files, *file) {
                note = describe(w.sim().evict(&p).map(|_| p));
            }
        }
        Op::DropConnection => {
            if !t.offline {
                w.drop_connection()?;
            }
        }
        Op::KillUnlatchd => {
            if !t.offline {
                w.kill_unlatchd()?;
                t.daemon_killed();
            }
        }
        Op::GoOffline => {
            if !t.offline {
                w.set_online(false)?;
                t.offline = true;
            }
        }
        Op::GoOnline => {
            if t.offline {
                w.set_online(true)?;
                t.offline = false;
            }
        }
        Op::RestartEngine => {
            if !t.offline {
                w.restart_engine()?;
            }
        }
        // Arming restarts the engine (as a child with UNLATCH_FAULT), which waits until it is
        // live: impossible while the link is cut (like RestartEngine, skipped then).
        Op::LoseReply { .. } if t.offline => {}
        Op::LoseReply { kind } => {
            let f = match kind % 4 {
                0 => ReplyFault::Create,
                1 => ReplyFault::Modify,
                2 => ReplyFault::Delete,
                _ => ReplyFault::Fetch,
            };
            note = format!("armed: {}", w.arm_reply_fault(f)?);
        }
        Op::AdvanceTime { secs } => w.sim().advance(Duration::from_secs(u64::from(*secs))),
        Op::Quiesce => {}
    }
    // fileproviderd works in the background: whatever is due now happens now.
    w.sim().pump(Duration::ZERO);
    Ok(note)
}

fn make_symlink<W: FuzzWorld>(
    w: &mut W,
    t: &mut Tracker,
    dir: u32,
    name: &str,
    target: &str,
) -> String {
    let (_, _, dirs) = vm_entries(w.vm());
    let Some(d) = pick(&dirs, dir) else {
        return String::new();
    };
    let p = join(&d, name);
    if w.vm().kind(&p).is_none() && w.vm().symlink(target, &p).is_ok() {
        t.vm_touch(&p);
    }
    format!("{p} -> {target}")
}

fn describe<T: std::fmt::Display, E: std::fmt::Display>(r: std::result::Result<T, E>) -> String {
    match r {
        Ok(v) => v.to_string(),
        Err(e) => format!("(refused: {e})"),
    }
}

/// Settle, converge, and run every invariant. Empty = all good.
pub fn check_all<W: FuzzWorld>(w: &mut W, t: &Tracker) -> Result<Vec<String>> {
    let mut problems = w.converged()?;
    let tree = w.vm_tree()?;
    problems.extend(t.check(&tree));
    problems.extend(w.extra_checks()?);
    Ok(problems)
}

/// Outcome of executing a whole op list.
#[derive(Clone, Debug, Default)]
pub struct RunOutcome {
    /// Index of the op after which the invariants first failed (`ops.len()` = final check).
    pub failed_at: Option<usize>,
    pub problems: Vec<String>,
    pub log: Vec<String>,
}

/// Run `ops` in a fresh world: add the domain, execute, check at every `Quiesce` and at the end.
pub fn run_ops<W: FuzzWorld>(w: &mut W, ops: &[Op], verbose: bool) -> Result<RunOutcome> {
    let mut t = Tracker::new();
    for n in super::ops::NAMES.iter().chain(super::ops::DIR_NAMES) {
        t.note_name(n);
    }
    let mut out = RunOutcome::default();
    w.sim()
        .add_domain()
        .map_err(|e| anyhow::anyhow!("add_domain: {e}"))?;
    let t0 = std::time::Instant::now();
    for (i, op) in ops.iter().enumerate() {
        let t_op = std::time::Instant::now();
        let note = exec_op(w, op, i, &mut t)?;
        let line = format!(
            "#{i:<4} {op}  {note}  [{:.2}s, at {:.1}s]",
            t_op.elapsed().as_secs_f64(),
            t0.elapsed().as_secs_f64()
        );
        if verbose {
            eprintln!("{line}");
        }
        out.log.push(line);
        if matches!(op, Op::Quiesce) {
            if t.offline {
                w.set_online(true)?;
                t.offline = false;
            }
            let tq = std::time::Instant::now();
            let p = check_all(w, &t)?;
            if verbose {
                eprintln!("      quiesce check {:.2}s", tq.elapsed().as_secs_f64());
            }
            if !p.is_empty() {
                out.failed_at = Some(i);
                out.problems = p;
                if verbose {
                    dump(w);
                }
                return Ok(out);
            }
            let tree = w.vm_tree()?;
            t.quiesced(&tree);
        }
    }
    if t.offline {
        w.set_online(true)?;
        t.offline = false;
    }
    let p = check_all(w, &t)?;
    if !p.is_empty() {
        out.failed_at = Some(ops.len());
        out.problems = p;
    }
    if verbose && out.failed_at.is_some() {
        dump(w);
    }
    Ok(out)
}

fn dump<W: FuzzWorld>(w: &mut W) {
    eprintln!("--- provider calls ---");
    for c in &w.sim().history {
        eprintln!(
            "{:>10.3}s {:<12} id={:?} fields={:#x} -> {:?}",
            c.at.as_secs_f64(),
            c.kind,
            c.id,
            c.fields,
            c.outcome
        );
    }
    eprintln!("--- provider log ---");
    for l in w.provider_log() {
        eprintln!("{l}");
    }
    eprintln!("--- Mac tree ---");
    for v in w.sim().visible() {
        eprintln!(
            "{:?} {} id={:?} enumerated={} pending={} sync_error={} local_only={}",
            v.kind, v.path, v.id, v.enumerated, v.pending, v.sync_error, v.local_only
        );
    }
}
