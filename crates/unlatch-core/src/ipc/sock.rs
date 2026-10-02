//! Unix-socket plumbing for the IPC transport: frame writes that never raise `SIGPIPE`,
//! `SCM_RIGHTS` fd passing, a frame reader that pairs received fds with the frame they were
//! attached to, and peer-uid checks.
//!
//! std's ancillary-data API is unstable, so this talks to `libc` directly. Every `unsafe` block
//! is a plain syscall on a descriptor we own for the duration of the call.

use std::collections::VecDeque;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use unlatch_proto::frame::MAX_FRAME;

/// Fds a single `recvmsg` can carry before the kernel truncates (we expect at most one).
const MAX_FDS_PER_READ: usize = 8;

#[cfg(any(target_os = "linux", target_os = "android"))]
const SEND_FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const SEND_FLAGS: libc::c_int = 0;

#[cfg(any(target_os = "linux", target_os = "android"))]
const RECV_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const RECV_FLAGS: libc::c_int = 0;

/// Aligned control-message buffer (cmsg headers must be `cmsghdr`-aligned).
#[repr(C, align(8))]
struct CmsgBuf([u8; 128]);

/// Prepare a freshly created/accepted socket: no `SIGPIPE` on write to a dead peer. The library
/// is embedded in a macOS app that does not ignore `SIGPIPE`, so a vanished extension must never
/// kill the engine.
pub(crate) fn prepare(stream: &UnixStream) -> io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let one: libc::c_int = 1;
        // SAFETY: valid fd, pointer to a c_int of the declared length.
        let r = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&one as *const libc::c_int).cast(),
                mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    let _ = stream;
    Ok(())
}

/// Write all of `data`, attaching `fd` (if any) to the first byte via `SCM_RIGHTS`.
pub(crate) fn send_frame(
    sock: &UnixStream,
    data: &[u8],
    fd: Option<BorrowedFd<'_>>,
) -> io::Result<()> {
    let mut off = 0;
    if let Some(fd) = fd {
        off = sendmsg_with_fd(sock.as_raw_fd(), data, fd.as_raw_fd())?;
    }
    while off < data.len() {
        // SAFETY: pointer/len describe a live slice.
        let r = unsafe {
            libc::send(
                sock.as_raw_fd(),
                data[off..].as_ptr().cast(),
                data.len() - off,
                SEND_FLAGS,
            )
        };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if r == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        off += r as usize;
    }
    Ok(())
}

fn sendmsg_with_fd(sock: RawFd, data: &[u8], fd: RawFd) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let mut cbuf = CmsgBuf([0; 128]);
    // SAFETY: CMSG_SPACE is a pure computation.
    let space = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) } as usize;
    debug_assert!(space <= cbuf.0.len());
    // SAFETY: zeroed msghdr is a valid "empty" value; we fill in the fields we use.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.0.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: msg_control points at `space` bytes of aligned storage; CMSG_FIRSTHDR is non-null
    // because msg_controllen ≥ sizeof(cmsghdr).
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd);
    }
    loop {
        // SAFETY: msg is fully initialised and its buffers outlive the call.
        let r = unsafe { libc::sendmsg(sock, &msg, SEND_FLAGS) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if r == 0 && !data.is_empty() {
            return Err(io::ErrorKind::WriteZero.into());
        }
        return Ok(r as usize);
    }
}

/// `recvmsg` into `buf`, pushing any received fds onto `fds`. `Ok(0)` = EOF.
fn recv_with_fds(sock: RawFd, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cbuf = CmsgBuf([0; 128]);
    // SAFETY: pure computation.
    let space =
        unsafe { libc::CMSG_SPACE((MAX_FDS_PER_READ * mem::size_of::<RawFd>()) as u32) } as usize;
    let space = space.min(cbuf.0.len());
    // SAFETY: as in sendmsg_with_fd.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.0.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    let n = loop {
        // SAFETY: msg describes live, writable buffers.
        let r = unsafe { libc::recvmsg(sock, &mut msg, RECV_FLAGS) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        break r as usize;
    };
    // SAFETY: iterate the control messages the kernel wrote; CMSG_* respect msg_controllen.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c);
                let payload = (*c).cmsg_len as usize - (data as usize - c as usize);
                for i in 0..payload / mem::size_of::<RawFd>() {
                    let raw = std::ptr::read_unaligned(data.cast::<RawFd>().add(i));
                    let owned = OwnedFd::from_raw_fd(raw);
                    #[cfg(not(any(target_os = "linux", target_os = "android")))]
                    libc::fcntl(owned.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
                    fds.push(owned);
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        tracing::warn!("ipc: control data truncated (peer sent more than {MAX_FDS_PER_READ} fds)");
    }
    Ok(n)
}

/// Fds that arrived with the bytes `[start, end)` of the stream.
struct FdArrival {
    fd: OwnedFd,
    start: u64,
    end: u64,
}

/// Reads length-prefixed frames from a unix stream and pairs every received fd with its frame.
///
/// The sender attaches an fd to the first byte of a frame (one `sendmsg`). Linux may glue earlier
/// bytes into the same `recvmsg`, but always stops right after the skb that carried fds, so the
/// fd belongs to the *last* frame that starts inside that read's byte range: the frame starting
/// at `S` in `[start, end)` whose end reaches `end`. This keeps an fd from ever being handed to
/// the wrong call, even when a buggy peer sends an fd with a frame that did not declare one.
pub(crate) struct FrameReader {
    sock: UnixStream,
    buf: Vec<u8>,
    /// Valid bytes are `buf[head..tail]`; `pos` = absolute stream offset of `buf[head]`.
    head: usize,
    tail: usize,
    pos: u64,
    arrivals: VecDeque<FdArrival>,
}

impl FrameReader {
    pub(crate) fn new(sock: UnixStream) -> Self {
        Self {
            sock,
            buf: vec![0; 64 * 1024],
            head: 0,
            tail: 0,
            pos: 0,
            arrivals: VecDeque::new(),
        }
    }

    /// Ensure `n` contiguous bytes are buffered. `Ok(false)` on EOF (clean only if nothing was
    /// buffered; a truncated frame is an error).
    fn fill(&mut self, n: usize) -> io::Result<bool> {
        while self.tail - self.head < n {
            if self.head > 0 {
                self.buf.copy_within(self.head..self.tail, 0);
                self.tail -= self.head;
                self.head = 0;
            }
            if self.buf.len() < n {
                self.buf.resize(n.next_power_of_two(), 0);
            }
            let start = self.pos + self.tail as u64;
            let mut fds = Vec::new();
            let got = recv_with_fds(self.sock.as_raw_fd(), &mut self.buf[self.tail..], &mut fds)?;
            let end = start + got as u64;
            for fd in fds {
                self.arrivals.push_back(FdArrival {
                    fd,
                    start,
                    end: end.max(start + 1),
                });
            }
            if got == 0 {
                if self.tail == self.head {
                    return Ok(false);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "ipc: truncated frame",
                ));
            }
            self.tail += got;
        }
        Ok(true)
    }

    /// Next frame body (`flags + payload`) and the fd attached to it. `Ok(None)` on clean EOF.
    pub(crate) fn next(&mut self) -> io::Result<Option<(Vec<u8>, Option<OwnedFd>)>> {
        if !self.fill(4)? {
            return Ok(None);
        }
        let h = self.head;
        let len = u32::from_le_bytes([
            self.buf[h],
            self.buf[h + 1],
            self.buf[h + 2],
            self.buf[h + 3],
        ]) as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ipc: bad frame length {len}"),
            ));
        }
        if !self.fill(4 + len)? {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "ipc: truncated frame",
            ));
        }
        let frame_start = self.pos;
        let frame_end = frame_start + 4 + len as u64;
        let body = self.buf[self.head + 4..self.head + 4 + len].to_vec();
        self.head += 4 + len;
        self.pos = frame_end;

        let mut fd = None;
        let mut keep = VecDeque::with_capacity(self.arrivals.len());
        for a in self.arrivals.drain(..) {
            if fd.is_none() && claims(&a, frame_start, frame_end) {
                fd = Some(a.fd);
            } else if a.end > frame_end {
                keep.push_back(a);
            } else {
                // Belonged to this or an earlier frame but was not claimed: close it.
                tracing::debug!("ipc: dropping unclaimed fd");
            }
        }
        self.arrivals = keep;
        Ok(Some((body, fd)))
    }
}

/// Does the frame `[frame_start, frame_end)` own the fds that arrived with bytes `a.start..a.end`?
#[cfg(any(target_os = "linux", target_os = "android"))]
fn claims(a: &FdArrival, frame_start: u64, frame_end: u64) -> bool {
    // Linux glues earlier skbs into the read but stops after the one carrying fds: the owner is
    // the last frame starting inside the read.
    a.start <= frame_start && frame_start < a.end && frame_end >= a.end
}

/// BSD-derived kernels start a read at a control-message boundary, so fds arrive with the first
/// byte of their frame.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn claims(a: &FdArrival, frame_start: u64, _frame_end: u64) -> bool {
    a.start <= frame_start && frame_start < a.end
}

/// Effective uid of the connected peer.
pub(crate) fn peer_uid(sock: &UnixStream) -> io::Result<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: zeroed ucred is valid; getsockopt writes at most `len` bytes.
        let mut cred: libc::ucred = unsafe { mem::zeroed() };
        let mut len = mem::size_of::<libc::ucred>() as libc::socklen_t;
        let r = unsafe {
            libc::getsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: out-pointers to live locals.
        let r = unsafe { libc::getpeereid(sock.as_raw_fd(), &mut uid, &mut gid) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }
}

pub(crate) fn euid() -> u32 {
    // SAFETY: geteuid cannot fail.
    unsafe { libc::geteuid() }
}
