//! Engine configuration as JSON (the `config_json` argument of `unlatch_engine_start`).
//!
//! Mirrors [`unlatch_core::EngineConfig`]; every field not listed as required has the engine's
//! default. Unknown keys are rejected so a Swift-side typo fails loudly instead of being ignored.
//!
//! ```json
//! {
//!   "name": "devbox",                       // required: NSFileProviderDomainIdentifier
//!   "transport": {"ssh": {"destination": "devbox", "port": 22,
//!                         "identity": "/Users/me/.ssh/id_ed25519", "extra_args": []}},
//!                // or {"command": {"argv": ["/path/unlatchd", "stdio", …], "env": {"K": "V"}}}
//!   "remote_root": "~/code",                 // required
//!   "state_dir": "/abs/path",                // required, absolute
//!   "client_name": "Sam's MacBook",          // required (SCDynamicStoreCopyComputerName)
//!   "cache_dir": "/abs", "temp_dir": "/abs", // default: <state_dir>/cache, <state_dir>/tmp
//!   "unlatchd_command": null,
//!   "remote_install_dir": null,              // VM dir the bootstrap probe tries first
//!   "unlatchd_upload": [{"arch": "x86_64", "path": "/abs/unlatchd", "sha256_hex": "…64 hex…"}],
//!   "cache_budget": 5368709120,
//!   "prefetch": {"max_file": 262144, "per_container": 8388608,
//!                "bytes_per_min": 67108864, "burst": 16777216},
//!   "default_lazy_names": ["node_modules", …],
//!   "ssh_env": {"PATH": "…", "SSH_AUTH_SOCK": "…"},
//!   "askpass": "/Applications/Unlatch.app/Contents/MacOS/unlatch-askpass",
//!   "list_timeout_ms": 20000,
//!   "expose_exec": false,
//!   "mass_delete_frac": 0.2,
//!   "mass_delete_abs": 1000,
//!   "mass_delete_min": 32
//! }
//! ```

use crate::util::FfiError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use unlatch_core::{EngineConfig, PrefetchConfig, Transport, UnlatchdBinary};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FfiConfig {
    pub name: String,
    pub transport: FfiTransport,
    pub remote_root: String,
    pub state_dir: PathBuf,
    pub client_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlatchd_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_install_dir: Option<String>,
    #[serde(default)]
    pub unlatchd_upload: Vec<FfiUnlatchdBinary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefetch: Option<FfiPrefetch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_lazy_names: Option<Vec<String>>,
    /// Environment for spawning ssh. The host passes its full environment with PATH (and, if the
    /// user opted in, SSH_AUTH_SOCK) taken from the login shell (review D21).
    #[serde(default)]
    pub ssh_env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub askpass: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose_exec: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mass_delete_frac: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mass_delete_abs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mass_delete_min: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum FfiTransport {
    Ssh {
        destination: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<PathBuf>,
        #[serde(default)]
        extra_args: Vec<String>,
    },
    Command {
        argv: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FfiUnlatchdBinary {
    /// `uname -m` of the VM this binary runs on: `x86_64` or `aarch64`.
    pub arch: String,
    pub path: PathBuf,
    pub sha256_hex: String,
}

/// Every field optional; missing ones keep [`PrefetchConfig::default`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FfiPrefetch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_container: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_per_min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u64>,
}

const UNLATCHD_ARCHES: &[&str] = &["x86_64", "aarch64"];

impl FfiConfig {
    pub fn from_json(json: &str) -> Result<FfiConfig, String> {
        serde_json::from_str(json).map_err(|e| format!("config JSON: {e}"))
    }

    /// Validate and convert into the engine's config.
    pub fn into_engine_config(self) -> Result<EngineConfig, String> {
        if self.name.trim().is_empty() {
            return Err("name must not be empty".into());
        }
        if self.remote_root.trim().is_empty() {
            return Err("remote_root must not be empty".into());
        }
        if self.client_name.trim().is_empty() {
            return Err("client_name must not be empty".into());
        }
        require_absolute("state_dir", &self.state_dir)?;
        let transport = match self.transport {
            FfiTransport::Ssh {
                destination,
                port,
                identity,
                extra_args,
            } => {
                if destination.trim().is_empty() {
                    return Err("transport.ssh.destination must not be empty".into());
                }
                // ssh would parse a leading '-' as an option: never let a host string do that.
                if destination.starts_with('-') {
                    return Err("transport.ssh.destination must not start with '-'".into());
                }
                if port == Some(0) {
                    return Err("transport.ssh.port must be 1..=65535".into());
                }
                if let Some(id) = &identity {
                    require_absolute("transport.ssh.identity", id)?;
                }
                Transport::Ssh {
                    destination,
                    port,
                    identity,
                    extra_args,
                }
            }
            FfiTransport::Command { argv, env } => {
                if argv.is_empty() || argv[0].is_empty() {
                    return Err("transport.command.argv must name a program".into());
                }
                Transport::Command {
                    argv,
                    env: env.into_iter().collect(),
                }
            }
        };
        let mut cfg = EngineConfig::new(
            &self.name,
            transport,
            &self.remote_root,
            self.state_dir,
            &self.client_name,
        );
        if let Some(d) = self.cache_dir {
            require_absolute("cache_dir", &d)?;
            cfg.cache_dir = d;
        }
        if let Some(d) = self.temp_dir {
            require_absolute("temp_dir", &d)?;
            cfg.temp_dir = d;
        }
        cfg.unlatchd_command = self.unlatchd_command;
        if let Some(d) = self.remote_install_dir {
            if d.is_empty() || d.contains('\0') {
                return Err("remote_install_dir must be a non-empty path".into());
            }
            cfg.remote_install_dir = Some(d);
        }
        for b in &self.unlatchd_upload {
            if !UNLATCHD_ARCHES.contains(&b.arch.as_str()) {
                return Err(format!(
                    "unlatchd_upload arch {:?} is not one of {UNLATCHD_ARCHES:?}",
                    b.arch
                ));
            }
            require_absolute("unlatchd_upload.path", &b.path)?;
            if b.sha256_hex.len() != 64 || !b.sha256_hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!(
                    "unlatchd_upload sha256_hex for {} must be 64 hex digits",
                    b.arch
                ));
            }
        }
        cfg.unlatchd_upload = self
            .unlatchd_upload
            .into_iter()
            .map(|b| UnlatchdBinary {
                arch: b.arch,
                path: b.path,
                sha256_hex: b.sha256_hex.to_ascii_lowercase(),
            })
            .collect();
        if let Some(b) = self.cache_budget {
            cfg.cache_budget = b;
        }
        if let Some(p) = self.prefetch {
            let d = PrefetchConfig::default();
            cfg.prefetch = PrefetchConfig {
                max_file: p.max_file.unwrap_or(d.max_file),
                per_container: p.per_container.unwrap_or(d.per_container),
                bytes_per_min: p.bytes_per_min.unwrap_or(d.bytes_per_min),
                burst: p.burst.unwrap_or(d.burst),
            };
        }
        if let Some(names) = self.default_lazy_names {
            cfg.default_lazy_names = names;
        }
        cfg.ssh_env = self.ssh_env.into_iter().collect();
        if let Some(a) = self.askpass {
            require_absolute("askpass", &a)?;
            cfg.askpass = Some(a);
        }
        if let Some(ms) = self.list_timeout_ms {
            cfg.list_timeout = Duration::from_millis(ms);
        }
        if let Some(x) = self.expose_exec {
            cfg.expose_exec = x;
        }
        if let Some(f) = self.mass_delete_frac {
            if !(f > 0.0 && f <= 1.0) {
                return Err("mass_delete_frac must be in (0, 1]".into());
            }
            cfg.mass_delete_frac = f;
        }
        if let Some(n) = self.mass_delete_abs {
            if n == 0 {
                return Err("mass_delete_abs must be > 0".into());
            }
            cfg.mass_delete_abs = n;
        }
        if let Some(n) = self.mass_delete_min {
            cfg.mass_delete_min = n;
        }
        Ok(cfg)
    }
}

fn require_absolute(what: &str, p: &std::path::Path) -> Result<(), String> {
    if p.is_absolute() {
        Ok(())
    } else {
        Err(format!(
            "{what} must be an absolute path, got {}",
            p.display()
        ))
    }
}

/// Parse + validate in one step (what `unlatch_engine_start` does).
pub(crate) fn parse(json: &str) -> Result<EngineConfig, FfiError> {
    FfiConfig::from_json(json)
        .and_then(FfiConfig::into_engine_config)
        .map_err(FfiError::invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> serde_json::Value {
        serde_json::json!({
            "name": "devbox",
            "transport": {"ssh": {"destination": "devbox"}},
            "remote_root": "~/code",
            "state_dir": "/tmp/unlatch-state",
            "client_name": "Sam's MacBook"
        })
    }

    fn parse_v(v: &serde_json::Value) -> Result<EngineConfig, FfiError> {
        parse(&v.to_string())
    }

    #[test]
    fn minimal_ssh_gets_engine_defaults() {
        let c = parse_v(&base()).unwrap();
        let d = EngineConfig::new(
            "devbox",
            Transport::Command {
                argv: vec![],
                env: vec![],
            },
            "~/code",
            PathBuf::from("/tmp/unlatch-state"),
            "x",
        );
        assert_eq!(c.name, "devbox");
        assert_eq!(c.remote_root, "~/code");
        assert_eq!(c.client_name, "Sam's MacBook");
        assert_eq!(c.cache_dir, PathBuf::from("/tmp/unlatch-state/cache"));
        assert_eq!(c.temp_dir, PathBuf::from("/tmp/unlatch-state/tmp"));
        assert_eq!(c.cache_budget, d.cache_budget);
        assert_eq!(c.default_lazy_names, d.default_lazy_names);
        assert_eq!(c.mass_delete_abs, 1000);
        assert_eq!(c.mass_delete_min, 32);
        assert_eq!(c.remote_install_dir, None);
        assert!(!c.expose_exec);
        assert!(c.askpass.is_none());
        assert!(c.ssh_env.is_empty());
        match c.transport {
            Transport::Ssh {
                destination,
                port,
                identity,
                extra_args,
            } => {
                assert_eq!(destination, "devbox");
                assert_eq!(port, None);
                assert_eq!(identity, None);
                assert!(extra_args.is_empty());
            }
            other => panic!("wrong transport {other:?}"),
        }
    }

    #[test]
    fn every_optional_field_is_applied() {
        let mut v = base();
        let o = v.as_object_mut().unwrap();
        o.insert(
            "transport".into(),
            serde_json::json!({"ssh": {"destination": "me@vm", "port": 2222,
                "identity": "/Users/me/.ssh/id", "extra_args": ["-o", "ProxyJump=bastion"]}}),
        );
        o.insert("cache_dir".into(), "/c".into());
        o.insert("temp_dir".into(), "/t".into());
        o.insert("unlatchd_command".into(), "/opt/unlatchd".into());
        o.insert(
            "unlatchd_upload".into(),
            serde_json::json!([{"arch": "aarch64", "path": "/b/unlatchd", "sha256_hex": "AB".repeat(32)}]),
        );
        o.insert("cache_budget".into(), 42.into());
        o.insert(
            "prefetch".into(),
            serde_json::json!({"max_file": 1, "burst": 4}),
        );
        o.insert("default_lazy_names".into(), serde_json::json!(["x"]));
        o.insert(
            "ssh_env".into(),
            serde_json::json!({"PATH": "/opt/homebrew/bin:/usr/bin", "A": "b"}),
        );
        o.insert(
            "askpass".into(),
            "/Applications/Unlatch.app/Contents/MacOS/unlatch-askpass".into(),
        );
        o.insert("list_timeout_ms".into(), 1500.into());
        o.insert("expose_exec".into(), true.into());
        o.insert("mass_delete_frac".into(), 0.5.into());
        o.insert("mass_delete_abs".into(), 10.into());
        o.insert("mass_delete_min".into(), 3.into());
        o.insert("remote_install_dir".into(), "/var/tmp/unlatch-test".into());
        let c = parse_v(&v).unwrap();
        match &c.transport {
            Transport::Ssh {
                destination,
                port,
                identity,
                extra_args,
            } => {
                assert_eq!(destination, "me@vm");
                assert_eq!(*port, Some(2222));
                assert_eq!(
                    identity.as_deref(),
                    Some(std::path::Path::new("/Users/me/.ssh/id"))
                );
                assert_eq!(extra_args, &["-o", "ProxyJump=bastion"]);
            }
            other => panic!("wrong transport {other:?}"),
        }
        assert_eq!(c.cache_dir, PathBuf::from("/c"));
        assert_eq!(c.temp_dir, PathBuf::from("/t"));
        assert_eq!(c.unlatchd_command.as_deref(), Some("/opt/unlatchd"));
        assert_eq!(c.unlatchd_upload.len(), 1);
        assert_eq!(c.unlatchd_upload[0].sha256_hex, "ab".repeat(32));
        assert_eq!(c.cache_budget, 42);
        assert_eq!(c.prefetch.max_file, 1);
        assert_eq!(c.prefetch.burst, 4);
        assert_eq!(
            c.prefetch.per_container,
            PrefetchConfig::default().per_container
        );
        assert_eq!(c.default_lazy_names, vec!["x".to_string()]);
        assert_eq!(
            c.ssh_env,
            vec![
                ("A".into(), "b".into()),
                ("PATH".into(), "/opt/homebrew/bin:/usr/bin".into())
            ]
        );
        assert!(c.askpass.is_some());
        assert_eq!(c.list_timeout, Duration::from_millis(1500));
        assert!(c.expose_exec);
        assert_eq!(c.mass_delete_frac, 0.5);
        assert_eq!(c.mass_delete_abs, 10);
        assert_eq!(c.mass_delete_min, 3);
        assert_eq!(
            c.remote_install_dir.as_deref(),
            Some("/var/tmp/unlatch-test")
        );
    }

    #[test]
    fn command_transport() {
        let mut v = base();
        v["transport"] = serde_json::json!({"command": {"argv": ["/bin/unlatchd", "stdio"], "env": {"UNLATCH_FAULT": ""}}});
        match parse_v(&v).unwrap().transport {
            Transport::Command { argv, env } => {
                assert_eq!(argv, vec!["/bin/unlatchd", "stdio"]);
                assert_eq!(env, vec![("UNLATCH_FAULT".to_string(), String::new())]);
            }
            other => panic!("wrong transport {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_input() {
        let cases: Vec<(&str, serde_json::Value)> = vec![
            ("unknown key", {
                let mut v = base();
                v["cache_dri"] = "/x".into();
                v
            }),
            ("unknown transport key", {
                let mut v = base();
                v["transport"] = serde_json::json!({"ssh": {"destination": "a", "prot": 1}});
                v
            }),
            ("unknown transport kind", {
                let mut v = base();
                v["transport"] = serde_json::json!({"telnet": {"destination": "a"}});
                v
            }),
            ("missing name", {
                let mut v = base();
                v.as_object_mut().unwrap().remove("name");
                v
            }),
            ("missing client_name", {
                let mut v = base();
                v.as_object_mut().unwrap().remove("client_name");
                v
            }),
            ("empty name", {
                let mut v = base();
                v["name"] = " ".into();
                v
            }),
            ("relative state_dir", {
                let mut v = base();
                v["state_dir"] = "state".into();
                v
            }),
            ("option-like destination", {
                let mut v = base();
                v["transport"] = serde_json::json!({"ssh": {"destination": "-oProxyCommand=evil"}});
                v
            }),
            ("port 0", {
                let mut v = base();
                v["transport"] = serde_json::json!({"ssh": {"destination": "a", "port": 0}});
                v
            }),
            ("port overflow", {
                let mut v = base();
                v["transport"] = serde_json::json!({"ssh": {"destination": "a", "port": 70000}});
                v
            }),
            ("empty argv", {
                let mut v = base();
                v["transport"] = serde_json::json!({"command": {"argv": []}});
                v
            }),
            ("bad arch", {
                let mut v = base();
                v["unlatchd_upload"] = serde_json::json!([{"arch": "arm64", "path": "/x", "sha256_hex": "a".repeat(64)}]);
                v
            }),
            ("bad sha", {
                let mut v = base();
                v["unlatchd_upload"] =
                    serde_json::json!([{"arch": "x86_64", "path": "/x", "sha256_hex": "zz"}]);
                v
            }),
            ("frac > 1", {
                let mut v = base();
                v["mass_delete_frac"] = 1.5.into();
                v
            }),
            ("frac 0", {
                let mut v = base();
                v["mass_delete_frac"] = 0.0.into();
                v
            }),
            ("abs 0", {
                let mut v = base();
                v["mass_delete_abs"] = 0.into();
                v
            }),
            ("empty remote_install_dir", {
                let mut v = base();
                v["remote_install_dir"] = "".into();
                v
            }),
            ("relative askpass", {
                let mut v = base();
                v["askpass"] = "unlatch-askpass".into();
                v
            }),
        ];
        for (what, v) in cases {
            let e = parse_v(&v).expect_err(what);
            assert_eq!(e.code, crate::util::CODE_INVALID_ARGUMENT, "{what}");
        }
        assert!(parse("not json").is_err());
    }

    #[test]
    fn serializes_back_to_accepted_json() {
        let v = base();
        let c = FfiConfig::from_json(&v.to_string()).unwrap();
        let again = serde_json::to_string(&c).unwrap();
        assert_eq!(FfiConfig::from_json(&again).unwrap(), c);
    }
}
