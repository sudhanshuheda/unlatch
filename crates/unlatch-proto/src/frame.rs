//! Session preamble + length-prefixed frames with optional LZ4 compression.
//!
//! ```text
//! preamble (once per direction, raw):  MAGIC[8] proto_min:u16le proto_max:u16le build_id[16]
//! frame:  u32 LE  len   — number of bytes that follow (flags + payload), 1..=MAX_FRAME
//!         u8      flags — FLAG_LZ4
//!         [u8]    payload (LZ4: u32 LE decompressed length ≤ MAX_FRAME, then LZ4 block)
//! ```
//! Remote login shells sometimes print junk (motd, `echo` in .bashrc) before `unlatchd` starts; the
//! client skips up to [`MAX_PREAMBLE_JUNK`] bytes before [`MAGIC`] and reports them.

use serde::{de::DeserializeOwned, Serialize};

pub const FLAG_LZ4: u8 = 0b0000_0001;
/// Hard cap on a single frame (defends against corrupt length prefixes).
pub const MAX_FRAME: usize = 64 * 1024 * 1024;
/// Payloads smaller than this are never compressed.
pub const COMPRESS_MIN: usize = 512;
/// Largest data payload in a bulk frame (ReadChunk / WriteChunk / snapshot / listing part).
pub const BULK_CHUNK: usize = 64 * 1024;

pub const MAGIC: [u8; 8] = *b"\0UNLATCH";
pub const PREAMBLE_LEN: usize = 8 + 2 + 2 + 16;
pub const MAX_PREAMBLE_JUNK: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Preamble {
    pub proto_min: u16,
    pub proto_max: u16,
    /// Build identity (first 16 bytes of the binary's sha256, or zeros in dev builds).
    pub build_id: [u8; 16],
}

impl Preamble {
    pub fn to_bytes(&self) -> [u8; PREAMBLE_LEN] {
        let mut b = [0u8; PREAMBLE_LEN];
        b[..8].copy_from_slice(&MAGIC);
        b[8..10].copy_from_slice(&self.proto_min.to_le_bytes());
        b[10..12].copy_from_slice(&self.proto_max.to_le_bytes());
        b[12..28].copy_from_slice(&self.build_id);
        b
    }

    fn from_tail(b: &[u8]) -> Preamble {
        let mut build_id = [0u8; 16];
        build_id.copy_from_slice(&b[4..20]);
        Preamble {
            proto_min: u16::from_le_bytes([b[0], b[1]]),
            proto_max: u16::from_le_bytes([b[2], b[3]]),
            build_id,
        }
    }
}

/// Highest protocol version both sides support, if any.
pub fn negotiate(a: &Preamble, b: &Preamble) -> Option<u16> {
    let hi = a.proto_max.min(b.proto_max);
    let lo = a.proto_min.max(b.proto_min);
    (hi >= lo).then_some(hi)
}

/// Blocking: scan for [`MAGIC`], skipping ≤ [`MAX_PREAMBLE_JUNK`] bytes. Returns the preamble and
/// the junk that preceded it (to surface as "remote shell printed: …").
pub fn read_preamble_blocking<R: std::io::Read>(
    r: &mut R,
) -> Result<(Preamble, Vec<u8>), FrameError> {
    let mut seen: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        r.read_exact(&mut byte)?;
        seen.push(byte[0]);
        if seen.len() >= MAGIC.len() && seen[seen.len() - MAGIC.len()..] == MAGIC {
            let junk = seen[..seen.len() - MAGIC.len()].to_vec();
            let mut tail = [0u8; PREAMBLE_LEN - 8];
            r.read_exact(&mut tail)?;
            return Ok((Preamble::from_tail(&tail), junk));
        }
        if seen.len() > MAX_PREAMBLE_JUNK + MAGIC.len() {
            return Err(FrameError::Decode(format!(
                "no unlatch preamble; remote printed: {}",
                String::from_utf8_lossy(&seen[..seen.len().min(512)])
            )));
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame too large: {0} bytes")]
    TooLarge(usize),
    #[error("decode: {0}")]
    Decode(String),
    #[error("encode: {0}")]
    Encode(String),
}

/// Serialize `msg` into a complete frame (header included).
/// Compresses with LZ4 when `compress` is set, the payload is ≥ [`COMPRESS_MIN`] and it saves ≥ 10%.
pub fn encode<T: Serialize>(msg: &T, compress: bool) -> Result<Vec<u8>, FrameError> {
    let raw = postcard::to_stdvec(msg).map_err(|e| FrameError::Encode(e.to_string()))?;
    let (flags, payload) = if compress && raw.len() >= COMPRESS_MIN {
        let c = lz4_flex::compress_prepend_size(&raw);
        if c.len() * 10 <= raw.len() * 9 {
            (FLAG_LZ4, c)
        } else {
            (0, raw)
        }
    } else {
        (0, raw)
    };
    let len = payload.len() + 1;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.push(flags);
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Decode a frame body (`flags` + payload, i.e. everything after the length prefix).
/// The LZ4 size prefix is bounded by [`MAX_FRAME`] *before* allocating.
pub fn decode_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, FrameError> {
    let (&flags, payload) = body
        .split_first()
        .ok_or_else(|| FrameError::Decode("empty frame".into()))?;
    if flags & FLAG_LZ4 != 0 {
        if payload.len() < 4 {
            return Err(FrameError::Decode("short lz4 frame".into()));
        }
        let n = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
        if n > MAX_FRAME {
            return Err(FrameError::TooLarge(n));
        }
        let mut raw = vec![0u8; n];
        let got = lz4_flex::block::decompress_into(&payload[4..], &mut raw)
            .map_err(|e| FrameError::Decode(e.to_string()))?;
        if got != n {
            return Err(FrameError::Decode(format!(
                "lz4 length {got} != declared {n}"
            )));
        }
        postcard::from_bytes(&raw).map_err(|e| FrameError::Decode(e.to_string()))
    } else {
        postcard::from_bytes(payload).map_err(|e| FrameError::Decode(e.to_string()))
    }
}

/// Blocking: read one frame body from `r`. Returns `Ok(None)` on clean EOF at a frame boundary.
pub fn read_body_blocking<R: std::io::Read>(r: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Blocking: read and decode one message. `Ok(None)` on clean EOF.
pub fn read_blocking<R: std::io::Read, T: DeserializeOwned>(
    r: &mut R,
) -> Result<Option<T>, FrameError> {
    match read_body_blocking(r)? {
        Some(b) => decode_body(&b).map(Some),
        None => Ok(None),
    }
}

/// Blocking: encode and write one message.
pub fn write_blocking<W: std::io::Write, T: Serialize>(
    w: &mut W,
    msg: &T,
    compress: bool,
) -> Result<(), FrameError> {
    let f = encode(msg, compress)?;
    w.write_all(&f)?;
    Ok(())
}

#[cfg(feature = "async")]
pub mod aio {
    //! Tokio helpers.
    use super::*;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    /// Read one frame body. `Ok(None)` on clean EOF at a frame boundary.
    pub async fn read_body<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
        let mut len = [0u8; 4];
        match r.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_le_bytes(len) as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(FrameError::TooLarge(len));
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        Ok(Some(body))
    }

    /// Scan for the preamble (see [`super::read_preamble_blocking`]).
    pub async fn read_preamble<R: AsyncRead + Unpin>(
        r: &mut R,
    ) -> Result<(Preamble, Vec<u8>), FrameError> {
        let mut seen: Vec<u8> = Vec::new();
        loop {
            let b = r.read_u8().await?;
            seen.push(b);
            if seen.len() >= MAGIC.len() && seen[seen.len() - MAGIC.len()..] == MAGIC {
                let junk = seen[..seen.len() - MAGIC.len()].to_vec();
                let mut tail = [0u8; PREAMBLE_LEN - 8];
                r.read_exact(&mut tail).await?;
                return Ok((Preamble::from_tail(&tail), junk));
            }
            if seen.len() > MAX_PREAMBLE_JUNK + MAGIC.len() {
                return Err(FrameError::Decode(format!(
                    "no unlatch preamble; remote printed: {}",
                    String::from_utf8_lossy(&seen[..seen.len().min(512)])
                )));
            }
        }
    }

    pub async fn read<R: AsyncRead + Unpin, T: DeserializeOwned>(
        r: &mut R,
    ) -> Result<Option<T>, FrameError> {
        match read_body(r).await? {
            Some(b) => decode_body(&b).map(Some),
            None => Ok(None),
        }
    }

    /// Write an already-encoded frame (from [`encode`]).
    pub async fn write_frame<W: AsyncWrite + Unpin>(
        w: &mut W,
        frame: &[u8],
    ) -> Result<(), FrameError> {
        w.write_all(frame).await?;
        Ok(())
    }

    pub async fn write<W: AsyncWrite + Unpin, T: Serialize>(
        w: &mut W,
        msg: &T,
        compress: bool,
    ) -> Result<(), FrameError> {
        let f = encode(msg, compress)?;
        w.write_all(&f).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ClientMsg, Request};

    #[test]
    fn roundtrip_plain_and_lz4() {
        for compress in [false, true] {
            let msg = ClientMsg::Request {
                req_id: 7,
                req: Request::Ping { nonce: 42 },
            };
            let f = encode(&msg, compress).unwrap();
            let body = read_body_blocking(&mut &f[..]).unwrap().unwrap();
            let back: ClientMsg = decode_body(&body).unwrap();
            assert_eq!(back, msg);
        }
        // big compressible payload actually compresses
        let big = ClientMsg::WriteChunk {
            req_id: 1,
            data: vec![b'a'; 100_000],
            last: true,
        };
        let f = encode(&big, true).unwrap();
        assert!(f.len() < 10_000);
        assert_eq!(f[4] & FLAG_LZ4, FLAG_LZ4);
        let back: ClientMsg = decode_body(&f[4..]).unwrap();
        assert_eq!(back, big);
    }

    #[test]
    fn lz4_bomb_rejected() {
        // flags=LZ4, declared decompressed size 0xFFFFFFFF
        let body = [FLAG_LZ4, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0];
        let r: Result<ClientMsg, _> = decode_body(&body);
        assert!(matches!(r, Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn preamble_after_junk() {
        let p = Preamble {
            proto_min: 1,
            proto_max: 3,
            build_id: [7; 16],
        };
        let mut buf = b"Welcome to Ubuntu\nlast login...\n".repeat(300);
        buf.extend_from_slice(&p.to_bytes());
        buf.extend_from_slice(b"rest");
        let mut r = &buf[..];
        let (got, junk) = read_preamble_blocking(&mut r).unwrap();
        assert_eq!(got, p);
        assert_eq!(junk.len(), buf.len() - PREAMBLE_LEN - 4);
        assert_eq!(r, b"rest");
        assert_eq!(
            negotiate(
                &p,
                &Preamble {
                    proto_min: 2,
                    proto_max: 5,
                    build_id: [0; 16]
                }
            ),
            Some(3)
        );
        assert_eq!(
            negotiate(
                &p,
                &Preamble {
                    proto_min: 4,
                    proto_max: 5,
                    build_id: [0; 16]
                }
            ),
            None
        );
    }

    #[test]
    fn eof_is_none() {
        let empty: &[u8] = &[];
        assert!(read_body_blocking(&mut &empty[..]).unwrap().is_none());
    }
}
