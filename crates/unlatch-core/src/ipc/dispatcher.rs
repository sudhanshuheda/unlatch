//! Per-connection multiplexing: worker pools, per-call cancellation, serialized replies.

use super::dispatch::Disposition;
use crate::CancelToken;
use std::collections::{HashMap, VecDeque};
use std::os::fd::OwnedFd;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use unlatch_proto::ipc::{IpcFrame, IpcRequest, IpcResponse};
use unlatch_proto::{ErrorCode, ProtoError};

/// Request handler: runs one call, sends `Progress` frames and one final frame through the
/// callback, honours the call's [`CancelToken`].
pub(crate) type Handler = Arc<
    dyn Fn(
            IpcFrame<IpcRequest>,
            Option<OwnedFd>,
            &mut dyn FnMut(IpcFrame<IpcResponse>),
            &CancelToken,
        ) -> Disposition
        + Send
        + Sync,
>;

pub(crate) type SendFn = Box<dyn Fn(IpcFrame<IpcResponse>) + Send + Sync>;
pub(crate) type CloseFn = Box<dyn Fn() + Send + Sync>;

/// Metadata calls (Item/Enumerate/ChangesSince…) get their own pool so a folder open never
/// queues behind downloads (perf finding "strict request/response IPC"; D11). Sizes follow the
/// shim's explicit pipeline depths (16 downloads + 4 uploads, review (e)9) with headroom.
const INTERACTIVE_WORKERS: usize = 16;
const TRANSFER_WORKERS: usize = 24;
/// Calls queued beyond this are refused instead of growing without bound.
const MAX_QUEUED: usize = 4096;
const IDLE_EXIT: Duration = Duration::from_secs(30);

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking handler is caught per call; a poisoned lock only means another thread panicked
    // mid-update of plain bookkeeping, which stays consistent enough to keep serving.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

type Job = Box<dyn FnOnce() + Send>;

struct PoolState {
    queue: VecDeque<Job>,
    threads: usize,
    idle: usize,
    closed: bool,
}

/// A lazily grown, bounded thread pool whose idle workers exit after [`IDLE_EXIT`].
struct Pool {
    name: &'static str,
    max: usize,
    state: Mutex<PoolState>,
    cv: Condvar,
}

impl Pool {
    fn new(name: &'static str, max: usize) -> Arc<Pool> {
        Arc::new(Pool {
            name,
            max,
            state: Mutex::new(PoolState {
                queue: VecDeque::new(),
                threads: 0,
                idle: 0,
                closed: false,
            }),
            cv: Condvar::new(),
        })
    }

    /// Queue a job. `Err(job)` when the pool is closed or saturated.
    fn submit(self: &Arc<Self>, job: Job) -> std::result::Result<(), Job> {
        let mut st = lock(&self.state);
        if st.closed || st.queue.len() >= MAX_QUEUED {
            return Err(job);
        }
        st.queue.push_back(job);
        if st.queue.len() > st.idle && st.threads < self.max {
            let pool = Arc::clone(self);
            let spawned = std::thread::Builder::new()
                .name(format!("unlatch-ipc-{}", self.name))
                .spawn(move || pool.worker());
            match spawned {
                Ok(_) => st.threads += 1,
                // Existing workers will still drain the queue; only concurrency suffers.
                Err(e) => tracing::warn!("ipc: cannot spawn worker: {e}"),
            }
        }
        self.cv.notify_one();
        Ok(())
    }

    fn worker(self: Arc<Self>) {
        let mut st = lock(&self.state);
        loop {
            if let Some(job) = st.queue.pop_front() {
                drop(st);
                job();
                st = lock(&self.state);
                continue;
            }
            if st.closed {
                break;
            }
            st.idle += 1;
            let (guard, to) = self
                .cv
                .wait_timeout(st, IDLE_EXIT)
                .unwrap_or_else(PoisonError::into_inner);
            st = guard;
            st.idle -= 1;
            if to.timed_out() && st.queue.is_empty() {
                break;
            }
        }
        st.threads -= 1;
    }

    /// Stop accepting jobs, drop queued ones (their fds close) and let idle workers exit.
    fn close(&self) {
        let dropped: Vec<Job> = {
            let mut st = lock(&self.state);
            st.closed = true;
            st.queue.drain(..).collect()
        };
        drop(dropped);
        self.cv.notify_all();
    }
}

struct CallState {
    token: CancelToken,
    /// Set once the final frame (reply or `Cancelled`) has been sent; later frames are dropped.
    finished: AtomicBool,
}

pub(crate) struct DispInner {
    handler: Handler,
    send: SendFn,
    on_close: Option<CloseFn>,
    /// Serializes frames and makes "check finished + send" atomic per call.
    send_lock: Mutex<()>,
    calls: Mutex<HashMap<u64, Arc<CallState>>>,
    closed: AtomicBool,
    interactive: Arc<Pool>,
    transfer: Arc<Pool>,
}

fn is_transfer(req: &IpcRequest) -> bool {
    matches!(
        req,
        IpcRequest::Fetch { .. }
            | IpcRequest::Create { .. }
            | IpcRequest::Modify { .. }
            | IpcRequest::Delete { .. }
    )
}

impl DispInner {
    pub(crate) fn new(handler: Handler, send: SendFn, on_close: Option<CloseFn>) -> Arc<DispInner> {
        Arc::new(DispInner {
            handler,
            send,
            on_close,
            send_lock: Mutex::new(()),
            calls: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            interactive: Pool::new("meta", INTERACTIVE_WORKERS),
            transfer: Pool::new("xfer", TRANSFER_WORKERS),
        })
    }

    fn send_raw(&self, frame: IpcFrame<IpcResponse>) {
        let _g = lock(&self.send_lock);
        if !self.closed.load(Ordering::SeqCst) {
            (self.send)(frame);
        }
    }

    /// Send a frame for `st`'s call unless its final frame already went out. `last` marks the
    /// final frame.
    fn send_for(&self, st: &CallState, frame: IpcFrame<IpcResponse>, last: bool) -> bool {
        let _g = lock(&self.send_lock);
        if self.closed.load(Ordering::SeqCst) || st.finished.load(Ordering::SeqCst) {
            return false;
        }
        if last {
            st.finished.store(true, Ordering::SeqCst);
        }
        (self.send)(frame);
        true
    }

    fn error_frame(call: u64, code: ErrorCode, msg: &str) -> IpcFrame<IpcResponse> {
        IpcFrame {
            call,
            msg: IpcResponse::Error {
                code,
                msg: msg.to_string(),
                current: None,
            },
        }
    }

    pub(crate) fn submit(self: &Arc<Self>, frame: IpcFrame<IpcRequest>, fd: Option<OwnedFd>) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let call = frame.call;
        if let IpcRequest::Cancel { call: target } = frame.msg {
            self.cancel_call(target);
            self.send_raw(IpcFrame {
                call,
                msg: IpcResponse::Ok,
            });
            return;
        }
        let st = Arc::new(CallState {
            token: CancelToken::new(),
            finished: AtomicBool::new(false),
        });
        {
            let mut calls = lock(&self.calls);
            if calls.contains_key(&call) {
                drop(calls);
                tracing::warn!(call, "ipc: duplicate call id in flight");
                self.send_raw(Self::error_frame(
                    call,
                    ErrorCode::Protocol,
                    "duplicate call id",
                ));
                return;
            }
            calls.insert(call, Arc::clone(&st));
        }
        let pool = if is_transfer(&frame.msg) {
            &self.transfer
        } else {
            &self.interactive
        };
        let me = Arc::clone(self);
        let job_st = Arc::clone(&st);
        let job: Job = Box::new(move || me.run(frame, fd, &job_st));
        if pool.submit(job).is_err() {
            lock(&self.calls).remove(&call);
            self.send_for(
                &st,
                Self::error_frame(call, ErrorCode::Io, "engine busy: too many queued calls"),
                true,
            );
        }
    }

    fn run(
        self: &Arc<Self>,
        frame: IpcFrame<IpcRequest>,
        fd: Option<OwnedFd>,
        st: &Arc<CallState>,
    ) {
        let call = frame.call;
        if st.token.is_cancelled() || self.closed.load(Ordering::SeqCst) {
            return;
        }
        let mut reply = |f: IpcFrame<IpcResponse>| {
            if f.call != call {
                tracing::warn!(
                    call,
                    got = f.call,
                    "ipc: handler replied with a foreign call id"
                );
                return;
            }
            let last = !matches!(f.msg, IpcResponse::Progress { .. });
            self.send_for(st, f, last);
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            (self.handler)(frame, fd, &mut reply, &st.token)
        }));
        lock(&self.calls).remove(&call);
        match outcome {
            Ok(Disposition::Continue) => {
                // Guarantee exactly one final frame per call.
                self.send_for(
                    st,
                    Self::error_frame(call, ErrorCode::Io, "engine produced no reply"),
                    true,
                );
            }
            Ok(Disposition::CloseConnection) => self.close(),
            Err(panic) => {
                let what = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".into());
                tracing::error!(call, "ipc: handler panicked: {what}");
                self.send_for(
                    st,
                    Self::error_frame(call, ErrorCode::Io, &format!("internal error: {what}")),
                    true,
                );
            }
        }
    }

    /// Cancel one call: its token fires and its final reply becomes `Error { Cancelled }` now,
    /// without waiting for the engine to notice (the system expects a prompt completion).
    fn cancel_call(&self, target: u64) {
        let st = lock(&self.calls).remove(&target);
        if let Some(st) = st {
            st.token.cancel();
            let err = ProtoError::new(ErrorCode::Cancelled, "cancelled");
            let frame = IpcFrame {
                call: target,
                msg: IpcResponse::Error {
                    code: err.code,
                    msg: err.msg,
                    current: None,
                },
            };
            self.send_for(&st, frame, true);
        }
    }

    /// Connection gone (or dropped deliberately): cancel every call, stop sending, stop the
    /// pools. An invalidated connection means "cancel my calls", never "engine gone" (MQ-073).
    pub(crate) fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let calls: Vec<Arc<CallState>> = lock(&self.calls).drain().map(|(_, st)| st).collect();
        for st in calls {
            st.token.cancel();
        }
        self.interactive.close();
        self.transfer.close();
        if let Some(on_close) = &self.on_close {
            on_close();
        }
    }
}
