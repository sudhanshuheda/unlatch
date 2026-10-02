//! A small fixed worker pool. fuser 0.15 runs one request loop; anything that may wait on the
//! engine (and therefore the network) is handed to a worker with its `Reply`, so one cold
//! download never stalls `ls` in another directory.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

type Job = Box<dyn FnOnce() + Send + 'static>;

pub struct Pool {
    tx: Option<Sender<Job>>,
    threads: Vec<JoinHandle<()>>,
}

impl Pool {
    pub fn new(workers: usize, name: &str) -> std::io::Result<Pool> {
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        let mut threads = Vec::with_capacity(workers.max(1));
        for i in 0..workers.max(1) {
            let rx = Arc::clone(&rx);
            threads.push(
                std::thread::Builder::new()
                    .name(format!("{name}-{i}"))
                    .spawn(move || worker(&rx))?,
            );
        }
        Ok(Pool {
            tx: Some(tx),
            threads,
        })
    }

    /// Run `job` on a worker. If the pool is shutting down it runs inline, so a reply is never
    /// dropped unanswered (the kernel would wait forever).
    pub fn spawn(&self, job: impl FnOnce() + Send + 'static) {
        match &self.tx {
            Some(tx) => {
                if let Err(e) = tx.send(Box::new(job)) {
                    (e.0)();
                }
            }
            None => job(),
        }
    }
}

fn worker(rx: &Mutex<Receiver<Job>>) {
    loop {
        let job = {
            let guard = match rx.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.recv()
        };
        match job {
            Ok(job) => job(),
            Err(_) => return,
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.tx.take();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn runs_all_jobs_before_drop_returns() {
        let n = Arc::new(AtomicUsize::new(0));
        {
            let pool = Pool::new(4, "t").unwrap();
            for _ in 0..100 {
                let n = Arc::clone(&n);
                pool.spawn(move || {
                    n.fetch_add(1, Ordering::SeqCst);
                });
            }
        }
        assert_eq!(n.load(Ordering::SeqCst), 100);
    }
}
