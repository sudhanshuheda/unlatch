//! `unlatch-bench fpsim engine-host`: runs one `Engine` + `serve_ipc` in its own process, standing
//! in for the macOS engine agent. Engine-side fault injection (`UNLATCH_FAULT=
//! die_before_ipc_reply:<kind>`) is read once per process, so it needs a process of its own.
//!
//! Line protocol (the parent is `super::e2e::EngineCtl`):
//! * stdout: `READY`, then `EVENT ws` / `EVENT resolved` / `EVENT reimport <id>` /
//!   `EVENT needsuser <reason>`, and one `OK` or `ERR <msg>` per command;
//! * stdin: `barrier <ms>`, `idle <ms>`, `live <ms>`, `drop`, `network`, `quit`.
//!
//! EOF on stdin shuts the engine down.

use crate::cli::Args;
use anyhow::{anyhow, Result};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use unlatch_core::{Engine, EngineConfig, EngineEvent, EventHandler, Transport};
use unlatch_proto::ItemId;

/// Encode an engine event as one stdout line (`None` = not forwarded).
pub fn event_line(ev: &EngineEvent) -> Option<String> {
    match ev {
        EngineEvent::WorkingSetChanged { .. } => Some("EVENT ws".into()),
        EngineEvent::ErrorResolved => Some("EVENT resolved".into()),
        EngineEvent::Reimport { below } => Some(format!("EVENT reimport {}", below.0)),
        EngineEvent::NeedsUser { reason, .. } => {
            Some(format!("EVENT needsuser {}", reason.replace('\n', " ")))
        }
        EngineEvent::ReplicaChanged { .. } | EngineEvent::StatusChanged(_) => None,
    }
}

/// Inverse of [`event_line`].
pub fn parse_event_line(line: &str) -> Option<EngineEvent> {
    let rest = line.strip_prefix("EVENT ")?;
    let (kind, arg) = rest.split_once(' ').unwrap_or((rest, ""));
    match kind {
        "ws" => Some(EngineEvent::WorkingSetChanged { anchor: Vec::new() }),
        "resolved" => Some(EngineEvent::ErrorResolved),
        "reimport" => arg
            .parse()
            .ok()
            .map(|n| EngineEvent::Reimport { below: ItemId(n) }),
        "needsuser" => Some(EngineEvent::NeedsUser {
            reason: arg.to_string(),
            url: None,
        }),
        _ => None,
    }
}

pub fn main(args: &[String]) -> Result<i32> {
    let mut a = Args::new(args);
    let socket = PathBuf::from(
        a.opt("socket")?
            .ok_or_else(|| anyhow!("--socket required"))?,
    );
    let root = a.opt("root")?.ok_or_else(|| anyhow!("--root required"))?;
    let state = PathBuf::from(a.opt("state")?.ok_or_else(|| anyhow!("--state required"))?);
    let argv = a.opts("argv")?;
    let name = a.opt("name")?.unwrap_or_else(|| "fpsim".into());
    a.finish()?;
    if argv.is_empty() {
        return Err(anyhow!("--argv required (the unlatchd command)"));
    }
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let out_ev = out.clone();
    let handler: EventHandler = Arc::new(move |ev: EngineEvent| {
        if let Some(line) = event_line(&ev) {
            if let Ok(mut o) = out_ev.lock() {
                let _ = writeln!(o, "{line}");
                let _ = o.flush();
            }
        }
    });
    let mut cfg = EngineConfig::new(
        &name,
        Transport::Command {
            argv,
            env: Vec::new(),
        },
        &root,
        state,
        "fpsim",
    );
    super::e2e::apply_list_timeout(&mut cfg);
    let engine = Engine::start(cfg, Some(handler)).map_err(|e| anyhow!("engine start: {e}"))?;
    let _ = std::fs::remove_file(&socket);
    let _server = engine
        .serve_ipc(&socket)
        .map_err(|e| anyhow!("serve_ipc: {e}"))?;
    let say = |line: &str| {
        if let Ok(mut o) = out.lock() {
            let _ = writeln!(o, "{line}");
            let _ = o.flush();
        }
    };
    say("READY");
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        let (cmd, arg) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let ms = Duration::from_millis(arg.trim().parse().unwrap_or(10_000));
        let r = match cmd {
            "barrier" => engine.server_barrier(ms),
            "idle" => engine.wait_idle(ms),
            "live" => engine.wait_live(ms),
            "drop" => {
                engine.drop_connection();
                Ok(())
            }
            "network" => {
                engine.network_changed();
                Ok(())
            }
            "interactive" => engine.connect_interactive(),
            "quit" => break,
            other => Err(unlatch_proto::ProtoError::new(
                unlatch_proto::ErrorCode::Protocol,
                format!("unknown command {other}"),
            )),
        };
        match r {
            Ok(()) => say("OK"),
            Err(e) => say(&format!("ERR {e}")),
        }
    }
    engine.shutdown();
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_lines_roundtrip() {
        for ev in [
            EngineEvent::ErrorResolved,
            EngineEvent::Reimport { below: ItemId(7) },
            EngineEvent::NeedsUser {
                reason: "host key".into(),
                url: None,
            },
        ] {
            let line = event_line(&ev).expect("forwarded");
            assert_eq!(parse_event_line(&line), Some(ev));
        }
        assert!(matches!(
            parse_event_line("EVENT ws"),
            Some(EngineEvent::WorkingSetChanged { .. })
        ));
        assert_eq!(parse_event_line("OK"), None);
    }
}
