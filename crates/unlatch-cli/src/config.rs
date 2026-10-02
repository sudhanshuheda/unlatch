//! `unlatch agent` configuration (JSON). Schema documented in `crates/unlatch-cli/README.md`.

use anyhow::{bail, Context};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use unlatch_core::{EngineConfig, PrefetchConfig, Transport, UnlatchdBinary};

/// macOS `sockaddr_un.sun_path` is 104 bytes including the NUL (Linux: 108). The review found a
/// real default path that overflowed it (D11), so we reject long socket paths up front instead
/// of failing in `bind`.
pub const MAX_SOCKET_PATH: usize = 103;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Machine name for conflict copies; default: this host's name.
    #[serde(default)]
    pub client_name: Option<String>,
    pub domains: Vec<DomainConfig>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TransportConfig {
    Ssh {
        destination: String,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        identity: Option<String>,
        #[serde(default)]
        extra_args: Vec<String>,
    },
    Command {
        argv: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PrefetchJson {
    pub max_file: u64,
    pub per_container: u64,
    pub bytes_per_min: u64,
    pub burst: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UploadJson {
    pub arch: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DomainConfig {
    pub name: String,
    pub transport: TransportConfig,
    pub remote_root: String,
    #[serde(default)]
    pub state_dir: Option<String>,
    #[serde(default)]
    pub socket: Option<String>,
    #[serde(default)]
    pub cache_dir: Option<String>,
    #[serde(default)]
    pub temp_dir: Option<String>,
    #[serde(default)]
    pub client_name: Option<String>,
    #[serde(default)]
    pub unlatchd_command: Option<String>,
    #[serde(default)]
    pub remote_install_dir: Option<String>,
    #[serde(default)]
    pub unlatchd_upload: Vec<UploadJson>,
    #[serde(default)]
    pub cache_budget_bytes: Option<u64>,
    #[serde(default)]
    pub prefetch: Option<PrefetchJson>,
    #[serde(default)]
    pub default_lazy_names: Option<Vec<String>>,
    #[serde(default)]
    pub list_timeout_ms: Option<u64>,
    #[serde(default)]
    pub expose_exec: bool,
    #[serde(default)]
    pub mass_delete_frac: Option<f64>,
    #[serde(default)]
    pub mass_delete_abs: Option<u64>,
    #[serde(default)]
    pub mass_delete_min: Option<u64>,
    #[serde(default)]
    pub ssh_env: BTreeMap<String, String>,
    #[serde(default)]
    pub askpass: Option<String>,
}

/// `$UNLATCH_CONFIG`, else `~/.config/unlatch/agent.json`.
pub fn default_config_path() -> PathBuf {
    match std::env::var_os("UNLATCH_CONFIG") {
        Some(p) => PathBuf::from(p),
        None => crate::home_dir().join(".config/unlatch/agent.json"),
    }
}

fn valid_domain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && !name.chars().any(|c| c == '/' || c.is_control())
}

impl AgentConfig {
    pub fn parse(text: &str) -> anyhow::Result<AgentConfig> {
        let cfg: AgentConfig = serde_json::from_str(text).context("invalid agent config JSON")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> anyhow::Result<AgentConfig> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.domains.is_empty() {
            bail!("config has no domains");
        }
        let mut names = HashSet::new();
        let mut sockets = HashSet::new();
        for d in &self.domains {
            if !valid_domain_name(&d.name) {
                bail!(
                    "invalid domain name {:?} (1-64 chars, no '/' or control characters)",
                    d.name
                );
            }
            if !names.insert(d.name.as_str()) {
                bail!("duplicate domain name {:?}", d.name);
            }
            if d.remote_root.trim().is_empty() {
                bail!("domain {:?}: remote_root is empty", d.name);
            }
            match &d.transport {
                TransportConfig::Ssh { destination, .. } => {
                    // A destination starting with '-' would be parsed by ssh as an option
                    // (argument injection through a config file).
                    if destination.is_empty() || destination.starts_with('-') {
                        bail!(
                            "domain {:?}: invalid ssh destination {:?}",
                            d.name,
                            destination
                        );
                    }
                }
                TransportConfig::Command { argv, .. } => {
                    if argv.is_empty() || argv[0].is_empty() {
                        bail!("domain {:?}: transport.command.argv is empty", d.name);
                    }
                }
            }
            let sock = d.socket_path();
            let len = sock.as_os_str().len();
            if len > MAX_SOCKET_PATH {
                bail!(
                    "domain {:?}: socket path {} is {len} bytes; unix sockets allow at most {MAX_SOCKET_PATH} \
                     (set a shorter \"socket\")",
                    d.name,
                    sock.display()
                );
            }
            if !sockets.insert(sock) {
                bail!(
                    "domain {:?}: socket path shared with another domain",
                    d.name
                );
            }
            if let Some(f) = d.mass_delete_frac {
                if !(0.0..=1.0).contains(&f) {
                    bail!("domain {:?}: mass_delete_frac must be within 0..=1", d.name);
                }
            }
        }
        Ok(())
    }

    /// The domain named `name`, or the only one when `name` is `None`.
    pub fn domain(&self, name: Option<&str>) -> anyhow::Result<&DomainConfig> {
        match name {
            Some(n) => self
                .domains
                .iter()
                .find(|d| d.name == n)
                .with_context(|| format!("no domain named {n:?}")),
            None if self.domains.len() == 1 => Ok(&self.domains[0]),
            None => {
                let names: Vec<&str> = self.domains.iter().map(|d| d.name.as_str()).collect();
                bail!(
                    "config has several domains; pick one with --domain ({})",
                    names.join(", ")
                )
            }
        }
    }
}

impl DomainConfig {
    pub fn state_dir(&self) -> PathBuf {
        match &self.state_dir {
            Some(s) => crate::expand_tilde(s),
            None => crate::home_dir().join(".unlatch/client").join(&self.name),
        }
    }

    pub fn socket_path(&self) -> PathBuf {
        match &self.socket {
            Some(s) => crate::expand_tilde(s),
            None => self.state_dir().join("engine.sock"),
        }
    }

    pub fn transport(&self) -> Transport {
        match &self.transport {
            TransportConfig::Ssh {
                destination,
                port,
                identity,
                extra_args,
            } => Transport::Ssh {
                destination: destination.clone(),
                port: *port,
                identity: identity.as_deref().map(crate::expand_tilde),
                extra_args: extra_args.clone(),
            },
            TransportConfig::Command { argv, env } => Transport::Command {
                argv: argv.clone(),
                env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            },
        }
    }

    /// Build the engine config. `client_name` must already be sanitized.
    pub fn engine_config(&self, client_name: &str) -> EngineConfig {
        let mut cfg = EngineConfig::new(
            &self.name,
            self.transport(),
            &self.remote_root,
            self.state_dir(),
            client_name,
        );
        if let Some(c) = &self.cache_dir {
            cfg.cache_dir = crate::expand_tilde(c);
        }
        if let Some(t) = &self.temp_dir {
            cfg.temp_dir = crate::expand_tilde(t);
        }
        cfg.unlatchd_command = self.unlatchd_command.clone();
        cfg.remote_install_dir = self.remote_install_dir.clone();
        cfg.unlatchd_upload = self
            .unlatchd_upload
            .iter()
            .map(|u| UnlatchdBinary {
                arch: u.arch.clone(),
                path: crate::expand_tilde(&u.path),
                sha256_hex: u.sha256.clone(),
            })
            .collect();
        if let Some(b) = self.cache_budget_bytes {
            cfg.cache_budget = b;
        }
        if let Some(p) = &self.prefetch {
            cfg.prefetch = PrefetchConfig {
                max_file: p.max_file,
                per_container: p.per_container,
                bytes_per_min: p.bytes_per_min,
                burst: p.burst,
            };
        }
        if let Some(l) = &self.default_lazy_names {
            cfg.default_lazy_names = l.clone();
        }
        if let Some(ms) = self.list_timeout_ms {
            cfg.list_timeout = Duration::from_millis(ms);
        }
        cfg.expose_exec = self.expose_exec;
        if let Some(f) = self.mass_delete_frac {
            cfg.mass_delete_frac = f;
        }
        if let Some(a) = self.mass_delete_abs {
            cfg.mass_delete_abs = a;
        }
        if let Some(m) = self.mass_delete_min {
            cfg.mass_delete_min = m;
        }
        cfg.ssh_env = self
            .ssh_env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        cfg.askpass = self.askpass.as_deref().map(crate::expand_tilde);
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{
        "client_name": "laptop",
        "domains": [
            {
                "name": "dev",
                "transport": {"ssh": {"destination": "dev-box", "port": 2222, "identity": "~/.ssh/id_ed25519",
                                      "extra_args": ["-o", "ProxyJump=bastion"]}},
                "remote_root": "~/code",
                "state_dir": "/tmp/unlatch-state/dev",
                "socket": "/tmp/unlatch-dev.sock",
                "cache_budget_bytes": 1073741824,
                "prefetch": {"max_file": 1024, "per_container": 2048, "bytes_per_min": 4096, "burst": 512},
                "list_timeout_ms": 1500,
                "expose_exec": true,
                "mass_delete_frac": 0.5,
                "mass_delete_abs": 10,
                "mass_delete_min": 4,
                "remote_install_dir": "/var/tmp/unlatch-dev",
                "ssh_env": {"SSH_AUTH_SOCK": "/tmp/agent"},
                "unlatchd_upload": [{"arch": "x86_64", "path": "/opt/unlatchd", "sha256": "ab"}]
            },
            {
                "name": "local",
                "transport": {"command": {"argv": ["unlatchd", "stdio", "--root", "/srv"], "env": {"UNLATCHD_LOG": "/tmp/l"}}},
                "remote_root": "/srv"
            }
        ]
    }"#;

    #[test]
    fn parses_full_config() {
        let cfg = AgentConfig::parse(FULL).unwrap();
        assert_eq!(cfg.client_name.as_deref(), Some("laptop"));
        assert_eq!(cfg.domains.len(), 2);
        let dev = cfg.domain(Some("dev")).unwrap();
        let ec = dev.engine_config("laptop");
        assert_eq!(ec.name, "dev");
        assert_eq!(ec.remote_root, "~/code");
        assert_eq!(ec.state_dir, PathBuf::from("/tmp/unlatch-state/dev"));
        assert_eq!(ec.cache_dir, PathBuf::from("/tmp/unlatch-state/dev/cache"));
        assert_eq!(ec.cache_budget, 1 << 30);
        assert_eq!(ec.prefetch.max_file, 1024);
        assert_eq!(ec.list_timeout, Duration::from_millis(1500));
        assert!(ec.expose_exec);
        assert_eq!(ec.mass_delete_abs, 10);
        assert_eq!(ec.mass_delete_min, 4);
        assert_eq!(
            ec.remote_install_dir.as_deref(),
            Some("/var/tmp/unlatch-dev")
        );
        assert_eq!(ec.client_name, "laptop");
        assert_eq!(
            ec.ssh_env,
            vec![("SSH_AUTH_SOCK".to_string(), "/tmp/agent".to_string())]
        );
        assert_eq!(ec.unlatchd_upload.len(), 1);
        match ec.transport {
            Transport::Ssh {
                destination,
                port,
                identity,
                extra_args,
            } => {
                assert_eq!(destination, "dev-box");
                assert_eq!(port, Some(2222));
                assert_eq!(identity, Some(crate::home_dir().join(".ssh/id_ed25519")));
                assert_eq!(extra_args, vec!["-o", "ProxyJump=bastion"]);
            }
            other => panic!("unexpected transport {other:?}"),
        }
        assert_eq!(dev.socket_path(), PathBuf::from("/tmp/unlatch-dev.sock"));
    }

    #[test]
    fn defaults_for_minimal_domain() {
        let cfg = AgentConfig::parse(FULL).unwrap();
        let local = cfg.domain(Some("local")).unwrap();
        assert_eq!(
            local.state_dir(),
            crate::home_dir().join(".unlatch/client/local")
        );
        assert_eq!(local.socket_path(), local.state_dir().join("engine.sock"));
        let ec = local.engine_config("x");
        assert!(!ec.expose_exec);
        assert_eq!(ec.mass_delete_frac, 0.20);
        match ec.transport {
            Transport::Command { argv, env } => {
                assert_eq!(argv[0], "unlatchd");
                assert_eq!(
                    env,
                    vec![("UNLATCHD_LOG".to_string(), "/tmp/l".to_string())]
                );
            }
            other => panic!("unexpected transport {other:?}"),
        }
    }

    #[test]
    fn domain_selection() {
        let cfg = AgentConfig::parse(FULL).unwrap();
        assert!(cfg.domain(None).is_err(), "ambiguous without --domain");
        assert!(cfg.domain(Some("nope")).is_err());
        let one = AgentConfig::parse(
            r#"{"domains":[{"name":"a","transport":{"ssh":{"destination":"h"}},"remote_root":"/r","socket":"/tmp/a.sock"}]}"#,
        )
        .unwrap();
        assert_eq!(one.domain(None).unwrap().name, "a");
    }

    fn err_of(json: &str) -> String {
        format!("{:#}", AgentConfig::parse(json).unwrap_err())
    }

    #[test]
    fn rejects_bad_configs() {
        assert!(err_of(r#"{"domains":[]}"#).contains("no domains"));
        assert!(err_of(r#"{"domains":[{"name":"a","transport":{"ssh":{"destination":"-oProxyCommand=x"}},"remote_root":"/r"}]}"#)
            .contains("invalid ssh destination"));
        assert!(err_of(r#"{"domains":[{"name":"a/b","transport":{"ssh":{"destination":"h"}},"remote_root":"/r"}]}"#)
            .contains("invalid domain name"));
        assert!(err_of(
            r#"{"domains":[{"name":"a","transport":{"ssh":{"destination":"h"}},"remote_root":"/r","socket":"/tmp/s"},
                           {"name":"a","transport":{"ssh":{"destination":"h"}},"remote_root":"/r","socket":"/tmp/t"}]}"#
        )
        .contains("duplicate"));
        assert!(err_of(
            r#"{"domains":[{"name":"a","transport":{"ssh":{"destination":"h"}},"remote_root":"/r","socket":"/tmp/s"},
                           {"name":"b","transport":{"ssh":{"destination":"h"}},"remote_root":"/r","socket":"/tmp/s"}]}"#
        )
        .contains("shared"));
        assert!(err_of(
            r#"{"domains":[{"name":"a","transport":{"command":{"argv":[]}},"remote_root":"/r"}]}"#
        )
        .contains("argv is empty"));
        let long = "x".repeat(120);
        assert!(err_of(&format!(
            r#"{{"domains":[{{"name":"a","transport":{{"ssh":{{"destination":"h"}}}},"remote_root":"/r","socket":"/tmp/{long}"}}]}}"#
        ))
        .contains("at most 103"));
        // Typos are errors, not silently ignored.
        assert!(err_of(r#"{"domains":[{"name":"a","transport":{"ssh":{"destination":"h"}},"remote_rot":"/r"}]}"#)
            .contains("invalid agent config JSON"));
        assert!(
            err_of(r#"{"domains":[{"name":"a","transport":{"ftp":{}},"remote_root":"/r"}]}"#)
                .contains("invalid agent config JSON")
        );
    }
}
