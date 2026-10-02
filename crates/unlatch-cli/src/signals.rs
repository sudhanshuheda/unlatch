//! Waiting for SIGINT / SIGTERM / SIGHUP (clean unmount / shutdown).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub struct Terminator {
    flag: Arc<AtomicBool>,
}

impl Terminator {
    /// Install handlers. After this, those signals only set a flag.
    pub fn install() -> std::io::Result<Terminator> {
        let flag = Arc::new(AtomicBool::new(false));
        for sig in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ] {
            signal_hook::flag::register(sig, Arc::clone(&flag))?;
        }
        Ok(Terminator { flag })
    }

    pub fn requested(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Block until a signal arrives or `done()` returns true (polled every 50 ms).
    /// Returns `true` if a signal ended the wait.
    pub fn wait_until(&self, mut done: impl FnMut() -> bool) -> bool {
        loop {
            if self.requested() {
                return true;
            }
            if done() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
