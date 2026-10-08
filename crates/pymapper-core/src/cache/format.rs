//! The container: a versioned header, 8-byte aligned sections and a section table, with a
//! checksum over everything after the header.
//!
//! ```text
//!   offset  size  field
//!   0       8     magic "PYMCACHE"
//!   8       4     format version (FORMAT_VERSION)
//!   12      4     kind (what the payload is, e.g. a facts segment)
//!   16      8     schema: hash of the payload layout + extractor version (see SCHEMA)
//!   24      8     key: hash of what the file caches (distribution, interpreter, ...)
//!   32      8     file length
//!   40      8     checksum of bytes 64..file length
//!   48      8     offset of the section table
//!   56      4     number of sections
//!   60      4     reserved (0)
//!   64      ...   sections, each starting 8-byte aligned (zero padding between)
//!   table   24*n  per section: id u32, reserved u32, offset u64, length u64
//! ```
//!
//! All integers are little-endian. Readers validate magic, version, kind, schema, key, length
//! and checksum before handing out any section, and view sections in place (`&[u32]` over the
//! mapping, no parse, no copy).

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::mmap::{Bytes, Mmap, Pod, as_bytes, cast};

pub(crate) const MAGIC: [u8; 8] = *b"PYMCACHE";
/// Bump on any change to the container layout.
pub(crate) const FORMAT_VERSION: u32 = 1;
pub(crate) const HEADER_LEN: usize = 64;
/// Table entry: id u32, reserved u32, offset u64, length u64 = 24 bytes.
const ENTRY_LEN: usize = 24;
/// Files below this are read into memory instead of mapped (a mapping costs a few syscalls
/// and a page-table entry per page; tiny distributions are the common case in a venv).
const MAP_THRESHOLD: u64 = 64 << 10;

/// Why a cache file was not used. Never fatal: the caller re-extracts.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
    #[error("not a pymapper cache file")]
    Magic,
    #[error("cache format version {0}, expected {FORMAT_VERSION}")]
    Version(u32),
    #[error("cache written by another pymapper version or extractor")]
    Schema,
    #[error("cache file is of another kind or key")]
    Key,
    #[error("checksum mismatch")]
    Checksum,
    #[error("corrupt cache file: {0}")]
    Corrupt(&'static str),
}

const K: u64 = 0xf135_7aea_2e62_a9c5;

#[inline(always)]
fn mix(h: u64, w: u64) -> u64 {
    (h.rotate_left(5) ^ w).wrapping_mul(K)
}

/// Streaming checksum of the payload: four independent Fx-style lanes over 32-byte blocks
/// (about 4x the throughput of one dependent multiply chain), the tail in 8-byte words,
/// folded with the length. Every step is a bijection of the state for a fixed input, so any
/// single changed word changes the result.
#[derive(Clone)]
pub(crate) struct Checksum {
    h: [u64; 4],
    pending: [u8; 32],
    npend: usize,
    len: u64,
}

impl Checksum {
    pub fn new() -> Checksum {
        Checksum { h: [1, 2, 3, 4], pending: [0; 32], npend: 0, len: 0 }
    }

    #[inline]
    fn block(&mut self, blk: &[u8; 32]) {
        for (l, w) in self.h.iter_mut().zip(blk.as_chunks::<8>().0) {
            *l = mix(*l, u64::from_le_bytes(*w));
        }
    }

    pub fn update(&mut self, mut b: &[u8]) {
        self.len += b.len() as u64;
        if self.npend > 0 {
            let take = (32 - self.npend).min(b.len());
            self.pending[self.npend..self.npend + take].copy_from_slice(&b[..take]);
            self.npend += take;
            b = &b[take..];
            if self.npend < 32 {
                return;
            }
            let blk = self.pending;
            self.block(&blk);
            self.npend = 0;
        }
        let (blocks, rest) = b.as_chunks::<32>();
        for blk in blocks {
            self.block(blk);
        }
        self.pending[..rest.len()].copy_from_slice(rest);
        self.npend = rest.len();
    }

    pub fn finish(&self) -> u64 {
        let mut t = 0u64;
        for (i, c) in self.pending[..self.npend].chunks(8).enumerate() {
            let mut a = [0u8; 8];
            a[..c.len()].copy_from_slice(c);
            t = mix(t ^ i as u64, u64::from_le_bytes(a));
        }
        let mut out = mix(self.len, t);
        for l in self.h {
            out = mix(out, l);
        }
        out ^ (out >> 29)
    }
}

/// [`Checksum`] of a whole buffer.
pub(crate) fn checksum(b: &[u8]) -> u64 {
    let mut c = Checksum::new();
    c.update(b);
    c.finish()
}

/// Streams a cache file to `out`: each section is built in a reused scratch buffer, then
/// written and checksummed; the table follows the last section and the header is written
/// last (seek to 0). Memory: the largest section, not the file.
pub(crate) struct Writer<W: Write + Seek> {
    out: W,
    sum: Checksum,
    scratch: Vec<u8>,
    table: Vec<(u32, u64, u64)>,
    /// Bytes written so far (the next section's offset before alignment).
    pos: u64,
    /// First write error; later writes are skipped.
    err: Option<io::Error>,
}

impl<W: Write + Seek> Writer<W> {
    pub fn new(mut out: W) -> Writer<W> {
        let err = out.write_all(&[0u8; HEADER_LEN]).err();
        Writer { out, sum: Checksum::new(), scratch: Vec::new(), table: Vec::new(), pos: HEADER_LEN as u64, err }
    }

    /// Writes `b` (part of the payload).
    fn emit(&mut self, b: &[u8]) {
        if self.err.is_none() {
            self.sum.update(b);
            self.err = self.out.write_all(b).err();
        }
        self.pos += b.len() as u64;
    }

    fn align(&mut self) {
        let pad = (self.pos.next_multiple_of(8) - self.pos) as usize;
        self.emit(&[0u8; 8][..pad]);
    }

    /// Appends a section of scalars.
    pub fn put<T: Pod>(&mut self, id: u32, s: &[T]) {
        self.align();
        self.table.push((id, self.pos, size_of_val(s) as u64));
        self.emit(as_bytes(s));
    }

    /// Appends a section produced by `f` into a scratch buffer (to convert while writing).
    pub fn put_with(&mut self, id: u32, f: impl FnOnce(&mut Vec<u8>)) {
        let mut buf = std::mem::take(&mut self.scratch);
        buf.clear();
        f(&mut buf);
        self.align();
        self.table.push((id, self.pos, buf.len() as u64));
        self.emit(&buf);
        self.scratch = buf;
    }

    /// Writes the table and the header; returns the sink and the file length.
    pub fn finish(mut self, kind: u32, schema: u64, key: u64) -> io::Result<(W, u64)> {
        self.align();
        let table_off = self.pos;
        let mut t = std::mem::take(&mut self.scratch);
        t.clear();
        for &(id, off, len) in &self.table {
            t.extend_from_slice(&id.to_le_bytes());
            t.extend_from_slice(&0u32.to_le_bytes());
            t.extend_from_slice(&off.to_le_bytes());
            t.extend_from_slice(&len.to_le_bytes());
        }
        self.emit(&t);
        if let Some(e) = self.err {
            return Err(e);
        }
        let total = self.pos;
        let mut h = [0u8; HEADER_LEN];
        h[0..8].copy_from_slice(&MAGIC);
        h[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        h[12..16].copy_from_slice(&kind.to_le_bytes());
        h[16..24].copy_from_slice(&schema.to_le_bytes());
        h[24..32].copy_from_slice(&key.to_le_bytes());
        h[32..40].copy_from_slice(&total.to_le_bytes());
        h[40..48].copy_from_slice(&self.sum.finish().to_le_bytes());
        h[48..56].copy_from_slice(&table_off.to_le_bytes());
        h[56..60].copy_from_slice(&(self.table.len() as u32).to_le_bytes());
        self.out.seek(SeekFrom::Start(0))?;
        self.out.write_all(&h)?;
        self.out.flush()?;
        Ok((self.out, total))
    }
}

/// Unique temp-file counter within the process (with the pid it makes temp names unique
/// across concurrent writers).
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Writes `path` atomically: `write` fills a temp file in the same directory, which is then
/// renamed over `path`. Readers see the old file or the new one, never a partial one. No
/// fsync: a file torn by a crash fails its checksum and is rebuilt.
pub(crate) fn write_atomic_with<R>(path: &Path, write: impl FnOnce(BufWriter<File>) -> io::Result<R>) -> io::Result<R> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut name = std::ffi::OsString::from(".tmp-");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!("-{}-{}", std::process::id(), TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
    let tmp: PathBuf = dir.join(name);
    let res = File::create(&tmp)
        .and_then(|f| write(BufWriter::with_capacity(1 << 20, f)))
        .and_then(|r| std::fs::rename(&tmp, path).map(|()| r));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// [`write_atomic_with`] for bytes in memory.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with(path, |mut f| {
        f.write_all(bytes)?;
        f.flush()
    })
}

#[inline]
fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

#[inline]
fn le_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// A validated cache file. Sections are views into the mapping.
pub(crate) struct Reader {
    bytes: Bytes,
    /// `(offset, len)` by section id; `(0, 0)` for absent ids.
    sections: Vec<(usize, usize)>,
}

impl Reader {
    /// Opens and validates `path` (see the module docs).
    pub fn open(path: &Path, kind: u32, schema: u64, key: u64) -> Result<Reader, CacheError> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        // Both branches use the same descriptor, so they see the same file even if it is
        // replaced (renamed over) meanwhile.
        let bytes = if len >= MAP_THRESHOLD { Bytes::Mapped(Mmap::map(&file)?) } else { Bytes::read_from(&mut file, len)? };
        Reader::from_bytes(bytes, kind, schema, key)
    }

    pub fn from_bytes(bytes: Bytes, kind: u32, schema: u64, key: u64) -> Result<Reader, CacheError> {
        let b = bytes.as_slice();
        if b.len() < HEADER_LEN || b[0..8] != MAGIC {
            return Err(CacheError::Magic);
        }
        let version = le_u32(b, 8);
        if version != FORMAT_VERSION {
            return Err(CacheError::Version(version));
        }
        if le_u64(b, 16) != schema {
            return Err(CacheError::Schema);
        }
        if le_u32(b, 12) != kind || le_u64(b, 24) != key {
            return Err(CacheError::Key);
        }
        if le_u64(b, 32) != b.len() as u64 {
            return Err(CacheError::Corrupt("length"));
        }
        if le_u64(b, 40) != checksum(&b[HEADER_LEN..]) {
            return Err(CacheError::Checksum);
        }
        let table_off = le_u64(b, 48);
        let n = le_u32(b, 56) as u64;
        let table_end = n.checked_mul(ENTRY_LEN as u64).and_then(|t| t.checked_add(table_off));
        if table_off < HEADER_LEN as u64 || table_end != Some(b.len() as u64) {
            return Err(CacheError::Corrupt("section table"));
        }
        let mut sections: Vec<(usize, usize)> = Vec::new();
        for i in 0..n as usize {
            let e = table_off as usize + i * ENTRY_LEN;
            let id = le_u32(b, e) as usize;
            let (off, len) = (le_u64(b, e + 8), le_u64(b, e + 16));
            let end = off.checked_add(len);
            if off < HEADER_LEN as u64 || !off.is_multiple_of(8) || end.is_none_or(|end| end > table_off) {
                return Err(CacheError::Corrupt("section bounds"));
            }
            if id >= 4096 {
                return Err(CacheError::Corrupt("section id"));
            }
            if sections.len() <= id {
                sections.resize(id + 1, (0, 0));
            }
            sections[id] = (off as usize, len as usize);
        }
        Ok(Reader { bytes, sections })
    }

    /// Bytes of section `id` (empty if absent).
    #[inline]
    pub fn bytes(&self, id: u32) -> &[u8] {
        let (off, len) = self.sections.get(id as usize).copied().unwrap_or((0, 0));
        &self.bytes.as_slice()[off..off + len]
    }

    /// Section `id` viewed as scalars, in place.
    #[inline]
    pub fn get<T: Pod>(&self, id: u32) -> Result<&[T], CacheError> {
        cast(self.bytes(id)).ok_or(CacheError::Corrupt("section size"))
    }

    /// Size of the file.
    pub fn len(&self) -> usize {
        self.bytes.as_slice().len()
    }
}
