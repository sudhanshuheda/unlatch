//! Scanning the server's stdout for bootstrap markers and the raw preamble, skipping shell junk.

use std::io;
use tokio::io::{AsyncRead, AsyncReadExt};
use unlatch_proto::frame::{Preamble, MAGIC, MAX_PREAMBLE_JUNK, PREAMBLE_LEN};

/// Longest marker line accepted (markers are short; this bounds buffering of a bogus prefix).
const MAX_MARKER_LINE: usize = 4096;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    /// Text after `UNLATCH-<nonce>-` up to (excluding) the newline.
    Marker(String),
    Preamble(Preamble),
    Eof,
}

pub(crate) struct Scanner<R> {
    r: R,
    buf: Vec<u8>,
    /// Bytes printed before the preamble that were not markers ("remote shell printed: …").
    pub junk: Vec<u8>,
    prefix: Option<Vec<u8>>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn preamble_from(b: &[u8]) -> Preamble {
    let mut build_id = [0u8; 16];
    build_id.copy_from_slice(&b[12..28]);
    Preamble {
        proto_min: u16::from_le_bytes([b[8], b[9]]),
        proto_max: u16::from_le_bytes([b[10], b[11]]),
        build_id,
    }
}

impl<R: AsyncRead + Unpin> Scanner<R> {
    /// `marker_prefix`: `UNLATCH-<nonce>-` for bootstrap sessions, `None` to look only for the
    /// preamble.
    pub(crate) fn new(r: R, marker_prefix: Option<String>) -> Self {
        Self {
            r,
            buf: Vec::new(),
            junk: Vec::new(),
            prefix: marker_prefix.map(String::into_bytes),
        }
    }

    /// Next marker or the preamble. Errors with `InvalidData` after more than
    /// [`MAX_PREAMBLE_JUNK`] bytes of junk.
    pub(crate) async fn next(&mut self) -> io::Result<Seen> {
        loop {
            if let Some(seen) = self.try_parse() {
                return Ok(seen);
            }
            if self.junk.len() + self.buf.len() > MAX_PREAMBLE_JUNK + PREAMBLE_LEN + MAX_MARKER_LINE
            {
                let mut shown = self.junk.clone();
                shown.extend_from_slice(&self.buf);
                shown.truncate(512);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "no unlatch preamble; remote shell printed: {}",
                        String::from_utf8_lossy(&shown)
                    ),
                ));
            }
            let mut chunk = [0u8; 8192];
            let n = self.r.read(&mut chunk).await?;
            if n == 0 {
                self.junk.append(&mut self.buf);
                return Ok(Seen::Eof);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn try_parse(&mut self) -> Option<Seen> {
        let magic = find(&self.buf, &MAGIC);
        let marker = self.prefix.as_ref().and_then(|p| {
            let at = find(&self.buf, p)?;
            let nl = self.buf[at..].iter().position(|&b| b == b'\n')? + at;
            Some((at, p.len(), nl))
        });
        match (magic, marker) {
            (Some(m), mk) if !matches!(mk, Some((at, _, _)) if at < m) => {
                if self.buf.len() < m + PREAMBLE_LEN {
                    return None;
                }
                self.junk.extend_from_slice(&self.buf[..m]);
                let p = preamble_from(&self.buf[m..m + PREAMBLE_LEN]);
                self.buf.drain(..m + PREAMBLE_LEN);
                Some(Seen::Preamble(p))
            }
            (_, Some((at, plen, nl))) => {
                self.junk.extend_from_slice(&self.buf[..at]);
                let line = String::from_utf8_lossy(&self.buf[at + plen..nl])
                    .trim_end_matches('\r')
                    .to_string();
                self.buf.drain(..=nl);
                Some(Seen::Marker(line))
            }
            _ => None,
        }
    }

    /// Bytes read past the preamble (the start of the frame stream) and the underlying reader.
    pub(crate) fn into_parts(self) -> (Vec<u8>, R, Vec<u8>) {
        (self.buf, self.r, self.junk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pre() -> Preamble {
        Preamble {
            proto_min: 1,
            proto_max: 2,
            build_id: [9; 16],
        }
    }

    async fn scan_all(input: Vec<u8>, prefix: Option<&str>) -> (Vec<Seen>, Vec<u8>, Vec<u8>) {
        let mut s = Scanner::new(&input[..], prefix.map(str::to_string));
        let mut out = Vec::new();
        loop {
            match s.next().await.expect("scan") {
                Seen::Eof => break,
                Seen::Preamble(p) => {
                    out.push(Seen::Preamble(p));
                    break;
                }
                m => out.push(m),
            }
        }
        let (rest, _, junk) = s.into_parts();
        (out, rest, junk)
    }

    #[tokio::test]
    async fn markers_junk_and_preamble() {
        let mut input =
            b"motd line\nUNLATCH-abc-NEED x86_64\nmore junk UNLATCH-zzz-FAKE\n".to_vec();
        input.extend_from_slice(b"UNLATCH-abc-EXEC\r\n");
        input.extend_from_slice(&pre().to_bytes());
        input.extend_from_slice(b"frames");
        let (seen, rest, junk) = scan_all(input, Some("UNLATCH-abc-")).await;
        assert_eq!(
            seen,
            vec![
                Seen::Marker("NEED x86_64".into()),
                Seen::Marker("EXEC".into()),
                Seen::Preamble(pre())
            ]
        );
        assert_eq!(rest, b"frames");
        assert_eq!(junk, b"motd line\nmore junk UNLATCH-zzz-FAKE\n");
    }

    #[tokio::test]
    async fn preamble_after_10k_of_junk_without_markers() {
        let mut input = vec![b'x'; 10 * 1024];
        input.extend_from_slice(&pre().to_bytes());
        let (seen, rest, junk) = scan_all(input, None).await;
        assert_eq!(seen, vec![Seen::Preamble(pre())]);
        assert!(rest.is_empty());
        assert_eq!(junk.len(), 10 * 1024);
    }

    #[tokio::test]
    async fn too_much_junk_is_an_error() {
        let input = vec![b'y'; MAX_PREAMBLE_JUNK + 64 * 1024];
        let mut s = Scanner::new(&input[..], None);
        let e = s.next().await.expect_err("junk");
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("remote shell printed: yyy"));
    }

    #[tokio::test]
    async fn eof_before_preamble_keeps_the_junk() {
        let (seen, _, junk) = scan_all(b"bash: unlatchd: not found\n".to_vec(), None).await;
        assert!(seen.is_empty());
        assert_eq!(junk, b"bash: unlatchd: not found\n");
    }

    #[tokio::test]
    async fn byte_at_a_time_reader() {
        // A reader that returns one byte per read exercises every partial-buffer path.
        struct Trickle(Vec<u8>, usize);
        impl AsyncRead for Trickle {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<io::Result<()>> {
                if self.1 < self.0.len() {
                    let b = self.0[self.1];
                    self.1 += 1;
                    buf.put_slice(&[b]);
                }
                std::task::Poll::Ready(Ok(()))
            }
        }
        let mut input = b"junk\nUNLATCH-n-EXEC\n".to_vec();
        input.extend_from_slice(&pre().to_bytes());
        input.extend_from_slice(b"tail");
        let mut s = Scanner::new(Trickle(input, 0), Some("UNLATCH-n-".into()));
        assert_eq!(s.next().await.expect("m"), Seen::Marker("EXEC".into()));
        assert_eq!(s.next().await.expect("p"), Seen::Preamble(pre()));
        let (rest, mut r, junk) = s.into_parts();
        let mut tail = rest;
        r.read_to_end(&mut tail).await.expect("rest");
        assert_eq!(tail, b"tail");
        assert_eq!(junk, b"junk\n");
    }
}
