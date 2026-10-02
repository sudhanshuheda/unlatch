//! Minimal logger: `UNLATCHD_LOG=<file>` (or the serve log set up by daemonize). Never writes to
//! stdout — in `stdio`/`connect` mode stdout is the protocol stream.

use std::io::Write;
use std::sync::Mutex;

static SINK: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Open the log file named by `UNLATCHD_LOG`, or `fallback` when set.
pub fn init(fallback: Option<&std::path::Path>) {
    let path = std::env::var_os("UNLATCHD_LOG")
        .map(std::path::PathBuf::from)
        .or_else(|| fallback.map(|p| p.to_path_buf()));
    if let Some(p) = path {
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
        {
            if let Ok(mut g) = SINK.lock() {
                *g = Some(f);
            }
        }
    }
}

pub fn write(args: std::fmt::Arguments<'_>) {
    if let Ok(mut g) = SINK.lock() {
        if let Some(f) = g.as_mut() {
            let now = crate::sys::now_ns();
            let _ = writeln!(
                f,
                "{}.{:03} [{}] {}",
                now / 1_000_000_000,
                (now / 1_000_000) % 1000,
                crate::sys::getpid(),
                args
            );
        }
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log::write(format_args!($($t)*)) };
}
