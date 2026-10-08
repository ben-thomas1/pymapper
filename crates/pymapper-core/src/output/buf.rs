//! Buffered output sink: one reused buffer, large writes, graceful on a closed pipe.

use std::fmt;
use std::io::{self, Write};

/// Bytes buffered before a write to the underlying sink.
const CAPACITY: usize = 64 * 1024;

/// How writing ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// Everything was written and flushed.
    Written,
    /// The reader closed the pipe (EPIPE); the rest of the output was discarded.
    Closed,
}

/// Whether an I/O error means the reader went away (`pymapper check | head`).
#[inline]
pub fn is_broken_pipe(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::BrokenPipe
}

/// A write buffer over a sink. Writers append freely; the buffer is handed to the sink in
/// chunks of about 64 KiB, so a run costs one allocation however many lines it writes. The
/// first sink error is kept and all later output dropped; [`Out::finish`] reports it (a broken
/// pipe as [`Outcome::Closed`], anything else as an error).
pub struct Out<'w> {
    buf: Vec<u8>,
    sink: &'w mut dyn Write,
    err: Option<io::Error>,
}

impl<'w> Out<'w> {
    /// A buffer over `sink`.
    pub fn new(sink: &'w mut dyn Write) -> Out<'w> {
        Out { buf: Vec::with_capacity(CAPACITY), sink, err: None }
    }

    /// Appends bytes.
    #[inline]
    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
        self.maybe_flush();
    }

    /// Appends a string.
    #[inline]
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    /// Appends one byte.
    #[inline]
    pub fn byte(&mut self, b: u8) {
        self.buf.push(b);
        self.maybe_flush();
    }

    /// Appends a decimal number (no `format!`).
    #[inline]
    pub fn u32(&mut self, v: u32) {
        let mut digits = [0u8; 10];
        let s = fmt_u32(v, &mut digits);
        self.bytes(s);
    }

    /// The buffer, for writers that escape into it directly. Call [`Out::maybe_flush`] after
    /// appending.
    #[inline]
    pub fn buf(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }

    /// Hands the buffer to the sink once it holds a chunk's worth.
    #[inline]
    pub fn maybe_flush(&mut self) {
        if self.buf.len() >= CAPACITY {
            self.drain();
        }
    }

    fn drain(&mut self) {
        if self.err.is_none()
            && let Err(e) = self.sink.write_all(&self.buf)
        {
            self.err = Some(e);
        }
        self.buf.clear();
    }

    /// Writes what is left and flushes the sink.
    pub fn finish(mut self) -> io::Result<Outcome> {
        self.drain();
        if self.err.is_none()
            && let Err(e) = self.sink.flush()
        {
            self.err = Some(e);
        }
        match self.err {
            None => Ok(Outcome::Written),
            Some(e) if is_broken_pipe(&e) => Ok(Outcome::Closed),
            Some(e) => Err(e),
        }
    }
}

impl fmt::Write for Out<'_> {
    #[inline]
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.bytes(s.as_bytes());
        Ok(())
    }
}

/// Formats `v` in decimal into the tail of `digits`; returns the digits.
#[inline]
pub(crate) fn fmt_u32(mut v: u32, digits: &mut [u8; 10]) -> &[u8] {
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &digits[i..]
}

/// Appends `v` in decimal to `out`.
#[inline]
pub(crate) fn push_u32(out: &mut Vec<u8>, v: u32) {
    let mut digits = [0u8; 10];
    out.extend_from_slice(fmt_u32(v, &mut digits));
}

/// Appends `v` as 16 lower-case hex digits to `out`.
pub(crate) fn push_hex64(out: &mut Vec<u8>, v: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for k in (0..16).rev() {
        out.push(HEX[((v >> (k * 4)) & 0xf) as usize]);
    }
}
