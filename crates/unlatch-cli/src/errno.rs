//! Engine error codes → POSIX errno values (FUSE replies).

use unlatch_proto::{ErrorCode, ProtoError};

pub fn errno_of(code: ErrorCode) -> i32 {
    use ErrorCode::*;
    match code {
        NotFound => libc::ENOENT,
        Exists => libc::EEXIST,
        NotDir => libc::ENOTDIR,
        IsDir => libc::EISDIR,
        NotEmpty => libc::ENOTEMPTY,
        Permission => libc::EACCES,
        // The item changed on the VM between our view and the op. EBUSY tells the caller to
        // look again rather than implying the file is gone.
        VersionMismatch | DeletionRejected => libc::EBUSY,
        InvalidName => libc::EINVAL,
        NoSpace => libc::ENOSPC,
        // "Host is down" is the most honest message `ls`/editors can print for these.
        Offline | NeedsUser => libc::EHOSTDOWN,
        AnchorExpired | Io | CannotSync => libc::EIO,
        Unsupported => libc::ENOTSUP,
        Timeout => libc::ETIMEDOUT,
        Protocol => libc::EPROTO,
        IndexChanged | RootReplaced => libc::ESTALE,
        ExcludedFromSync => libc::EPERM,
        Cancelled => libc::EINTR,
    }
}

pub fn errno(e: &ProtoError) -> i32 {
    errno_of(e.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_codes() {
        assert_eq!(errno_of(ErrorCode::NotFound), libc::ENOENT);
        assert_eq!(errno_of(ErrorCode::NotEmpty), libc::ENOTEMPTY);
        assert_eq!(errno_of(ErrorCode::Offline), libc::EHOSTDOWN);
        assert_eq!(
            errno(&ProtoError::new(ErrorCode::Exists, "x")),
            libc::EEXIST
        );
    }
}
