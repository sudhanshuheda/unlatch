//! The correctness fuzz end to end (real engine + unlatchd + a real root directory), including
//! fault injection at both hops, the crash/replay matrix and the targeted races. Runs by
//! default; it needs a built `unlatchd` — `$UNLATCHD_BIN`, or `target/<profile>/unlatchd` next to this
//! test binary (`cargo test --workspace` builds it) — and fails loudly without one:
//!
//! ```text
//! cargo build -p unlatchd -p unlatch-bench
//! cargo test -p unlatch-bench --test fuzz_e2e
//! ```

use unlatch_bench::fpsim::e2e::E2eConfig;
use unlatch_bench::fuzz::{fuzz_seed, run_matrix, run_specials, Target};

fn target() -> Target {
    // Like `unlatch-bench fuzz`: Offline after 5 s while the fuzzer cuts the link on purpose.
    if std::env::var_os("UNLATCH_E2E_LIST_TIMEOUT_MS").is_none() {
        std::env::set_var("UNLATCH_E2E_LIST_TIMEOUT_MS", "5000");
    }
    Target::Real(E2eConfig::discover(None, None, false).unwrap_or_else(|e| {
        panic!("{e:#}: build it first (cargo build -p unlatchd) or set UNLATCHD_BIN")
    }))
}

#[test]
fn random_interleavings_with_faults() {
    let t = target();
    for seed in 1..=5 {
        if let Some(f) = fuzz_seed(&t, seed, 150, 30, false).expect("run") {
            panic!(
                "seed {seed}: {:#?}\nminimal ({} ops): {:#?}\nminimal problems: {:#?}",
                f.first.problems,
                f.minimal.len(),
                f.minimal,
                f.minimal_problems
            );
        }
    }
}

#[test]
fn crash_replay_matrix_both_hops() {
    let t = target();
    let mut failures = Vec::new();
    for (name, v) in run_matrix(&t) {
        match v {
            Ok(v) if v.skipped.is_some() => eprintln!("{name}: skipped: {:?}", v.skipped),
            Ok(v) if v.problems.is_empty() => {}
            Ok(v) => failures.push(format!("{name}: {:?}", v.problems)),
            Err(e) => failures.push(format!("{name}: harness error {e:#}")),
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn targeted_races() {
    let t = target();
    let mut failures = Vec::new();
    for (name, v) in run_specials(&t) {
        match v {
            Ok(v) if v.skipped.is_some() => eprintln!("{name}: skipped: {:?}", v.skipped),
            Ok(v) if v.problems.is_empty() => {}
            Ok(v) => failures.push(format!("{name}: {:?}", v.problems)),
            Err(e) => failures.push(format!("{name}: harness error {e:#}")),
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Minimal sequences of bugs the 200-seed sweep found (each fixed; see the commit that added
/// it). Replayed verbatim: picks resolve against the same tree every time.
const REGRESSIONS: &[(&str, &str)] = &[
    // unlatchd: a Mac-created lazy-named dir (node_modules) was never scanned or watched, so an
    // agent's file inside it never reached the Mac.
    (
        "seed 1: Mkdir of a lazy name",
        r#"[{"Mkdir":{"dir":0,"name":"src"}},{"Mkdir":{"dir":0,"name":"docs"}},{"RmRf":{"dir":795125249}},{"LoseReply":{"kind":2}},{"Mkdir":{"dir":1783463726,"name":"deep"}},{"Overwrite":{"file":1095675499,"n":1000034}},{"Overwrite":{"file":119992291,"n":1000040}},{"Mkdir":{"dir":2509244144,"name":"tmp"}},{"MacSave":{"file":1351004289,"n":1000042}},{"Mkdir":{"dir":3326532262,"name":"docs"}},"Quiesce",{"LoseReply":{"kind":3}},{"MacSave":{"file":3410066697,"n":1000057}},{"MacMkdir":{"dir":3360612293,"name":"node_modules"}},{"SymlinkIn":{"dir":3310408180,"name":"x","target":"docs/"}},{"Overwrite":{"file":3320438109,"n":1000060}}]"#,
    ),
    // harness: a Mac rename not yet acknowledged was tracked under its old name, so deleting
    // the renamed folder afterwards looked like losing the agent file inside it.
    (
        "seed 40: rename then delete on the Mac",
        r#"[{"Mkdir":{"dir":0,"name":"src"}},{"Mkdir":{"dir":0,"name":"docs"}},{"Write":{"dir":3825281183,"name":".env","n":40000126}},"Quiesce",{"MacRename":{"item":879692545,"name":"café.md"}},{"MacDelete":{"item":1508864688}},{"MacOpen":{"file":877751742}}]"#,
    ),
    // unlatchd: the verify walk after a restart found docs moved (swapped for a symlink) and
    // never re-watched it; the file written into docs.old afterwards never reached the Mac.
    (
        "seed 58: dir moved while the daemon restarted",
        r#"[{"Mkdir":{"dir":3201968188,"name":"docs"}},"KillUnlatchd",{"SwapDirForSymlink":{"dir":715183935}},{"Write":{"dir":3897519253,"name":"notes","n":58000229}}]"#,
    ),
    // harness: an edit whose read failed (lost fetch reply) was saved anyway.
    (
        "seed 67: MacEdit after a lost fetch",
        r#"[{"MacCreate":{"dir":3632748683,"name":"readme","n":67000208}},{"LoseReply":{"kind":3}},{"MacEvict":{"file":1502587259}},{"AtomicReplace":{"file":209995530,"n":67000229}},{"MacEdit":{"file":1758319795,"n":67000232}}]"#,
    ),
    // unlatchd: a file the agent wrote shortly before a daemon crash was bumped by the startup racy rule however long the daemon kept watching after it; the Mac's next save conflicted with itself (seed 475).
    (
        "seed 475: Mac save after a crash that followed an agent write",
        r#"[{"Write":{"dir":107248169,"name":"cafe\u0301.md","n":475001431}},"Quiesce",{"MacOpen":{"file":3399943621}},"KillUnlatchd",{"MacSave":{"file":43941736,"n":475001434}}]"#,
    ),
    // harness: an edit the VM refused for good (read-only hard link) stays on the Mac with an error badge; the checker reported its VM file missing on the Mac.
    (
        "seed 267: errored upload accounts for its name",
        r#"[{"Mkdir":{"dir":0,"name":"src"}},{"Mkdir":{"dir":0,"name":"docs"}},{"Write":{"dir":2066936886,"name":"main.rs","n":267000802}},{"Write":{"dir":578449186,"name":"caf\u00e9.md","n":267000803}},{"Write":{"dir":3685721957,"name":"data.json","n":267000805}},{"Write":{"dir":2877680300,"name":".env","n":267000806}},{"Write":{"dir":837662232,"name":"notes","n":267000807}},"Quiesce",{"Mkdir":{"dir":1899790885,"name":"docs"}},{"MacDragIn":{"dir":456762705,"n":267000810,"count":2}},{"Overwrite":{"file":1883820742,"n":267000815}},"Quiesce",{"Hardlink":{"file":3963964755,"dir":799991804,"name":"README"}},{"MacMkdir":{"dir":1947100945,"name":"node_modules"}},{"MacCreate":{"dir":2369250649,"name":"A.txt","n":267000816}},{"Write":{"dir":1662265593,"name":"A.txt","n":267000818}},{"MacOpen":{"file":172487447}},{"MacChmod":{"file":834667755,"exec":true}},{"Chmod":{"file":1832958780,"mode":292}},{"MacEdit":{"file":3550690576,"n":267000822}}]"#,
    ),
    // harness: link(2) changes the source inode's ctime, a content change by the version rule; the Mac's save of the source raced it.
    (
        "seed 390: Mac save racing an agent hardlink of that file",
        r#"[{"Mkdir":{"dir":0,"name":"src"}},{"Mkdir":{"dir":0,"name":"docs"}},{"Write":{"dir":2819936387,"name":"notes","n":390001171}},{"Write":{"dir":3333161025,"name":"a.txt","n":390001172}},{"Write":{"dir":2952372133,"name":"data.json","n":390001173}},{"Write":{"dir":3969744588,"name":"main.rs","n":390001174}},{"Write":{"dir":2549986557,"name":"cafe\u0301.md","n":390001175}},{"Write":{"dir":2554464697,"name":".env","n":390001176}},{"Write":{"dir":152704463,"name":"README","n":390001178}},{"Rm":{"file":1407777674}},"Quiesce",{"MacEdit":{"file":551863370,"n":390001181}},{"Rm":{"file":3140109645}},{"SwapDirForSymlink":{"dir":1046926749}},{"Overwrite":{"file":1829787694,"n":390001182}},{"RmRf":{"dir":2861066435}},{"AtomicReplace":{"file":400618888,"n":390001183}},{"Hardlink":{"file":3906865019,"dir":3807317235,"name":"readme"}},{"MacEdit":{"file":3603698184,"n":390001184}},{"MacSave":{"file":2147287894,"n":390001185}},{"MacSave":{"file":988314997,"n":390001186}},{"MacEdit":{"file":1754168038,"n":390001190}},{"MacDelete":{"item":1991015555}},{"MacBrowse":{"dir":1677686407}},{"MacEdit":{"file":3854244434,"n":390001191}}]"#,
    ),
    // engine: a delete retried after a lost reply recomputed seen_seq from anchors consumed since, and deleted the agent file its first attempt kept.
    (
        "seed 186: retried folder delete keeps its first seen_seq",
        r#"[{"Mkdir":{"dir":0,"name":"src"}},{"Mkdir":{"dir":0,"name":"docs"}},{"Write":{"dir":3498686974,"name":"data.json","n":186000559}},{"Write":{"dir":3403624839,"name":"notes","n":186000560}},{"Write":{"dir":1126098624,"name":"A.txt","n":186000561}},{"Write":{"dir":3537708700,"name":"cafe\u0301.md","n":186000562}},{"MacCreate":{"dir":3140839531,"name":"README","n":186000567}},"Quiesce",{"Write":{"dir":3292674013,"name":"README","n":186000568}},{"Hardlink":{"file":4224061412,"dir":595486180,"name":".env"}},{"AtomicReplace":{"file":3559581297,"n":186000569}},{"MacBrowse":{"dir":1887665286}},{"SymlinkOut":{"dir":1512714514,"name":"x"}},{"Overwrite":{"file":591376741,"n":186000570}},{"MacSave":{"file":925256063,"n":186000571}},{"Overwrite":{"file":162558274,"n":186000572}},{"MacSave":{"file":2090303837,"n":186000573}},{"Write":{"dir":497415946,"name":"main.rs","n":186000574}},{"MacOpen":{"file":3181589290}},"Quiesce",{"MacTag":{"item":1526069784}},{"Write":{"dir":2125805120,"name":"a.txt","n":186000575}},"Quiesce",{"MacBrowse":{"dir":1142916171}},{"MacEvict":{"file":175456475}},{"Rename":{"src":1117433276,"dir":1340396199,"name":"data.json"}},{"RmRf":{"dir":3503947708}},"Quiesce",{"Write":{"dir":1753826225,"name":"readme","n":186000576}},{"Overwrite":{"file":3162201772,"n":186000577}},{"AtomicReplace":{"file":424223495,"n":186000578}},{"MacMove":{"item":912563407,"dir":1094695407}},{"Rm":{"file":2543340011}},{"AdvanceTime":{"secs":30}},"GoOnline",{"MacDragIn":{"dir":2164364551,"n":186000579,"count":2}},{"Overwrite":{"file":2447313553,"n":186000580}},{"MacSave":{"file":2608129640,"n":186000581}},{"Hardlink":{"file":2847327996,"dir":2532226127,"name":"cafe\u0301.md"}},{"LoseReply":{"kind":2}},{"Overwrite":{"file":2647391946,"n":186000582}},{"MacCreate":{"dir":3189940390,"name":".env","n":186000583}},{"Append":{"file":1415256925,"n":186000584}},{"MacDelete":{"item":3960674364}},{"Mkdir":{"dir":517048873,"name":"src"}},{"AdvanceTime":{"secs":120}},{"MacEdit":{"file":2387526025,"n":186000585}},{"MacBrowse":{"dir":3575946798}},{"Rename":{"src":1716988601,"dir":2024582605,"name":"x"}},{"MacEdit":{"file":1811917886,"n":186000586}},{"Rename":{"src":2684360770,"dir":3473125171,"name":"A.txt"}},{"MacSave":{"file":2215352135,"n":186000587}},{"RmRf":{"dir":85875285}},"GoOnline",{"MacSave":{"file":3148135991,"n":186000588}},{"AtomicReplace":{"file":1580164691,"n":186000589}},{"MacMkdir":{"dir":2374088464,"name":"deep"}},{"MacDelete":{"item":882103599}},{"AdvanceTime":{"secs":6}},{"Rename":{"src":1051064695,"dir":570538777,"name":".env"}},{"Write":{"dir":1421655831,"name":"x","n":186000590}},{"MacDelete":{"item":3691745172}},{"SymlinkIn":{"dir":1276173280,"name":"b.txt","target":"a.txt"}},{"Write":{"dir":4083260421,"name":"a.txt","n":186000591}},{"MacSave":{"file":2484334883,"n":186000592}},"DropConnection",{"AdvanceTime":{"secs":30}},{"MacDragIn":{"dir":4224409264,"n":186000593,"count":2}},{"MacEdit":{"file":2753675457,"n":186000594}},"GoOnline",{"Overwrite":{"file":1477152729,"n":186000595}},{"LoseReply":{"kind":0}}]"#,
    ),
];

#[test]
fn sweep_regressions() {
    let t = target();
    let mut failures = Vec::new();
    for (name, json) in REGRESSIONS {
        let ops: Vec<unlatch_bench::fuzz::ops::Op> = serde_json::from_str(json).expect(name);
        match unlatch_bench::fuzz::run_once(&t, &ops, false) {
            Ok(o) if o.failed_at.is_none() => {}
            Ok(o) => failures.push(format!("{name}: {:?}", o.problems)),
            Err(e) => failures.push(format!("{name}: harness error {e:#}")),
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
