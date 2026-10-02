//! Probe (ignored by default; slow): Mac saves racing an agent's appends against the real
//! daemon (the review's `reply_version_vs_bytes_race`, data-safety group, extended). Every
//! round the agent appends a unique tag 0–2 ms after the Mac's Write was sent; then:
//!
//! * a `Read` at the reply's version must never return bytes other than the Mac's (a version
//!   that names the agent's bytes is "a mismatch not reported as a race");
//! * the Mac's next save, based on the reply's version, must leave the agent's tag somewhere
//!   on the VM (in place or in a conflict copy) — else "a silent loss".
//!
//! `cargo test -p unlatchd --test race_probe -- --ignored --nocapture`; `PROBE_ROUNDS` (120)
//! and `PROBE_TMP` (a directory to create the root in, e.g. on tmpfs). Run it pinned to one
//! CPU (`taskset -c 0`, daemon included) to widen the windows. `readonly_open_vs_reply`: the
//! same with an agent that only reads (no race, no conflict copy, never held back).

mod common;
use common::*;
use std::time::Duration;
use unlatch_proto::wire::Response;
use unlatch_proto::{ErrorCode, ItemId};

fn files(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().unwrap().is_file())
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect()
}

fn has(b: &[u8], needle: &[u8]) -> bool {
    b.windows(needle.len()).any(|w| w == needle)
}

#[test]
#[ignore]
fn reply_version_vs_bytes_race() {
    use std::io::Write;
    let rounds: u64 = std::env::var("PROBE_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let (root, state) = match std::env::var_os("PROBE_TMP") {
        Some(d) => (
            tempfile::tempdir_in(&d).unwrap(),
            tempfile::tempdir_in(&d).unwrap(),
        ),
        None => (tmp(), tmp()),
    };
    let p = root.path().join("f.txt");
    std::fs::write(&p, b"v0\n").unwrap();
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let (mut conflicts, mut clean, mut reported, mut unreported, mut lost) = (0, 0, 0, 0, 0);
    for i in 0..rounds {
        c.ping();
        let f = c.find("f.txt").unwrap();
        let mac = format!("mac {i}\n").into_bytes();
        let tag = format!("AGENT{i}X\n").into_bytes();
        let delay = (i % 40) * 50; // 0..2 ms in 50 us steps
        let (pp, t2) = (p.clone(), tag.clone());
        let agent = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_micros(delay));
            let mut h = std::fs::OpenOptions::new().append(true).open(&pp).unwrap();
            h.write_all(&t2).unwrap();
        });
        let r = c.write(
            op(1000 + i),
            ItemId::ROOT,
            "f.txt",
            Some(f.id),
            Some(f.version.content),
            &mac,
            false,
        );
        agent.join().unwrap();
        let (entry, cc) = match r {
            Ok(Response::Written {
                entry,
                conflict_copy,
            }) => (entry, conflict_copy),
            o => panic!("{o:?}"),
        };
        if cc.is_some() {
            conflicts += 1;
        } else {
            std::thread::sleep(Duration::from_millis(150));
            c.ping();
            c.pump(Duration::from_millis(50));
            match c.read(entry.id, Some(entry.version.content)) {
                Ok((bytes, _)) if bytes == mac => clean += 1,
                Ok((bytes, _)) => {
                    unreported += 1;
                    eprintln!(
                        "round {i}: version {} carries {:?}, the Mac holds {:?}",
                        entry.version.content,
                        String::from_utf8_lossy(&bytes),
                        String::from_utf8_lossy(&mac)
                    );
                }
                Err(e) if e.code == ErrorCode::VersionMismatch => reported += 1,
                Err(e) => panic!("read: {e:?}"),
            }
            // The Mac's next save, based on the version it was told it holds.
            let r2 = c.write(
                op(90_000 + i),
                ItemId::ROOT,
                "f.txt",
                Some(entry.id),
                Some(entry.version.content),
                format!("mac next {i}\n").as_bytes(),
                false,
            );
            assert!(matches!(r2, Ok(Response::Written { .. })), "{r2:?}");
        }
        let fs = files(root.path());
        if !fs.iter().any(|(_, b)| has(b, &tag)) {
            lost += 1;
            eprintln!(
                "round {i} (delay {delay}us): {} lost; files {:?}",
                String::from_utf8_lossy(&tag),
                fs.iter()
                    .map(|(n, b)| (n.clone(), String::from_utf8_lossy(b).into_owned()))
                    .collect::<Vec<_>>()
            );
        }
        for (n, _) in &fs {
            if n != "f.txt" {
                let _ = std::fs::remove_file(root.path().join(n));
            }
        }
    }
    eprintln!(
        "PROBE rounds={rounds} conflict_at_write={conflicts} clean={clean} \
         reported={reported} unreported_mismatch={unreported} silent_loss={lost}"
    );
    assert_eq!((unreported, lost), (0, 0));
}

/// The same race with an agent that only reads (an editor's auto-reload, an LSP, an indexer
/// opening the file the moment it changes), 0–2 ms after the Mac's Write was sent. A reader is
/// no race: no conflict copy at the Write, `Read` at the reply's version returns the Mac's
/// bytes (not "reported"), and the Mac's next save based on it gets no conflict copy. Prints
/// how long the reader's open + read took (held back by a lease that breaks on any open).
#[test]
#[ignore]
fn readonly_open_vs_reply() {
    let rounds: u64 = std::env::var("PROBE_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let (root, state) = match std::env::var_os("PROBE_TMP") {
        Some(d) => (
            tempfile::tempdir_in(&d).unwrap(),
            tempfile::tempdir_in(&d).unwrap(),
        ),
        None => (tmp(), tmp()),
    };
    let p = root.path().join("f.txt");
    std::fs::write(&p, b"v0\n").unwrap();
    let mut c = Client::spawn(root.path(), state.path(), Opts::default());
    c.wait_snapshot();
    let (mut conflicts, mut clean, mut reported, mut unreported, mut next_cc) = (0, 0, 0, 0, 0);
    let mut read_us = Vec::new();
    for i in 0..rounds {
        c.ping();
        let f = c.find("f.txt").unwrap();
        let mac = format!("mac {i}\n").into_bytes();
        let delay = (i % 40) * 50; // 0..2 ms in 50 us steps
        let pp = p.clone();
        let agent = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_micros(delay));
            let t = std::time::Instant::now();
            let _ = std::fs::read(&pp).unwrap();
            t.elapsed().as_micros() as u64
        });
        let r = c.write(
            op(1000 + i),
            ItemId::ROOT,
            "f.txt",
            Some(f.id),
            Some(f.version.content),
            &mac,
            false,
        );
        read_us.push(agent.join().unwrap());
        let (entry, cc) = match r {
            Ok(Response::Written {
                entry,
                conflict_copy,
            }) => (entry, conflict_copy),
            o => panic!("{o:?}"),
        };
        if cc.is_some() {
            conflicts += 1;
        } else {
            std::thread::sleep(Duration::from_millis(150));
            c.ping();
            c.pump(Duration::from_millis(50));
            match c.read(entry.id, Some(entry.version.content)) {
                Ok((bytes, _)) if bytes == mac => clean += 1,
                Ok((bytes, _)) => {
                    unreported += 1;
                    eprintln!(
                        "round {i}: version {} carries {:?}, the Mac holds {:?}",
                        entry.version.content,
                        String::from_utf8_lossy(&bytes),
                        String::from_utf8_lossy(&mac)
                    );
                }
                Err(e) if e.code == ErrorCode::VersionMismatch => {
                    reported += 1;
                    eprintln!("round {i} (delay {delay}us): a reader was reported as a race");
                }
                Err(e) => panic!("read: {e:?}"),
            }
            let r2 = c.write(
                op(90_000 + i),
                ItemId::ROOT,
                "f.txt",
                Some(entry.id),
                Some(entry.version.content),
                format!("mac next {i}\n").as_bytes(),
                false,
            );
            match r2 {
                Ok(Response::Written {
                    conflict_copy: Some(_),
                    ..
                }) => {
                    next_cc += 1;
                    eprintln!("round {i} (delay {delay}us): the next save became a conflict copy");
                }
                Ok(Response::Written { .. }) => {}
                o => panic!("{o:?}"),
            }
        }
        for (n, _) in &files(root.path()) {
            if n != "f.txt" {
                let _ = std::fs::remove_file(root.path().join(n));
            }
        }
    }
    read_us.sort_unstable();
    eprintln!(
        "PROBE-RO rounds={rounds} conflict_at_write={conflicts} clean={clean} \
         reported={reported} unreported_mismatch={unreported} next_save_conflict_copy={next_cc} \
         reader_us_p50={} reader_us_max={}",
        read_us[read_us.len() / 2],
        read_us[read_us.len() - 1]
    );
    assert_eq!((conflicts, reported, unreported, next_cc), (0, 0, 0, 0));
}
