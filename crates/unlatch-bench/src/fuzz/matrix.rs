//! Deterministic fault scenarios: the crash/replay matrix of review §2(f)3 and the targeted
//! races of §2(f)4.

use super::exec::FuzzWorld;
use crate::fpsim::names::{is_conflict_copy, numbered_base};
use crate::fpsim::scenarios::Verdict;
use crate::fpsim::scripted::ReplyFault;
use crate::fpsim::vmfs::{VmKind, VmTree};
use crate::fpsim::world::QUIESCE_BUDGET;
use anyhow::{anyhow, Result};

/// A Mac-side mutation whose reply gets lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutation {
    Create,
    Mkdir,
    Modify,
    Rename,
    Delete,
}

/// Where the process dies: the engine before its IPC reply, or `unlatchd` after the durable ops
/// table write but before its wire reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hop {
    Ipc,
    Wire,
}

impl Mutation {
    pub const ALL: [Mutation; 5] = [
        Mutation::Create,
        Mutation::Mkdir,
        Mutation::Modify,
        Mutation::Rename,
        Mutation::Delete,
    ];

    /// `die_before_ipc_reply:<kind>`: creates (files and dirs) are `create`, renames are `modify`.
    pub fn reply_fault(self) -> ReplyFault {
        match self {
            Mutation::Create | Mutation::Mkdir => ReplyFault::Create,
            Mutation::Modify | Mutation::Rename => ReplyFault::Modify,
            Mutation::Delete => ReplyFault::Delete,
        }
    }

    /// `die_after_commit:<op>` of the wire request the engine sends for it.
    pub fn wire_op(self) -> &'static str {
        match self {
            Mutation::Create | Mutation::Modify => "write",
            Mutation::Mkdir => "mkdir",
            Mutation::Rename => "rename",
            Mutation::Delete => "remove",
        }
    }
}

pub fn matrix_cases() -> Vec<(Mutation, Hop)> {
    Mutation::ALL
        .iter()
        .flat_map(|m| [(*m, Hop::Ipc), (*m, Hop::Wire)])
        .collect()
}

fn mkfile<W: FuzzWorld>(w: &mut W, path: &str, body: &[u8]) -> Result<()> {
    let mut acc = String::new();
    let comps: Vec<&str> = path.split('/').collect();
    for c in &comps[..comps.len() - 1] {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(c);
        if w.vm().kind(&acc).is_none() {
            w.vm().mkdir(&acc)?;
        }
    }
    w.vm().write(path, body)?;
    Ok(())
}

fn start<W: FuzzWorld>(w: &mut W, files: &[(&str, &[u8])], browse: &[&str]) -> Result<()> {
    for (p, b) in files {
        mkfile(w, p, b)?;
    }
    w.settle()?;
    w.sim()
        .add_domain()
        .map_err(|e| anyhow!("add_domain: {e}"))?;
    for d in browse {
        w.sim().browse(d).map_err(|e| anyhow!("browse {d}: {e}"))?;
    }
    w.sync()
}

fn content_at(tree: &VmTree, p: &str) -> Option<Vec<u8>> {
    tree.nodes.get(p).and_then(|n| n.content.clone())
}

fn final_checks<W: FuzzWorld>(
    w: &mut W,
    mut problems: Vec<String>,
    allow_conflicts: bool,
) -> Result<Verdict> {
    problems.extend(w.converged()?);
    let tree = w.vm_tree()?;
    if !allow_conflicts {
        for p in tree
            .nodes
            .keys()
            .filter(|p| is_conflict_copy(p.rsplit('/').next().unwrap_or(p)))
        {
            problems.push(format!("conflict copy after a replay: {p}"));
        }
    }
    problems.extend(w.extra_checks()?);
    Ok(Verdict {
        problems,
        skipped: None,
    })
}

/// One cell of the crash/replay matrix. Pass: the op happens exactly once — no duplicate, no
/// conflict copy, nothing stuck, trees converge.
pub fn run_case<W: FuzzWorld>(w: &mut W, m: Mutation, hop: Hop) -> Result<Verdict> {
    let unavailable = |hop: Hop| {
        Ok(Verdict {
            problems: Vec::new(),
            skipped: Some(format!("{hop:?} fault injection unavailable here")),
        })
    };
    // A wire fault restarts the daemon. Arm it before the tree exists: a daemon restarted after
    // the tree was written bumps recently-scanned files (review (a)3 racy rule) and the Mac's
    // first mutation would conflict for that reason alone — reported separately in
    // docs/TESTING.md, not what this matrix measures.
    if hop == Hop::Wire && !w.arm_wire_fault(m.wire_op())? {
        return unavailable(hop);
    }
    start(
        w,
        &[("d/f.txt", b"v1"), ("d/g.txt", b"g"), ("d/sub/keep", b"k")],
        &["d"],
    )?;
    w.sim().open("d/f.txt").map_err(|e| anyhow!("{e}"))?;
    if hop == Hop::Ipc && !w.arm_reply_fault(m.reply_fault())? {
        return unavailable(hop);
    }
    let r = match m {
        Mutation::Create => w
            .sim()
            .create_file("d", "new.txt", b"created by mac")
            .map(|_| ()),
        Mutation::Mkdir => w.sim().mkdir("d", "newdir").map(|_| ()),
        Mutation::Modify => w.sim().write("d/f.txt", b"modified by mac"),
        Mutation::Rename => w.sim().rename("d/g.txt", "g2.txt"),
        Mutation::Delete => w.sim().delete("d/g.txt"),
    };
    let mut problems = Vec::new();
    if let Err(e) = r {
        problems.push(format!("user action refused: {e}"));
    }
    w.sim().pump(QUIESCE_BUDGET);
    w.sync()?;
    let tree = w.vm_tree()?;
    let expect_name = match m {
        Mutation::Create => "new.txt",
        Mutation::Mkdir => "newdir",
        Mutation::Rename => "g2.txt",
        Mutation::Modify | Mutation::Delete => "",
    };
    match m {
        Mutation::Create => {
            let copies = tree
                .contents()
                .filter(|(_, c)| c.as_slice() == b"created by mac")
                .count();
            if copies != 1
                || content_at(&tree, "d/new.txt").as_deref() != Some(&b"created by mac"[..])
            {
                problems.push(format!("created file present {copies} times"));
            }
        }
        Mutation::Mkdir => {
            if tree.nodes.get("d/newdir").map(|n| n.kind) != Some(VmKind::Dir) {
                problems.push("d/newdir missing".into());
            }
        }
        Mutation::Modify => {
            if content_at(&tree, "d/f.txt").as_deref() != Some(&b"modified by mac"[..]) {
                problems.push("modify did not land in place".into());
            }
        }
        Mutation::Rename => {
            if content_at(&tree, "d/g2.txt").as_deref() != Some(&b"g"[..])
                || tree.nodes.contains_key("d/g.txt")
            {
                problems.push("rename did not land exactly once".into());
            }
        }
        Mutation::Delete => {
            if tree.nodes.contains_key("d/g.txt") {
                problems.push("delete did not land".into());
            }
        }
    }
    if !expect_name.is_empty() {
        for p in tree.nodes.keys() {
            let name = p.rsplit('/').next().unwrap_or(p);
            if numbered_base(name).as_deref() == Some(expect_name) {
                problems.push(format!("duplicate from replay: {p}"));
            }
        }
    }
    final_checks(w, problems, false)
}

/// Targeted races (review §2(f)4). Names in the order they run.
pub const SPECIAL: &[&str] = &[
    "mac_write_races_agent_write",
    "rename_over_with_observed_temp",
    "mv_a_b_then_touch_a",
    "rm_f_then_mkdir_f",
    "dir_moved_during_snapshot",
    "finder_rm_rf_while_agent_writes",
    "intermediate_dir_swapped_for_symlink",
    "hardlinks",
    "mac_save_after_daemon_restart",
    "bind_mount_inside_root",
    "inotify_queue_overflow",
];

pub fn run_special<W: FuzzWorld>(name: &str, w: &mut W) -> Result<Verdict> {
    let skip = |why: &str| {
        Ok(Verdict {
            problems: Vec::new(),
            skipped: Some(why.to_string()),
        })
    };
    let mut problems = Vec::new();
    match name {
        "mac_write_races_agent_write" => {
            start(w, &[("d/f.txt", b"v1")], &["d"])?;
            w.sim().open("d/f.txt").map_err(|e| anyhow!("{e}"))?;
            w.vm().write("d/f.txt", b"agent final bytes")?;
            w.sim().save_atomic("d/f.txt", b"mac save").map_err(|e| anyhow!("{e}"))?;
            w.sync()?;
            let tree = w.vm_tree()?;
            if content_at(&tree, "d/f.txt").as_deref() != Some(&b"agent final bytes"[..]) {
                problems.push("agent bytes did not survive the racing Mac save".into());
            }
            if !tree.contents().any(|(_, c)| c == b"mac save") {
                problems.push("Mac bytes lost (expected a conflict copy)".into());
            }
            final_checks(w, problems, true)
        }
        "rename_over_with_observed_temp" => {
            start(w, &[("d/f.txt", b"v1")], &["d"])?;
            let before = id_at(w, "d/f.txt");
            w.vm().write("d/.f.txt.tmp", b"replaced")?;
            w.settle()?;
            w.sim().pump(QUIESCE_BUDGET);
            w.vm().rename("d/.f.txt.tmp", "d/f.txt")?;
            w.sync()?;
            if id_at(w, "d/f.txt") != before {
                problems.push("rename-over changed the destination's identifier".into());
            }
            match w.sim().open("d/f.txt") {
                Ok(b) if b == b"replaced" => {}
                other => problems.push(format!("after rename-over the Mac reads {other:?}")),
            }
            final_checks(w, problems, false)
        }
        "mv_a_b_then_touch_a" => {
            start(w, &[("d/a.txt", b"original a")], &["d"])?;
            let a_id = id_at(w, "d/a.txt");
            w.vm().rename("d/a.txt", "d/b.txt")?;
            w.vm().write("d/a.txt", b"")?;
            w.sync()?;
            if id_at(w, "d/b.txt") != a_id {
                problems.push("the moved file did not keep its identifier".into());
            }
            match w.sim().open("d/b.txt") {
                Ok(b) if b == b"original a" => {}
                other => problems.push(format!("d/b.txt reads {other:?}")),
            }
            final_checks(w, problems, false)
        }
        "rm_f_then_mkdir_f" => {
            start(w, &[("d/f", b"file")], &["d"])?;
            w.vm().remove_file("d/f")?;
            w.vm().mkdir("d/f")?;
            w.vm().write("d/f/inner", b"inside")?;
            w.sync()?;
            final_checks(w, problems, false)
        }
        "dir_moved_during_snapshot" => {
            let files: Vec<(String, Vec<u8>)> =
                (0..1500).map(|i| (format!("d/big/f{i:04}.txt"), format!("{i}").into_bytes())).collect();
            for (p, b) in &files {
                mkfile(w, p, b)?;
            }
            start(w, &[("d/other.txt", b"o")], &["d"])?;
            if !w.restart_engine_fresh()? {
                return skip("fresh engine restart unavailable here");
            }
            w.vm().rename("d/big", "d/moved")?;
            w.sync()?;
            final_checks(w, problems, false)
        }
        "finder_rm_rf_while_agent_writes" => {
            start(w, &[("d/sub/a.txt", b"a"), ("d/sub/b.txt", b"b")], &["d", "d/sub"])?;
            w.vm().write("d/sub/new.txt", b"agent wrote this meanwhile")?;
            w.sim().delete("d/sub").map_err(|e| anyhow!("{e}"))?;
            w.sync()?;
            let tree = w.vm_tree()?;
            if content_at(&tree, "d/sub/new.txt").as_deref() != Some(&b"agent wrote this meanwhile"[..]) {
                problems.push("rm -rf in Finder destroyed an agent file the Mac never saw".into());
            }
            final_checks(w, problems, false)
        }
        "intermediate_dir_swapped_for_symlink" => {
            start(w, &[("d/sub/f.txt", b"v1")], &["d", "d/sub"])?;
            w.sim().open("d/sub/f.txt").map_err(|e| anyhow!("{e}"))?;
            w.vm().rename("d/sub", "d/sub.old")?;
            let target = w.outside_target();
            w.vm().symlink(&target, "d/sub")?;
            let _ = w.sim().write("d/sub/f.txt", b"mac bytes");
            w.sync()?;
            let tree = w.vm_tree()?;
            if !tree.contents().any(|(_, c)| c == b"mac bytes") {
                problems.push("the Mac's write vanished (expected it under d/sub.old)".into());
            }
            final_checks(w, problems, false)
        }
        "hardlinks" => {
            start(w, &[("d/f.txt", b"v1")], &[])?;
            if w.vm().hard_link("d/f.txt", "d/h.txt").is_err() {
                return skip("hardlinks unsupported here");
            }
            w.settle()?;
            w.sim().browse("d").map_err(|e| anyhow!("{e}"))?;
            w.sim().pump(QUIESCE_BUDGET);
            if id_at(w, "d/f.txt") == id_at(w, "d/h.txt") {
                problems.push("two hardlinks share one identifier (review (a)3: one id per link)".into());
            }
            w.sim().write("d/h.txt", b"mac edit via link").map_err(|e| anyhow!("{e}"))?;
            w.sync()?;
            let tree = w.vm_tree()?;
            if content_at(&tree, "d/h.txt").as_deref() != Some(&b"mac edit via link"[..]) {
                problems.push("edit through a hardlink did not land".into());
            }
            final_checks(w, problems, false)
        }
        "mac_save_after_daemon_restart" => {
            // The agent writes, the Mac syncs and opens the file, the daemon restarts (crash, VM
            // reboot), then the user saves. Nothing touched the file on the VM in between, so the
            // save must land in place — a restart alone must not make it a conflict.
            start(w, &[("d/f.txt", b"agent v1")], &["d"])?;
            w.sim().open("d/f.txt").map_err(|e| anyhow!("{e}"))?;
            w.kill_unlatchd()?;
            w.sync()?;
            w.sim().save_atomic("d/f.txt", b"mac save after restart").map_err(|e| anyhow!("{e}"))?;
            w.sync()?;
            let tree = w.vm_tree()?;
            if content_at(&tree, "d/f.txt").as_deref() != Some(&b"mac save after restart"[..]) {
                problems.push("the save did not land in place after a daemon restart".into());
            }
            final_checks(w, problems, false)
        }
        "bind_mount_inside_root" => skip("bind mounts need CAP_SYS_ADMIN in the daemon's mount namespace; not possible unprivileged here"),
        "inotify_queue_overflow" => {
            // `UNLATCH_FAULT=overflow_after_events:2`: the daemon's inotify reader loses every
            // event after the second and reports IN_Q_OVERFLOW (D14). Armed before the tree
            // exists (a restart after it would bump recent files, see run_case).
            if !w.arm_daemon_fault("overflow_after_events:2")? {
                return skip("daemon fault injection unavailable here");
            }
            start(
                w,
                &[("d/a.txt", b"a"), ("d/b.txt", b"b"), ("d/c.txt", b"c"), ("d/sub/k", b"k")],
                &["d", "d/sub"],
            )?;
            let a = id_at(w, "d/a.txt");
            let c = id_at(w, "d/c.txt");
            w.vm().write("d/new.txt", b"new after overflow")?;
            w.vm().rename("d/a.txt", "d/sub/a2.txt")?;
            w.vm().remove_file("d/b.txt")?;
            w.vm().write("d/c.txt", b"c rewritten, longer")?;
            w.vm().mkdir("d/fresh")?;
            w.vm().write("d/fresh/inner.txt", b"inner")?;
            w.sync()?;
            if id_at(w, "d/sub/a2.txt") != a {
                problems.push("a move whose events were lost changed the item's identifier".into());
            }
            if id_at(w, "d/c.txt") != c {
                problems.push("a rewrite whose events were lost changed the identifier".into());
            }
            match w.sim().open("d/c.txt") {
                Ok(b) if b == b"c rewritten, longer" => {}
                other => problems.push(format!("after the overflow the Mac reads d/c.txt as {other:?}")),
            }
            // Watching goes on after the overflow.
            w.vm().write("d/later.txt", b"later")?;
            w.sync()?;
            final_checks(w, problems, false)
        }
        other => Err(anyhow!("unknown special scenario {other:?}")),
    }
}

fn id_at<W: FuzzWorld>(w: &mut W, path: &str) -> Option<unlatch_proto::ItemId> {
    let k = w.sim().disk().resolve(path)?;
    w.sim().disk().get(k)?.id
}
