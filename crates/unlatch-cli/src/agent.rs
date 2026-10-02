//! `unlatch agent`: one engine per configured domain, each serving IPC on its socket.

use crate::config::AgentConfig;
use crate::signals::Terminator;
use anyhow::{bail, Context};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};
use unlatch_core::{Engine, EngineEvent, EventHandler};
use unlatch_proto::ipc::ConnState;

/// Remove a socket file left by a dead agent; refuse if a live one still answers.
pub fn clear_stale_socket(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("checking {}", path.display())),
        Ok(_) => {}
    }
    match UnixStream::connect(path) {
        Ok(_) => bail!(
            "{} is served by a running engine (another `unlatch agent`?)",
            path.display()
        ),
        Err(e) if matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound) => {
            std::fs::remove_file(path)
                .with_context(|| format!("removing stale socket {}", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("probing {}", path.display())),
    }
}

fn private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Logs connection-state transitions (not every status tick).
fn event_logger(domain: String) -> EventHandler {
    let last: Mutex<Option<std::mem::Discriminant<ConnState>>> = Mutex::new(None);
    Arc::new(move |ev| match ev {
        EngineEvent::StatusChanged(st) => {
            let d = std::mem::discriminant(&st.state);
            let mut g = last.lock().unwrap_or_else(|p| p.into_inner());
            if *g != Some(d) {
                *g = Some(d);
                match &st.state {
                    ConnState::Offline { error, .. } => warn!(%domain, %error, "offline"),
                    ConnState::NeedsUser { reason, .. } => warn!(%domain, %reason, "needs you"),
                    ConnState::Paused { reason } => {
                        warn!(%domain, %reason, "paused (mass-deletion guard)")
                    }
                    other => info!(%domain, state = ?other, "state"),
                }
            }
        }
        EngineEvent::NeedsUser { reason, url } => {
            warn!(%domain, %reason, url = url.as_deref().unwrap_or(""), "action needed (see `unlatch doctor`)")
        }
        EngineEvent::Reimport { below } => info!(%domain, %below, "reimport requested"),
        _ => {}
    })
}

pub fn run_agent(config_path: &Path, only: &[String]) -> anyhow::Result<()> {
    let cfg = AgentConfig::load(config_path)?;
    for name in only {
        cfg.domain(Some(name))?;
    }
    let term = Terminator::install().context("installing signal handlers")?;
    let default_client = cfg.client_name.clone().unwrap_or_else(crate::machine_name);
    let mut running: Vec<(String, Engine, unlatch_core::ipc::IpcServerHandle)> = Vec::new();
    for d in cfg
        .domains
        .iter()
        .filter(|d| only.is_empty() || only.contains(&d.name))
    {
        let client = unlatch_core::transport::sanitize_client_name(
            d.client_name.as_deref().unwrap_or(&default_client),
        );
        let ecfg = d.engine_config(&client);
        private_dir(&ecfg.state_dir)?;
        let socket = d.socket_path();
        if let Some(parent) = socket.parent() {
            private_dir(parent)?;
        }
        clear_stale_socket(&socket)?;
        let engine = Engine::start(ecfg, Some(event_logger(d.name.clone())))
            .map_err(|e| anyhow::anyhow!("starting engine for {:?}: {e}", d.name))?;
        let server = engine.serve_ipc(&socket).map_err(|e| {
            anyhow::anyhow!("serving IPC for {:?} on {}: {e}", d.name, socket.display())
        })?;
        // serve_ipc promises 0600; enforce it in case the umask interfered.
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
        info!(domain = %d.name, socket = %socket.display(), "serving");
        running.push((d.name.clone(), engine, server));
    }
    if running.is_empty() {
        bail!("no domains selected");
    }
    term.wait_until(|| false);
    info!("shutting down");
    for (name, engine, server) in running {
        drop(server);
        engine.shutdown();
        info!(domain = %name, "stopped");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn stale_socket_is_removed_live_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("e.sock");
        clear_stale_socket(&p).unwrap();
        {
            let _l = UnixListener::bind(&p).unwrap();
            assert!(
                clear_stale_socket(&p).is_err(),
                "a live listener must not be removed"
            );
        }
        // Listener dropped: the file remains but nobody accepts.
        assert!(p.exists());
        clear_stale_socket(&p).unwrap();
        assert!(!p.exists());
    }
}
