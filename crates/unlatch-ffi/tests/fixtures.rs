//! JSON fixtures for the Swift `Codable` mirrors (mac/UnlatchShared/Fixtures). The Swift test
//! target decodes and re-encodes every file, so a drift between the Rust protocol types and the
//! Swift structs fails CI on the macOS runner.
//!
//! `UNLATCH_UPDATE_FIXTURES=1 cargo test -p unlatch-ffi --test fixtures` rewrites them; otherwise
//! this test fails when they are stale.

mod common;

use common::*;
use std::collections::BTreeMap;
use std::path::PathBuf;
use unlatch::{event_json, FfiConfig, FfiPrefetch, FfiTransport, FfiUnlatchdBinary};
use unlatch_core::EngineEvent;
use unlatch_proto::ipc::{ConnState, IpcFrame, IpcResponse};
use unlatch_proto::ItemId;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mac/UnlatchShared/Fixtures")
}

fn pretty<T: serde::Serialize>(v: &T) -> String {
    let mut s = serde_json::to_string_pretty(v).expect("serializable");
    s.push('\n');
    s
}

fn configs() -> Vec<(&'static str, FfiConfig)> {
    let minimal = FfiConfig {
        name: "devbox".into(),
        transport: FfiTransport::Ssh {
            destination: "devbox".into(),
            port: None,
            identity: None,
            extra_args: vec![],
        },
        remote_root: "~/code".into(),
        state_dir: "/Users/me/Library/Group Containers/TEAMID.dev.unlatch.unlatch/domains/devbox"
            .into(),
        client_name: "Sam's MacBook".into(),
        cache_dir: None,
        temp_dir: None,
        unlatchd_command: None,
        remote_install_dir: None,
        unlatchd_upload: vec![],
        cache_budget: None,
        prefetch: None,
        default_lazy_names: None,
        ssh_env: BTreeMap::new(),
        askpass: None,
        list_timeout_ms: None,
        expose_exec: None,
        mass_delete_frac: None,
        mass_delete_abs: None,
        mass_delete_min: None,
    };
    let full = FfiConfig {
        transport: FfiTransport::Ssh {
            destination: "me@10.0.0.5".into(),
            port: Some(2222),
            identity: Some("/Users/me/.ssh/id_ed25519".into()),
            extra_args: vec!["-o".into(), "ProxyJump=bastion".into()],
        },
        cache_dir: Some(
            "/Users/me/Library/Group Containers/TEAMID.dev.unlatch.unlatch/cache/devbox".into(),
        ),
        temp_dir: Some(
            "/Users/me/Library/Group Containers/TEAMID.dev.unlatch.unlatch/tmp/devbox".into(),
        ),
        unlatchd_command: Some("/opt/unlatch/unlatchd".into()),
        unlatchd_upload: vec![FfiUnlatchdBinary {
            arch: "x86_64".into(),
            path: "/Applications/Unlatch.app/Contents/Resources/unlatchd/unlatchd-x86_64".into(),
            sha256_hex: "0123456789abcdef".repeat(4),
        }],
        cache_budget: Some(5 << 30),
        prefetch: Some(FfiPrefetch {
            max_file: Some(262_144),
            per_container: Some(8 << 20),
            bytes_per_min: Some(64 << 20),
            burst: Some(16 << 20),
        }),
        default_lazy_names: Some(vec!["node_modules".into(), ".git".into()]),
        ssh_env: BTreeMap::from([
            (
                "PATH".to_string(),
                "/opt/homebrew/bin:/usr/bin:/bin".to_string(),
            ),
            (
                "SSH_AUTH_SOCK".to_string(),
                "/Users/me/.1password/agent.sock".to_string(),
            ),
        ]),
        askpass: Some("/Applications/Unlatch.app/Contents/MacOS/unlatch-askpass".into()),
        list_timeout_ms: Some(20_000),
        expose_exec: Some(false),
        mass_delete_frac: Some(0.25),
        mass_delete_abs: Some(500),
        mass_delete_min: Some(16),
        remote_install_dir: Some("/var/tmp/unlatch-me".into()),
        ..minimal.clone()
    };
    let command = FfiConfig {
        transport: FfiTransport::Command {
            argv: vec![
                "/usr/local/bin/unlatchd".into(),
                "stdio".into(),
                "--root".into(),
                "/tmp/r".into(),
            ],
            env: BTreeMap::from([(
                "UNLATCH_FAULT".to_string(),
                "die_after_commit:write".to_string(),
            )]),
        },
        ..minimal.clone()
    };
    vec![("minimal", minimal), ("full", full), ("command", command)]
}

fn events() -> Vec<(&'static str, EngineEvent)> {
    vec![
        (
            "WorkingSetChanged",
            EngineEvent::WorkingSetChanged {
                anchor: vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
            },
        ),
        ("ErrorResolved", EngineEvent::ErrorResolved),
        (
            "Reimport",
            EngineEvent::Reimport {
                below: ItemId::ROOT,
            },
        ),
        (
            "NeedsUser",
            EngineEvent::NeedsUser {
                reason: "Host key for devbox changed".into(),
                url: None,
            },
        ),
        (
            "StatusChanged",
            EngineEvent::StatusChanged(status(ConnState::Paused {
                reason: "mass deletion".into(),
            })),
        ),
    ]
}

/// Every fixture file name → content.
fn generate() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, suffix, f) in requests() {
        out.insert(format!("ipc_request_{name}{suffix}.json"), pretty(&f));
    }
    for (name, suffix, f) in responses() {
        out.insert(format!("ipc_response_{name}{suffix}.json"), pretty(&f));
    }
    for (name, code) in error_codes() {
        let f = IpcFrame {
            call: 99,
            msg: IpcResponse::Error {
                code,
                msg: format!("{name} sample"),
                current: None,
            },
        };
        out.insert(format!("ipc_response_Error~code-{name}.json"), pretty(&f));
    }
    for (name, state) in conn_states() {
        out.insert(format!("engine_status_{name}.json"), pretty(&status(state)));
    }
    for (name, ev) in events() {
        let json = event_json("devbox", &ev).expect("forwarded event");
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        out.insert(format!("event_{name}.json"), pretty(&v));
    }
    for (name, cfg) in configs() {
        // Fixtures must be configs libunlatch accepts.
        cfg.clone()
            .into_engine_config()
            .unwrap_or_else(|e| panic!("config {name}: {e}"));
        out.insert(format!("engine_config_{name}.json"), pretty(&cfg));
    }
    out
}

#[test]
fn fixtures_are_current() {
    let dir = fixtures_dir();
    let want = generate();
    if std::env::var_os("UNLATCH_UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(&dir).expect("create fixtures dir");
        for entry in std::fs::read_dir(&dir).expect("read fixtures dir") {
            let p = entry.expect("dir entry").path();
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();
            if name.ends_with(".json") && !want.contains_key(&name) {
                std::fs::remove_file(&p).expect("remove stale fixture");
            }
        }
        for (name, content) in &want {
            std::fs::write(dir.join(name), content).expect("write fixture");
        }
        return;
    }
    let mut problems = Vec::new();
    for (name, content) in &want {
        match std::fs::read_to_string(dir.join(name)) {
            Ok(have) if &have == content => {}
            Ok(_) => problems.push(format!("stale: {name}")),
            Err(_) => problems.push(format!("missing: {name}")),
        }
    }
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".json") && !want.contains_key(&name) {
                problems.push(format!("unexpected: {name}"));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "Swift fixtures out of date ({}); run: UNLATCH_UPDATE_FIXTURES=1 cargo test -p unlatch-ffi --test fixtures",
        problems.join(", ")
    );
}

#[test]
fn every_forwarded_event_type_has_a_fixture() {
    let names: Vec<_> = events().into_iter().map(|(n, _)| n).collect();
    assert_eq!(
        names,
        [
            "WorkingSetChanged",
            "ErrorResolved",
            "Reimport",
            "NeedsUser",
            "StatusChanged"
        ]
    );
}
