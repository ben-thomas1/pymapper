//! Baselines: record today's findings, then report only new ones.
//!
//! # Keys
//!
//! A finding's key is its rule plus:
//!
//! - `id`: the stable qualified id of its symbol (`pkg.mod:Class.method`), else its file's
//!   path relative to the project root, else empty (project-level findings);
//! - `detail`: its stable message arguments in order: symbols (qualified ids; except the
//!   finding's own symbol, already the id), names, files (paths) and contracts (names). Numbers (metric values, limits, line numbers) are left out,
//!   so a function whose complexity goes from 12 to 13 stays baselined, and nothing depends on
//!   line numbers or byte offsets.
//!
//! Several findings may share a key (e.g. two identical unused imports in one module); the
//! baseline stores a count per key. Filtering drops up to `count` findings per key, the first
//! ones in output order, and reports the rest.
//!
//! # File format (version 1)
//!
//! ```json
//! {
//!   "version": 1,
//!   "tool": "pymapper",
//!   "entries": [
//!     {"code": "PM101", "id": "pkg.mod:f", "count": 1},
//!     {"code": "PM601", "id": "pkg.mod", "detail": ["yaml", "PyYAML"], "count": 1}
//!   ]
//! }
//! ```
//!
//! Entries are sorted by code, then id, then detail, so re-saving an unchanged project gives
//! an identical file. `detail` is omitted when empty; `count` defaults to 1. Unknown fields are
//! ignored (forward compatibility); a different `version` is an error.
//!
//! # Cost
//!
//! Loading is O(file size) with one byte pool for all keys (no allocation per entry); filtering
//! is one key rendering (into a reused buffer) and one hash lookup per finding.

use std::cmp::Ordering;
use std::fmt;

use super::{Report, lossy_tail};
use super::buf::{Out, push_u32};
use super::json::push_str;
use super::jsonread::{Reader, SyntaxError};
use crate::intern::{FxHashMap, hash_bytes};
use crate::rules::{Arg, FileId, Finding, SymbolId};

/// Version of the baseline file format.
pub const BASELINE_VERSION: u32 = 1;

/// Why a baseline file could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BaselineError {
    /// Not valid JSON.
    #[error("invalid JSON at byte {offset}: {what}")]
    Syntax { offset: usize, what: &'static str },
    /// Valid JSON with the wrong shape.
    #[error("not a pymapper baseline: {0}")]
    Schema(&'static str),
    /// A format version this build does not read.
    #[error("unsupported baseline version {0} (this pymapper reads version {BASELINE_VERSION})")]
    Version(u32),
}

impl From<SyntaxError> for BaselineError {
    fn from(e: SyntaxError) -> BaselineError {
        BaselineError::Syntax { offset: e.offset, what: e.what }
    }
}

/// Builds finding keys into reused buffers.
///
/// Encoding: the rule code (2 bytes, little endian), then each part (id first, then the detail
/// arguments) as a 4-byte little-endian length and its bytes. Length prefixes make the
/// encoding unambiguous whatever bytes the parts contain.
pub(crate) struct KeyBuf {
    key: Vec<u8>,
    part: Vec<u8>,
    text: String,
}

fn push_part(key: &mut Vec<u8>, part: &[u8]) {
    key.extend_from_slice(&(part.len() as u32).to_le_bytes());
    key.extend_from_slice(part);
}

impl KeyBuf {
    pub fn new() -> KeyBuf {
        KeyBuf {
            key: Vec::with_capacity(super::SCRATCH),
            part: Vec::with_capacity(super::SCRATCH),
            text: String::with_capacity(super::SCRATCH),
        }
    }

    /// The key of `f` (valid until the next call).
    pub fn build(&mut self, r: &Report<'_>, f: &Finding) -> &[u8] {
        let ctx = r.ctx();
        self.key.clear();
        self.key.extend_from_slice(&f.rule.code().to_le_bytes());
        self.part.clear();
        if f.symbol != SymbolId::NONE {
            ctx.write_symbol_id(f.symbol, &mut self.part);
            lossy_tail(&mut self.part, 0);
        } else if f.loc.file != FileId::NONE {
            ctx.write_root_path(f.loc.file, &mut self.part);
            lossy_tail(&mut self.part, 0);
        }
        push_part(&mut self.key, &self.part);
        for &a in r.findings().args(f) {
            self.text.clear();
            let res = match a {
                // Wording (`never used` / `only used by unused code`) is not identity.
                Arg::U32(_) | Arg::F32(_) | Arg::Phrase(_) => continue,
                // The finding's own symbol is already the id.
                Arg::Symbol(s) if s == f.symbol => continue,
                Arg::Symbol(s) => {
                    self.part.clear();
                    ctx.write_symbol_id(s, &mut self.part);
                    lossy_tail(&mut self.part, 0);
                    push_part(&mut self.key, &self.part);
                    continue;
                }
                Arg::Name(n) => ctx.write_name(n, &mut self.text),
                // Root-relative, so keys do not depend on the working directory.
                Arg::File(file) => {
                    self.part.clear();
                    ctx.write_root_path(file, &mut self.part);
                    lossy_tail(&mut self.part, 0);
                    push_part(&mut self.key, &self.part);
                    continue;
                }
                Arg::Contract(c) => ctx.write_contract(c, &mut self.text),
            };
            if res.is_err() {
                self.text.clear();
            }
            push_part(&mut self.key, self.text.as_bytes());
        }
        &self.key
    }

    /// Hash of the key of `f`.
    pub fn hash(&mut self, r: &Report<'_>, f: &Finding) -> u64 {
        hash_bytes(self.build(r, f))
    }
}

/// Splits an encoded key into the code and its parts.
fn decode(key: &[u8]) -> (u16, Parts<'_>) {
    (u16::from_le_bytes([key[0], key[1]]), Parts { rest: &key[2..] })
}

struct Parts<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Parts<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        let (len, rest) = self.rest.split_first_chunk::<4>()?;
        let n = u32::from_le_bytes(*len) as usize;
        let (part, rest) = rest.split_at(n);
        self.rest = rest;
        Some(part)
    }
}

/// Orders keys by code, then part by part (bytes).
fn cmp_keys(a: &[u8], b: &[u8]) -> Ordering {
    let (ca, pa) = decode(a);
    let (cb, pb) = decode(b);
    ca.cmp(&cb).then_with(|| pa.cmp(pb))
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    start: u32,
    len: u32,
    count: u32,
}

/// A set of finding keys with counts (see the module docs).
#[derive(Clone, Debug, Default)]
pub struct Baseline {
    pool: Vec<u8>,
    entries: Vec<Entry>,
    /// Key hash -> first entry with that hash; `next` chains entries whose hashes collide.
    heads: FxHashMap<u64, u32>,
    next: Vec<u32>,
}

const END: u32 = u32::MAX;

impl Baseline {
    /// An empty baseline.
    pub fn new() -> Baseline {
        Baseline::default()
    }

    /// Number of distinct keys.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no keys.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Sum of all counts.
    pub fn total(&self) -> u64 {
        self.entries.iter().map(|e| u64::from(e.count)).sum()
    }

    fn key(&self, e: &Entry) -> &[u8] {
        &self.pool[e.start as usize..(e.start + e.len) as usize]
    }

    fn find(&self, hash: u64, key: &[u8]) -> Option<usize> {
        let mut i = *self.heads.get(&hash)?;
        while i != END {
            if self.key(&self.entries[i as usize]) == key {
                return Some(i as usize);
            }
            i = self.next[i as usize];
        }
        None
    }

    /// Adds `count` occurrences of an encoded key.
    fn add(&mut self, key: &[u8], count: u32) {
        let hash = hash_bytes(key);
        if let Some(i) = self.find(hash, key) {
            let e = &mut self.entries[i];
            e.count = e.count.saturating_add(count);
            return;
        }
        let idx = u32::try_from(self.entries.len()).expect("more than u32::MAX baseline entries");
        let start = u32::try_from(self.pool.len()).expect("baseline keys exceed 4 GiB");
        self.pool.extend_from_slice(key);
        self.entries.push(Entry { start, len: key.len() as u32, count });
        let head = self.heads.entry(hash).or_insert(END);
        self.next.push(*head);
        *head = idx;
    }

    /// The baseline of every finding in `r`.
    pub fn from_report(r: &Report<'_>) -> Baseline {
        let mut b = Baseline::new();
        let mut kb = KeyBuf::new();
        for f in r.iter() {
            b.add(kb.build(r, f), 1);
        }
        b
    }

    /// Drops baselined findings from `r` (see [`Report::apply_baseline`]).
    pub(super) fn filter(&self, r: &mut Report<'_>) -> u32 {
        if self.entries.is_empty() {
            return 0;
        }
        let mut left: Vec<u32> = self.entries.iter().map(|e| e.count).collect();
        let mut kb = KeyBuf::new();
        let mut order = std::mem::take(&mut r.order);
        let before = order.len();
        let items = r.findings().as_slice();
        order.retain(|&i| {
            let key = kb.build(r, &items[i as usize]);
            match self.find(hash_bytes(key), key) {
                Some(e) if left[e] > 0 => {
                    left[e] -= 1;
                    false
                }
                _ => true,
            }
        });
        let dropped = (before - order.len()) as u32;
        r.order = order;
        dropped
    }
}

impl Baseline {
    /// Reads a baseline file (see the module docs for the format).
    pub fn parse(text: &[u8]) -> Result<Baseline, BaselineError> {
        let mut rd = Reader::new(text);
        let mut b = Baseline::new();
        let mut key = Vec::new();
        let mut version = None;
        let mut entries = false;
        let mut e = EntryScratch::default();
        rd.begin_object()?;
        let mut first = true;
        while rd.next_key(first, &mut key)? {
            first = false;
            match key.as_slice() {
                b"version" => {
                    let v = rd.u32()?;
                    if v != BASELINE_VERSION {
                        return Err(BaselineError::Version(v));
                    }
                    version = Some(v);
                }
                b"entries" => {
                    entries = true;
                    rd.begin_array()?;
                    let mut first_item = true;
                    while rd.next_item(first_item)? {
                        first_item = false;
                        let count = e.parse(&mut rd, &mut key)?;
                        b.add(&e.key, count);
                    }
                }
                _ => rd.skip()?,
            }
        }
        rd.end()?;
        if version.is_none() {
            return Err(BaselineError::Schema("missing `version`"));
        }
        if !entries {
            return Err(BaselineError::Schema("missing `entries`"));
        }
        Ok(b)
    }
}

/// Reused buffers for parsing one entry.
#[derive(Default)]
struct EntryScratch {
    key: Vec<u8>,
    id: Vec<u8>,
    detail: Vec<u8>,
    s: Vec<u8>,
}

impl EntryScratch {
    /// Parses `{"code": ..., "id": ..., "detail": [...], "count": N}` into `self.key`; returns
    /// the count.
    fn parse(&mut self, rd: &mut Reader<'_>, field: &mut Vec<u8>) -> Result<u32, BaselineError> {
        let mut code = None;
        let mut has_id = false;
        let mut count = 1;
        self.detail.clear();
        rd.begin_object()?;
        let mut first = true;
        while rd.next_key(first, field)? {
            first = false;
            match field.as_slice() {
                b"code" => {
                    rd.string(&mut self.s)?;
                    code = Some(parse_code(&self.s).ok_or(BaselineError::Schema("entry `code` is not PMxxx"))?);
                }
                b"id" => {
                    rd.string(&mut self.id)?;
                    has_id = true;
                }
                b"detail" => {
                    self.detail.clear();
                    rd.begin_array()?;
                    let mut first_item = true;
                    while rd.next_item(first_item)? {
                        first_item = false;
                        rd.string(&mut self.s)?;
                        push_part(&mut self.detail, &self.s);
                    }
                }
                b"count" => count = rd.u32()?,
                _ => rd.skip()?,
            }
        }
        let Some(code) = code else { return Err(BaselineError::Schema("entry without `code`")) };
        if !has_id {
            return Err(BaselineError::Schema("entry without `id`"));
        }
        self.key.clear();
        self.key.extend_from_slice(&code.to_le_bytes());
        push_part(&mut self.key, &self.id);
        self.key.extend_from_slice(&self.detail);
        Ok(count)
    }
}

/// `PM` + three digits (registered or not: a removed rule's entries simply never match).
fn parse_code(s: &[u8]) -> Option<u16> {
    match s {
        [b'P', b'M', d @ ..] if d.len() == 3 && d.iter().all(u8::is_ascii_digit) => {
            Some(d.iter().fold(0u16, |acc, &c| acc * 10 + u16::from(c - b'0')))
        }
        _ => None,
    }
}

/// Writes the baseline of every finding in `r`, entries sorted.
pub(super) fn write(r: &Report<'_>, out: &mut Out<'_>) {
    Baseline::from_report(r).write_to(out);
}

impl Baseline {
    fn write_to(&self, out: &mut Out<'_>) {
        let mut order: Vec<u32> = (0..self.entries.len() as u32).collect();
        order.sort_unstable_by(|&x, &y| cmp_keys(self.key(&self.entries[x as usize]), self.key(&self.entries[y as usize])));
        let buf = out.buf();
        buf.extend_from_slice(b"{\n  \"version\": ");
        push_u32(buf, BASELINE_VERSION);
        buf.extend_from_slice(b",\n  \"tool\": \"pymapper\",\n  \"entries\": [");
        for (n, &i) in order.iter().enumerate() {
            let e = &self.entries[i as usize];
            let (code, mut parts) = decode(self.key(e));
            let buf = out.buf();
            buf.extend_from_slice(if n == 0 { b"\n    {\"code\": \"PM" } else { b",\n    {\"code\": \"PM" });
            buf.extend_from_slice(&[b'0' + (code / 100 % 10) as u8, b'0' + (code / 10 % 10) as u8, b'0' + (code % 10) as u8]);
            buf.extend_from_slice(b"\", \"id\": ");
            push_str(buf, parts.next().unwrap_or_default());
            let mut first = true;
            for part in parts {
                buf.extend_from_slice(if first { b", \"detail\": [" } else { b", " });
                first = false;
                push_str(buf, part);
            }
            if !first {
                buf.push(b']');
            }
            buf.extend_from_slice(b", \"count\": ");
            push_u32(buf, e.count);
            buf.push(b'}');
            out.maybe_flush();
        }
        out.str(if order.is_empty() { "]\n}\n" } else { "\n  ]\n}\n" });
    }
}

impl fmt::Display for Baseline {
    /// The file rendering, entries sorted (for tests and debugging).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut bytes = Vec::new();
        let mut out = Out::new(&mut bytes);
        self.write_to(&mut out);
        if out.finish().is_err() {
            return Err(fmt::Error);
        }
        f.write_str(&String::from_utf8_lossy(&bytes))
    }
}
