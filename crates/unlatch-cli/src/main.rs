//! `unlatch` — see `crates/unlatch-cli/README.md`.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use unlatch_cli::{agent, doctor, ipccmd, mount, probe};

#[derive(Parser)]
#[command(
    name = "unlatch",
    version,
    about = "Your cloud VM's files, as snappy as a local folder"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone)]
struct IpcOpts {
    /// Engine IPC socket (default: from the agent config).
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Domain name served on that socket / selected from the config.
    #[arg(long)]
    domain: Option<String>,
    /// Agent config (default: $UNLATCH_CONFIG or ~/.config/unlatch/agent.json).
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mount a VM directory with FUSE (Linux).
    Mount {
        /// Local directory to mount on.
        mountpoint: PathBuf,
        /// ssh destination (config alias or user@host).
        #[arg(long, conflicts_with = "command")]
        host: Option<String>,
        #[arg(long, requires = "host")]
        port: Option<u16>,
        #[arg(long, requires = "host")]
        identity: Option<PathBuf>,
        /// Extra ssh argument (repeatable), e.g. --ssh-arg=-oProxyJump=bastion.
        #[arg(long = "ssh-arg", allow_hyphen_values = true, requires = "host")]
        ssh_args: Vec<String>,
        /// Root directory on the VM (absolute or ~/…).
        #[arg(long)]
        root: String,
        /// Client state (replica, cache). Default: ~/.unlatch/mount/<hash>.
        #[arg(long)]
        state: Option<PathBuf>,
        /// Display name (default: <host>-<root basename>).
        #[arg(long)]
        name: Option<String>,
        /// Stay in the foreground (default: detach once mounted).
        #[arg(long)]
        foreground: bool,
        /// Kernel entry/attribute cache TTL; push invalidation keeps it correct.
        #[arg(long, default_value_t = 300)]
        ttl_secs: u64,
        /// Do not prefetch small files when a directory is listed.
        #[arg(long)]
        no_prefetch: bool,
        /// Hide exec bits.
        #[arg(long)]
        no_exec: bool,
        #[arg(long, default_value_t = 16)]
        workers: usize,
        /// Wait up to this long for the first sync before mounting (0 = mount the last-known
        /// tree immediately).
        #[arg(long, default_value_t = 0)]
        wait_secs: u64,
        /// Remote unlatchd command (skips bootstrap/upload), e.g. ~/.local/bin/unlatchd.
        #[arg(long, requires = "host")]
        unlatchd: Option<String>,
        /// Speak the protocol to this command instead of ssh (tests: `unlatchd stdio --root R
        /// --state S`). Takes the rest of the command line; put it last.
        #[arg(long, num_args = 1.., allow_hyphen_values = true, value_name = "ARGV")]
        command: Vec<String>,
    },
    /// Run one engine per configured domain and serve IPC (the macOS host app's job, headless).
    Agent {
        #[arg(long)]
        config: PathBuf,
        /// Only these domains (repeatable).
        #[arg(long = "domain")]
        domains: Vec<String>,
    },
    /// List a directory through the engine (IPC).
    Ls {
        #[arg(default_value = "")]
        path: String,
        /// Show item ids.
        #[arg(long)]
        ids: bool,
        #[command(flatten)]
        ipc: IpcOpts,
    },
    /// Show one item's metadata through the engine (IPC).
    Stat {
        path: String,
        #[command(flatten)]
        ipc: IpcOpts,
    },
    /// Print a file's content through the engine (IPC).
    Cat {
        path: String,
        #[command(flatten)]
        ipc: IpcOpts,
    },
    /// Engine status (every configured domain unless --domain/--socket).
    Status {
        #[command(flatten)]
        ipc: IpcOpts,
    },
    /// Check a VM for everything Unlatch needs.
    Doctor {
        #[arg(long)]
        host: String,
        #[arg(long)]
        root: String,
        #[arg(long = "ssh-arg", allow_hyphen_values = true)]
        ssh_args: Vec<String>,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Measure the real user experience: VM ⇄ local latencies, first listings, conflicts.
    Probe {
        /// The local view of --remote (an `unlatch mount` path, or ~/Library/CloudStorage/<domain>).
        #[arg(long)]
        local: PathBuf,
        /// ssh destination of the VM.
        #[arg(long)]
        ssh: String,
        /// The VM directory shown at --local.
        #[arg(long)]
        remote: String,
        /// Write the JSON report here.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Repetitions per latency check.
        #[arg(short, long, default_value_t = 20)]
        n: usize,
        /// Per-step timeout.
        #[arg(long, default_value_t = 20)]
        timeout_secs: u64,
        #[arg(long = "ssh-arg", allow_hyphen_values = true)]
        ssh_args: Vec<String>,
    },
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("UNLATCH_LOG_LEVEL")
        .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,fuser=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn ipc_targets(ipc: &IpcOpts, all: bool) -> anyhow::Result<Vec<ipccmd::IpcTarget>> {
    ipccmd::targets(
        ipc.socket.as_deref(),
        ipc.domain.as_deref(),
        ipc.config.as_deref(),
        all,
    )
}

fn one_target(ipc: &IpcOpts) -> anyhow::Result<ipccmd::IpcTarget> {
    ipc_targets(ipc, false)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no engine selected"))
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.cmd {
        Cmd::Mount {
            mountpoint,
            host,
            port,
            identity,
            ssh_args,
            root,
            state,
            name,
            foreground,
            ttl_secs,
            no_prefetch,
            no_exec,
            workers,
            wait_secs,
            unlatchd,
            command,
        } => {
            mount::run_mount(mount::MountArgs {
                mountpoint,
                host,
                port,
                identity,
                ssh_args,
                command,
                root,
                state,
                name,
                foreground,
                ttl: Duration::from_secs(ttl_secs),
                prefetch: !no_prefetch,
                exec: !no_exec,
                workers,
                wait_live: Duration::from_secs(wait_secs),
                unlatchd_command: unlatchd,
            })?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Agent { config, domains } => {
            agent::run_agent(&config, &domains)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Ls { path, ids, ipc } => {
            ipccmd::ls(&one_target(&ipc)?, &path, ids)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Stat { path, ipc } => {
            ipccmd::stat(&one_target(&ipc)?, &path)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Cat { path, ipc } => {
            ipccmd::cat(&one_target(&ipc)?, &path)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Status { ipc } => {
            ipccmd::status(&ipc_targets(&ipc, true)?)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Doctor {
            host,
            root,
            ssh_args,
            json,
        } => {
            let report = doctor::run(&host, &root, &ssh_args);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                doctor::print(&report);
            }
            Ok(if doctor::failed(&report) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
        Cmd::Probe {
            local,
            ssh,
            remote,
            json,
            n,
            timeout_secs,
            ssh_args,
        } => {
            let args = probe::ProbeArgs {
                local,
                ssh,
                remote,
                json,
                n,
                timeout: Duration::from_secs(timeout_secs),
                ssh_args,
            };
            let report = probe::run(&args)?;
            print!("{}", probe::scorecard(&report));
            if let Some(path) = &args.json {
                std::fs::write(path, serde_json::to_string_pretty(&report)? + "\n")?;
                println!("  report: {}", path.display());
            }
            Ok(if probe::all_ok(&report) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("unlatch: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn command_takes_the_rest_of_the_line() {
        let cli = Cli::try_parse_from([
            "unlatch",
            "mount",
            "/mnt",
            "--root",
            "/r",
            "--foreground",
            "--command",
            "/bin/unlatchd",
            "stdio",
            "--root",
            "/r",
            "--state",
            "/s",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Mount {
                command,
                root,
                foreground,
                host,
                ..
            } => {
                assert_eq!(
                    command,
                    vec!["/bin/unlatchd", "stdio", "--root", "/r", "--state", "/s"]
                );
                assert_eq!(root, "/r");
                assert!(foreground);
                assert!(host.is_none());
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn host_and_command_conflict() {
        assert!(Cli::try_parse_from([
            "unlatch",
            "mount",
            "/mnt",
            "--root",
            "/r",
            "--host",
            "h",
            "--command",
            "x"
        ])
        .is_err());
    }

    #[test]
    fn ipc_opts_parse() {
        let cli = Cli::try_parse_from([
            "unlatch", "ls", "src", "--socket", "/tmp/s", "--domain", "d", "--ids",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Ls { path, ids, ipc } => {
                assert_eq!(path, "src");
                assert!(ids);
                assert_eq!(ipc.domain.as_deref(), Some("d"));
            }
            _ => panic!("wrong subcommand"),
        }
    }
}
