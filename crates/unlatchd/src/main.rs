//! `unlatchd` CLI (shared contract):
//!
//! ```text
//! unlatchd stdio   --root <R> --state <DIR>             one wire session on stdin/stdout, in-process
//! unlatchd connect --root <R> [--state <DIR>]           attach to / spawn `serve`, bridge stdio
//! unlatchd serve   --root <R> [--state <DIR>] [--foreground]
//! unlatchd status|stop|gc [--state <DIR>]
//! unlatchd --version
//! ```

use std::path::PathBuf;
use unlatchd::lifecycle;

const USAGE: &str =
    "usage: unlatchd (stdio|connect|serve) --root <R> [--state <DIR>] [--foreground]
       unlatchd (status|stop|gc) [--state <DIR>]
       unlatchd --version";

struct Args {
    cmd: String,
    root: Option<PathBuf>,
    state: Option<PathBuf>,
    foreground: bool,
}

fn parse() -> Result<Args, String> {
    let mut it = std::env::args_os().skip(1);
    let cmd = it
        .next()
        .ok_or("missing command")?
        .to_string_lossy()
        .into_owned();
    let mut a = Args {
        cmd,
        root: None,
        state: None,
        foreground: false,
    };
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("--root") => {
                a.root = Some(PathBuf::from(it.next().ok_or("--root needs a value")?))
            }
            Some("--state") => {
                a.state = Some(PathBuf::from(it.next().ok_or("--state needs a value")?))
            }
            Some("--foreground") => a.foreground = true,
            _ => return Err(format!("unexpected argument {}", arg.to_string_lossy())),
        }
    }
    Ok(a)
}

fn main() {
    unlatchd::sys::limit_malloc_arenas();
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("unlatchd: {e}\n{USAGE}");
            std::process::exit(lifecycle::EXIT_USAGE);
        }
    };
    let root = || -> PathBuf {
        match &args.root {
            Some(r) => unlatchd::core::expand_root(&r.to_string_lossy()),
            None => {
                eprintln!("unlatchd: --root is required\n{USAGE}");
                std::process::exit(lifecycle::EXIT_USAGE);
            }
        }
    };
    let state = args.state.as_deref();
    let code = match args.cmd.as_str() {
        "--version" | "version" => {
            println!(
                "unlatchd {} (proto {})",
                env!("CARGO_PKG_VERSION"),
                unlatch_proto::PROTO_VERSION
            );
            0
        }
        "stdio" => lifecycle::stdio(&root(), state),
        "connect" => lifecycle::connect(&root(), state),
        "serve" => lifecycle::serve(&root(), state, args.foreground),
        "status" => lifecycle::status(state),
        "stop" => lifecycle::stop(state),
        "gc" => lifecycle::gc(state),
        "--help" | "-h" | "help" => {
            println!("{USAGE}");
            0
        }
        other => {
            eprintln!("unlatchd: unknown command {other}\n{USAGE}");
            lifecycle::EXIT_USAGE
        }
    };
    std::process::exit(code);
}
