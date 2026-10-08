//! String interner producing `u32` ids and the Fx-style hasher (design: Performance rules).
//!
//! - [`hash_bytes`], [`FxHasher`], [`FxHashMap`], [`FxHashSet`]: our own Fx-style
//!   multiply/rotate hash (word at a time). Fast, not DoS-resistant: only hash trusted or
//!   bounded data.
//! - [`Sym`]: an interned string id (`u32`, with a niche so `Option<Sym>` is 4 bytes).
//! - [`Interner`]: an unsynchronised interner with dense ids `0..len`. Each rayon worker owns
//!   one (through [`crate::arena::PerThread`]); afterwards [`Interner::merge_all`] merges them
//!   into a global interner in parallel and returns one [`Remap`] per worker, which translates
//!   the ids that worker handed out. [`Interner::merge`] adds one interner into an existing one
//!   without renumbering it (incremental updates).
//!
//! Ids are only meaningful within the interner that produced them and are not stable across
//! runs: never persist them or order output by them.
//!
//! ```text
//!   worker 0: Interner {self:0, os:1}        worker 1: Interner {path:0, self:1}
//!   facts:    [0 1 0]                        facts:    [1 0 0]
//!                          \                /
//!   merge_all -> global {self:0, os:1, path:2}   remaps: [0->0 1->1]  [0->2 1->0]
//!   facts after Remap::apply:  [0 1 0]           [0 2 2]
//! ```
//!
//! # Why per-thread + remap (and not a shared concurrent interner)
//!
//! Both were built and measured with `benches/intern.rs` (14-core Apple Silicon, release, best
//! of N; identifiers scanned from Python sources). The shared variant was one global interner
//! split into 64 hash-selected shards, each a `Mutex` around the same open-addressing table;
//! it was deleted after this comparison (see git history of this file). Identifier frequencies
//! are Zipfian (`self`, `None`, ...), so a few shards take most of the traffic and their locks
//! and cache lines bounce between cores: on 14 threads it was slower than one thread.
//!
//! ```text
//!   corpus (identifiers, distinct)      phase         single    sharded   per-thread
//!   stdlib (536k ids, 32.7k distinct)   intern        7.7 ms    16.5 ms   2.6 ms
//!                                       scan+intern   37.4 ms   20.8 ms   5.5 ms
//!   venv   (24.1M ids, 802k distinct)   intern        -         592 ms    82.6 ms
//!                                       scan+intern   -         672 ms    200 ms
//! ```
//!
//! The per-thread variant pays in transient memory (every worker holds the common vocabulary
//! until the merge) and a merge step. Numbers for the final version are in `benches/intern.rs`.
//!
//! # Storage layout
//!
//! ```text
//!   bytes:  [u8]          "selfospath..."               all strings back to back
//!   offs:   [u32; n+1]    0 4 6 10 ...                  string i = bytes[offs[i]..offs[i+1]]
//!   shards: [Index; 64]   selected by the low 6 bits of the hash tag
//!   Index:  slots [u64; 2^k] = (tag << 32) | (id + 1), 0 = empty; linear probing from the
//!           top k bits of the tag
//! ```
//!
//! `tag` is the high 32 bits of [`hash_bytes`]. Comparing tags first avoids touching string bytes
//! on almost every mismatch, and growth rehashes from stored tags without reading strings. The
//! index is split into shards so that [`Interner::merge_all`] can merge every shard on its own
//! thread; a worker's strings of shard `s` are exactly the occupied slots of its shard `s`.
//! Per string: 4 bytes of offset, 8 bytes per slot (load factor 3/8..3/4) and the bytes
//! themselves; no per-string heap allocation.

use std::fmt;
use std::hash::{BuildHasherDefault, Hasher};
use std::num::NonZeroU32;

use rayon::prelude::*;

// ---------------------------------------------------------------------------------------------
// Fx-style hash
// ---------------------------------------------------------------------------------------------

/// Multiplier of the Fx hash (the constant used by rustc-hash 2).
const FX_K: u64 = 0xf135_7aea_2e62_a9c5;

#[inline(always)]
const fn fx_add(h: u64, w: u64) -> u64 {
    (h.rotate_left(5) ^ w).wrapping_mul(FX_K)
}

/// Folds `bytes` into `h` eight bytes at a time. The tail (< 8 bytes) is read with
/// overlapping loads; together with the length (mixed in by callers) it is unambiguous.
#[inline(always)]
fn fx_words(mut h: u64, bytes: &[u8]) -> u64 {
    let (words, r) = bytes.as_chunks::<8>();
    for w in words {
        h = fx_add(h, u64::from_le_bytes(*w));
    }
    let n = r.len();
    if n >= 4 {
        let lo = u32::from_le_bytes(r[..4].try_into().expect("4-byte head"));
        let hi = u32::from_le_bytes(r[n - 4..].try_into().expect("4-byte tail"));
        h = fx_add(h, u64::from(lo) | (u64::from(hi) << 32));
    } else if n > 0 {
        let w = u64::from(r[0]) | (u64::from(r[n / 2]) << 8) | (u64::from(r[n - 1]) << 16);
        h = fx_add(h, w);
    }
    h
}

/// Hashes a byte string. The high bits are the best mixed (the last operation is a multiply);
/// the interner indexes its tables with the top bits.
#[inline]
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    fx_add(fx_words(0, bytes), bytes.len() as u64)
}

/// Fx-style [`Hasher`] for `HashMap`/`HashSet` keys we control (ids, small integers, names).
#[derive(Clone, Copy, Default)]
pub struct FxHasher {
    h: u64,
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.h = fx_add(fx_words(self.h, bytes), bytes.len() as u64);
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.h = fx_add(self.h, u64::from(i));
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.h = fx_add(self.h, u64::from(i));
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.h = fx_add(self.h, u64::from(i));
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.h = fx_add(self.h, i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.h = fx_add(self.h, i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        // std's hash tables take the bucket from the low bits; move the well-mixed high bits
        // there.
        self.h.rotate_left(26)
    }
}

/// `BuildHasher` for [`FxHasher`].
pub type FxBuildHasher = BuildHasherDefault<FxHasher>;
/// `HashMap` with the Fx hasher.
pub type FxHashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;
/// `HashSet` with the Fx hasher.
pub type FxHashSet<T> = std::collections::HashSet<T, FxBuildHasher>;

// ---------------------------------------------------------------------------------------------
// Sym
// ---------------------------------------------------------------------------------------------

/// Interned string id. 4 bytes; `Option<Sym>` is also 4 bytes.
///
/// [`Sym::index`] is a small integer suitable for indexing side tables (`Vec<T>` by symbol).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Sym(NonZeroU32);

impl Sym {
    /// Largest representable index.
    pub const MAX_INDEX: u32 = u32::MAX - 1;

    /// Symbol with the given index.
    ///
    /// # Panics
    /// If `index > Sym::MAX_INDEX`.
    #[inline]
    pub const fn from_index(index: u32) -> Sym {
        match NonZeroU32::new(index.wrapping_add(1)) {
            Some(n) => Sym(n),
            None => panic!("Sym index out of range"),
        }
    }

    /// Index for side tables.
    #[inline]
    pub const fn index(self) -> usize {
        (self.0.get() - 1) as usize
    }

    /// Raw non-zero encoding (index + 1), e.g. for compact serialisation.
    #[inline]
    pub const fn to_raw(self) -> u32 {
        self.0.get()
    }

    /// Inverse of [`Sym::to_raw`]; `None` for 0.
    #[inline]
    pub const fn from_raw(raw: u32) -> Option<Sym> {
        match NonZeroU32::new(raw) {
            Some(n) => Some(Sym(n)),
            None => None,
        }
    }
}

impl fmt::Debug for Sym {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sym({})", self.index())
    }
}

// ---------------------------------------------------------------------------------------------
// Index: one shard of the open-addressing hash index
// ---------------------------------------------------------------------------------------------

/// log2 of the number of index shards.
const SHARD_BITS: u32 = 6;
/// Number of index shards.
const SHARDS: usize = 1 << SHARD_BITS;
/// Smallest non-empty slot array of one shard.
const MIN_SLOTS: usize = 16;

#[cold]
#[inline(never)]
fn interner_full() -> ! {
    panic!("interner capacity exceeded (more than 2^32 - 2 strings or 4 GiB of string data)")
}

/// High 32 bits of the hash: stored in slots, probe start from its top bits.
#[inline(always)]
fn tag_of(s: &[u8]) -> u32 {
    (hash_bytes(s) >> 32) as u32
}

/// Index shard of a tag. Uses the low bits; probe starts use the top bits, so the two are
/// independent while a shard has fewer than `2^(32 - SHARD_BITS)` slots.
#[inline(always)]
fn shard_of(tag: u32) -> usize {
    (tag as usize) & (SHARDS - 1)
}

#[inline(always)]
const fn slot_tag(slot: u64) -> u32 {
    (slot >> 32) as u32
}

/// Id stored in an occupied slot.
#[inline(always)]
const fn slot_id(slot: u64) -> u32 {
    (slot as u32).wrapping_sub(1)
}

#[inline(always)]
const fn make_slot(tag: u32, id: u32) -> u64 {
    ((tag as u64) << 32) | (id as u64 + 1)
}

/// String `id` of the storage `(bytes, offs)`.
#[inline(always)]
fn get_str<'a>(bytes: &'a [u8], offs: &[u32], id: u32) -> &'a [u8] {
    let i = id as usize;
    &bytes[offs[i] as usize..offs[i + 1] as usize]
}

/// One shard of the hash index: maps tags + string bytes to ids.
#[derive(Default)]
struct Index {
    slots: Vec<u64>,
    /// `32 - log2(slots.len())`; meaningless while `slots` is empty.
    shift: u32,
    /// Occupied slots.
    len: u32,
}

impl Index {
    #[inline(always)]
    fn start(&self, tag: u32) -> usize {
        debug_assert!(self.shift < 32 && self.slots.len().is_power_of_two());
        (tag >> self.shift) as usize
    }

    /// Id of the string equal to `s` (with tag `tag`) in `(bytes, offs)`.
    #[inline]
    fn find(&self, bytes: &[u8], offs: &[u32], s: &[u8], tag: u32) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut i = self.start(tag);
        loop {
            let slot = self.slots[i];
            if slot == 0 {
                return None;
            }
            if slot_tag(slot) == tag && get_str(bytes, offs, slot_id(slot)) == s {
                return Some(slot_id(slot));
            }
            i = (i + 1) & mask;
        }
    }

    /// Whether one more entry needs a bigger slot array (load factor above 3/4).
    #[inline(always)]
    fn needs_grow(&self) -> bool {
        (self.len as usize + 1) * 4 > self.slots.len() * 3
    }

    /// Ensures `extra` more entries fit without growing.
    fn reserve(&mut self, extra: usize) {
        let want = self.len as usize + extra;
        let mut cap = self.slots.len().max(MIN_SLOTS);
        while want * 4 > cap * 3 {
            cap *= 2;
        }
        if cap != self.slots.len() {
            self.rehash(cap);
        }
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self) {
        self.rehash((self.slots.len() * 2).max(MIN_SLOTS));
    }

    fn rehash(&mut self, cap: usize) {
        debug_assert!(cap.is_power_of_two() && cap <= 1 << (32 - SHARD_BITS));
        let old = std::mem::replace(&mut self.slots, vec![0; cap]);
        self.shift = 32 - cap.trailing_zeros();
        let mask = cap - 1;
        for slot in old {
            if slot == 0 {
                continue;
            }
            let mut i = self.start(slot_tag(slot));
            while self.slots[i] != 0 {
                i = (i + 1) & mask;
            }
            self.slots[i] = slot;
        }
    }

    /// Occupied slots, i.e. `(tag, id)` of every string in this shard.
    #[inline]
    fn entries(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.slots.iter().filter(|&&s| s != 0).map(|&s| (slot_tag(s), slot_id(s)))
    }

    fn heap_bytes(&self) -> usize {
        self.slots.capacity() * 8
    }
}

/// Inserts into `(bytes, offs, ix)`: returns the id of `s`, appending it if new. Free function
/// over the parts so a shard index can be borrowed mutably next to the string storage.
#[inline]
fn intern_into(bytes: &mut Vec<u8>, offs: &mut Vec<u32>, ix: &mut Index, s: &[u8], tag: u32) -> u32 {
    if ix.needs_grow() {
        ix.grow();
    }
    let mask = ix.slots.len() - 1;
    let mut i = ix.start(tag);
    loop {
        let slot = ix.slots[i];
        if slot == 0 {
            break;
        }
        if slot_tag(slot) == tag && get_str(bytes, offs, slot_id(slot)) == s {
            return slot_id(slot);
        }
        i = (i + 1) & mask;
    }
    // All checks happen before any mutation, so a panic leaves the interner consistent.
    let id = offs.len() - 1;
    let end = bytes.len() + s.len();
    if id >= Sym::MAX_INDEX as usize || end > u32::MAX as usize {
        interner_full();
    }
    bytes.extend_from_slice(s);
    offs.push(end as u32);
    ix.slots[i] = make_slot(tag, id as u32);
    ix.len += 1;
    id as u32
}

// ---------------------------------------------------------------------------------------------
// Interner
// ---------------------------------------------------------------------------------------------

/// Unsynchronised string interner with dense ids `0..len` (see the module docs).
pub struct Interner {
    bytes: Vec<u8>,
    /// `offs[i]..offs[i + 1]` is string `i`; `offs[0] == 0`, so `len == offs.len() - 1`.
    offs: Vec<u32>,
    shards: Box<[Index; SHARDS]>,
}

impl Default for Interner {
    fn default() -> Self {
        Interner::new()
    }
}

impl fmt::Debug for Interner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Interner")
            .field("len", &self.len())
            .field("string_bytes", &self.bytes.len())
            .finish()
    }
}

/// Output of one shard of [`Interner::merge_all`].
struct ShardBuild {
    bytes: Vec<u8>,
    offs: Vec<u32>,
    ix: Index,
    /// Shard-local id of every input string, in part order, then slot order of the part's
    /// shard (the order of [`Index::entries`]).
    locals: Vec<u32>,
}

impl Interner {
    /// Creates an empty interner.
    pub fn new() -> Interner {
        Interner {
            bytes: Vec::new(),
            offs: vec![0],
            shards: Box::new(std::array::from_fn(|_| Index::default())),
        }
    }

    /// Ensures `strings` more strings with `bytes` bytes of data fit without reallocating
    /// (assuming they spread evenly over the index shards).
    pub fn reserve(&mut self, strings: usize, bytes: usize) {
        let per_shard = strings.div_ceil(SHARDS);
        for ix in self.shards.iter_mut() {
            ix.reserve(per_shard);
        }
        self.bytes.reserve(bytes);
        self.offs.reserve(strings);
    }

    /// Returns the id of `s`, inserting it if new.
    #[inline]
    pub fn intern(&mut self, s: &[u8]) -> Sym {
        let tag = tag_of(s);
        let Interner { bytes, offs, shards } = self;
        Sym::from_index(intern_into(bytes, offs, &mut shards[shard_of(tag)], s, tag))
    }

    /// Id of `s` if it was interned.
    #[inline]
    pub fn get(&self, s: &[u8]) -> Option<Sym> {
        let tag = tag_of(s);
        self.shards[shard_of(tag)].find(&self.bytes, &self.offs, s, tag).map(Sym::from_index)
    }

    /// The string of `sym`.
    ///
    /// # Panics
    /// If `sym` is out of range here (it came from another interner).
    #[inline]
    pub fn resolve(&self, sym: Sym) -> &[u8] {
        get_str(&self.bytes, &self.offs, sym.index() as u32)
    }

    /// Number of distinct strings (ids are `0..len`, so this is also the side-table length).
    #[inline]
    pub fn len(&self) -> usize {
        self.offs.len() - 1
    }

    /// Whether nothing was interned.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Heap bytes reserved.
    pub fn heap_bytes(&self) -> usize {
        let mut n = self.bytes.capacity() + self.offs.capacity() * 4 + SHARDS * size_of::<Index>();
        for ix in self.shards.iter() {
            n += ix.heap_bytes();
        }
        n
    }

    /// Total bytes of string data.
    pub fn string_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// All `(sym, string)` pairs in id order.
    pub fn iter(&self) -> impl Iterator<Item = (Sym, &[u8])> {
        (0..self.len() as u32).map(|i| (Sym::from_index(i), get_str(&self.bytes, &self.offs, i)))
    }

    /// Interns every string of `other` into `self` without renumbering `self` (e.g. names of
    /// re-extracted files into the global interner). The remap translates ids of `other` into
    /// ids of `self`. Sequential: use [`Interner::merge_all`] to build a global interner from
    /// many workers.
    pub fn merge(&mut self, other: &Interner) -> Remap {
        let mut map = vec![Sym::from_index(0); other.len()];
        let Interner { bytes, offs, shards } = self;
        for (ix, oix) in shards.iter_mut().zip(other.shards.iter()) {
            for (tag, id) in oix.entries() {
                let s = get_str(&other.bytes, &other.offs, id);
                map[id as usize] = Sym::from_index(intern_into(bytes, offs, ix, s, tag));
            }
        }
        Remap { map: Some(map) }
    }
}

impl Interner {
    /// Merges per-worker interners into a new global interner, one index shard per rayon task,
    /// and returns one [`Remap`] per part (in input order) translating that part's ids.
    ///
    /// Ids of the result are grouped by index shard and depend on the parts' contents and
    /// order; like all ids they are not stable across runs (see the module docs). If at most
    /// one part is non-empty it becomes the result unchanged and every remap is the identity.
    ///
    /// Peak memory is the parts plus the result plus 4 bytes per input string; the parts are
    /// dropped before the result's string data is concatenated.
    pub fn merge_all(mut parts: Vec<Interner>) -> (Interner, Vec<Remap>) {
        let mut non_empty = 0;
        let mut first = None;
        for (i, p) in parts.iter().enumerate() {
            if !p.is_empty() {
                non_empty += 1;
                first.get_or_insert(i);
            }
        }
        if non_empty <= 1 {
            let global = first.map_or_else(Interner::new, |i| std::mem::take(&mut parts[i]));
            let remaps = (0..parts.len()).map(|_| Remap { map: None }).collect();
            return (global, remaps);
        }

        // 1. Every shard independently: intern the part strings of that shard into a fresh
        //    shard-local table, remembering each string's shard-local id.
        let mut built: Vec<ShardBuild> =
            (0..SHARDS).into_par_iter().map(|s| merge_shard(&parts, s)).collect();

        // 2. Shard `s` gets the global ids `bases[s]..bases[s + 1]`.
        let mut bases = [0u32; SHARDS + 1];
        let mut total = 0usize;
        let mut total_bytes = 0usize;
        for (s, b) in built.iter().enumerate() {
            bases[s] = total as u32;
            total += b.offs.len() - 1;
            total_bytes += b.bytes.len();
            if total > Sym::MAX_INDEX as usize || total_bytes > u32::MAX as usize {
                interner_full();
            }
        }
        bases[SHARDS] = total as u32;

        // 3. Remaps, one task per part, while the parts still exist.
        let remaps: Vec<Remap> = (0..parts.len())
            .into_par_iter()
            .map(|p| remap_part(&parts, &built, &bases, p))
            .collect();
        drop(parts);

        // 4. Shift each shard's slot ids to global ids, then concatenate the string data.
        built.par_iter_mut().zip(&bases[..SHARDS]).for_each(|(b, &base)| {
            for slot in b.ix.slots.iter_mut() {
                if *slot != 0 {
                    // No carry into the tag: `id + 1 + base <= total <= Sym::MAX_INDEX`.
                    *slot += u64::from(base);
                }
            }
            b.locals = Vec::new();
        });
        let mut bytes = Vec::with_capacity(total_bytes);
        let mut offs = Vec::with_capacity(total + 1);
        offs.push(0u32);
        let mut shards: Box<[Index; SHARDS]> = Box::new(std::array::from_fn(|_| Index::default()));
        for (b, ix) in built.into_iter().zip(shards.iter_mut()) {
            let off = bytes.len() as u32;
            bytes.extend_from_slice(&b.bytes);
            for &o in &b.offs[1..] {
                offs.push(o + off);
            }
            *ix = b.ix;
        }
        debug_assert_eq!(offs.len(), total + 1);
        debug_assert_eq!(bytes.len(), total_bytes);
        (Interner { bytes, offs, shards }, remaps)
    }
}

/// Step 1 of [`Interner::merge_all`] for shard `s`.
fn merge_shard(parts: &[Interner], s: usize) -> ShardBuild {
    let mut inputs = 0usize;
    let mut largest = 0usize;
    let mut bytes_est = 0usize;
    for p in parts {
        let n = p.shards[s].len as usize;
        inputs += n;
        largest = largest.max(n);
        // Shards fill evenly, so the largest part's share of its bytes estimates ours.
        bytes_est = bytes_est.max(p.bytes.len() / SHARDS);
    }
    // Most strings occur in many parts, so size for the largest part, not the sum.
    let mut b = ShardBuild {
        bytes: Vec::with_capacity(bytes_est),
        offs: Vec::with_capacity(largest + 1),
        ix: Index::default(),
        locals: Vec::with_capacity(inputs),
    };
    b.offs.push(0);
    b.ix.reserve(largest);
    for p in parts {
        for (tag, id) in p.shards[s].entries() {
            let st = get_str(&p.bytes, &p.offs, id);
            b.locals.push(intern_into(&mut b.bytes, &mut b.offs, &mut b.ix, st, tag));
        }
    }
    b
}

/// Step 3 of [`Interner::merge_all`] for part `p`.
fn remap_part(parts: &[Interner], built: &[ShardBuild], bases: &[u32], p: usize) -> Remap {
    let part = &parts[p];
    let mut map = vec![Sym::from_index(0); part.len()];
    for (s, b) in built.iter().enumerate() {
        // This part's run in `locals` starts after the runs of the earlier parts.
        let mut start = 0usize;
        for q in &parts[..p] {
            start += q.shards[s].len as usize;
        }
        let ix = &part.shards[s];
        let locals = &b.locals[start..start + ix.len as usize];
        for ((_, id), &local) in ix.entries().zip(locals) {
            map[id as usize] = Sym::from_index(bases[s] + local);
        }
    }
    Remap { map: Some(map) }
}

/// Translation of one worker's ids into global ids (from [`Interner::merge_all`] or
/// [`Interner::merge`]).
pub struct Remap {
    /// `None` = identity.
    map: Option<Vec<Sym>>,
}

impl fmt::Debug for Remap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.map {
            Some(m) => write!(f, "Remap({} ids)", m.len()),
            None => f.write_str("Remap(identity)"),
        }
    }
}

impl Remap {
    /// Whether ids stay unchanged.
    pub fn is_identity(&self) -> bool {
        self.map.is_none()
    }

    /// Global id of a worker-local id.
    ///
    /// # Panics
    /// If `local` was not handed out by the worker interner this remap belongs to.
    #[inline]
    pub fn get(&self, local: Sym) -> Sym {
        match &self.map {
            Some(m) => m[local.index()],
            None => local,
        }
    }

    /// Rewrites ids in place.
    #[inline]
    pub fn apply(&self, ids: &mut [Sym]) {
        if let Some(m) = &self.map {
            for id in ids {
                *id = m[id.index()];
            }
        }
    }

    /// Heap bytes held by the table.
    pub fn heap_bytes(&self) -> usize {
        self.map.as_ref().map_or(0, |m| m.capacity() * 4)
    }
}
// ---------------------------------------------------------------------------------------------
// Candidate strategy: sharded global concurrent interner (benchmark comparison only)
// ---------------------------------------------------------------------------------------------

pub mod sharded {
    //! One global interner split into [`SHARDS`](super) mutex-protected shards selected by the
    //! low bits of the hash tag. Workers intern directly into it and get final ids at once.
    //!
    //! `Sym::index = (local_id << SHARD_BITS) | shard`, so ids are final as soon as they are
    //! handed out; the index space has small holes (shards fill evenly but not exactly).

    use std::sync::{Mutex, PoisonError};

    use super::{Index, SHARD_BITS, SHARDS, Sym, get_str, intern_into, interner_full, shard_of, tag_of};
    use crate::arena::CachePadded;

    const MASK: u32 = (1 << SHARD_BITS) - 1;

    /// String storage + index of one shard.
    struct Shard {
        bytes: Vec<u8>,
        offs: Vec<u32>,
        ix: Index,
    }

    impl Shard {
        fn new() -> Shard {
            Shard { bytes: Vec::new(), offs: vec![0], ix: Index::default() }
        }
    }

    #[inline]
    fn make_sym(local: u32, shard: usize) -> Sym {
        let index = (u64::from(local) << SHARD_BITS) | shard as u64;
        if index > u64::from(Sym::MAX_INDEX) {
            interner_full();
        }
        Sym::from_index(index as u32)
    }

    /// Concurrent interner: `intern(&self)` from any number of threads.
    pub struct ShardedInterner {
        shards: Box<[CachePadded<Mutex<Shard>>]>,
    }

    impl Default for ShardedInterner {
        fn default() -> Self {
            ShardedInterner::new()
        }
    }

    impl ShardedInterner {
        /// Creates an empty interner.
        pub fn new() -> ShardedInterner {
            let mut shards = Vec::with_capacity(SHARDS);
            for _ in 0..SHARDS {
                shards.push(CachePadded(Mutex::new(Shard::new())));
            }
            ShardedInterner { shards: shards.into_boxed_slice() }
        }

        /// Returns the id of `s`, inserting it if new. Thread-safe.
        #[inline]
        pub fn intern(&self, s: &[u8]) -> Sym {
            let tag = tag_of(s);
            let shard = shard_of(tag);
            let local = {
                let mut g = self.shards[shard].0.lock().unwrap_or_else(PoisonError::into_inner);
                let Shard { bytes, offs, ix } = &mut *g;
                intern_into(bytes, offs, ix, s, tag)
            };
            make_sym(local, shard)
        }

        /// Ends the concurrent phase: lock-free read-only view.
        pub fn freeze(self) -> Frozen {
            let mut shards = Vec::with_capacity(SHARDS);
            for s in self.shards.into_vec() {
                shards.push(s.0.into_inner().unwrap_or_else(PoisonError::into_inner));
            }
            Frozen { shards: shards.into_boxed_slice() }
        }
    }

    /// Frozen interner: resolve and look up without locks.
    pub struct Frozen {
        shards: Box<[Shard]>,
    }

    impl Frozen {
        /// The string of `sym`.
        #[inline]
        pub fn resolve(&self, sym: Sym) -> &[u8] {
            let i = sym.index() as u32;
            let sh = &self.shards[(i & MASK) as usize];
            get_str(&sh.bytes, &sh.offs, i >> SHARD_BITS)
        }

        /// Id of `s` if it was interned.
        #[inline]
        pub fn get(&self, s: &[u8]) -> Option<Sym> {
            let tag = tag_of(s);
            let shard = shard_of(tag);
            let sh = &self.shards[shard];
            let local = sh.ix.find(&sh.bytes, &sh.offs, s, tag)?;
            Some(make_sym(local, shard))
        }

        /// Number of distinct strings.
        pub fn len(&self) -> usize {
            let mut n = 0;
            for sh in self.shards.iter() {
                n += sh.offs.len() - 1;
            }
            n
        }

        /// Whether nothing was interned.
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }

        /// Heap bytes held.
        pub fn heap_bytes(&self) -> usize {
            let mut n = 0;
            for sh in self.shards.iter() {
                n += sh.bytes.capacity() + sh.offs.capacity() * 4 + sh.ix.heap_bytes();
            }
            n
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::PerThread;
    use std::collections::HashMap;
    use std::hash::{BuildHasher, Hash};

    fn words(n: usize) -> Vec<Vec<u8>> {
        // Mix of short/long, ASCII/non-UTF-8, and near-duplicates differing only in length/tail.
        let mut out = Vec::new();
        for i in 0..n {
            let w = match i % 6 {
                0 => format!("n{i}").into_bytes(),
                1 => format!("some_longer_identifier_number_{i}").into_bytes(),
                2 => {
                    let mut v = format!("x{i}").into_bytes();
                    v.push(0xff);
                    v
                }
                3 => format!("_{i:x}_").into_bytes(),
                4 => format!("{i}\0").into_bytes(),
                _ => format!("{i}").into_bytes(),
            };
            out.push(w);
        }
        out
    }

    #[test]
    fn hash_distinguishes_lengths_and_tails() {
        let cases: [&[u8]; 12] =
            [b"", b"\0", b"\0\0", b"a", b"a\0", b"ab", b"abc", b"abcd", b"abcde", b"abcdefg",
             b"abcdefgh", b"abcdefgh\0"];
        let mut seen = FxHashSet::default();
        for c in cases {
            assert!(seen.insert(hash_bytes(c)), "collision for {c:?}");
        }
        assert_eq!(hash_bytes(b"self"), hash_bytes(b"self"));
    }

    #[test]
    fn hash_top_bits_spread() {
        // Top 8 bits of 64k sequential identifiers should hit every bucket roughly evenly.
        let mut buckets = [0u32; 256];
        for i in 0..65_536u32 {
            let s = format!("name_{i}");
            buckets[(hash_bytes(s.as_bytes()) >> 56) as usize] += 1;
        }
        let (min, max) = (buckets.iter().min().unwrap(), buckets.iter().max().unwrap());
        assert!(*min > 150 && *max < 380, "poor spread: min {min} max {max}");
    }

    #[test]
    fn fx_hasher_works_in_std_maps() {
        let mut m: FxHashMap<&str, u32> = FxHashMap::default();
        for (i, w) in ["a", "b", "self", "__init__"].iter().enumerate() {
            m.insert(w, i as u32);
        }
        assert_eq!(m["self"], 2);
        let bh = FxBuildHasher::default();
        let h = |v: &dyn Fn(&mut FxHasher)| {
            let mut s = bh.build_hasher();
            v(&mut s);
            s.finish()
        };
        assert_ne!(h(&|s| 1u32.hash(s)), h(&|s| 2u32.hash(s)));
        assert_ne!(h(&|s| "ab".hash(s)), h(&|s| "a".hash(s)));
    }

    #[test]
    fn sym_layout_and_roundtrip() {
        assert_eq!(std::mem::size_of::<Option<Sym>>(), 4);
        let s = Sym::from_index(41);
        assert_eq!(s.index(), 41);
        assert_eq!(Sym::from_raw(s.to_raw()), Some(s));
        assert_eq!(Sym::from_raw(0), None);
        assert_eq!(Sym::from_index(Sym::MAX_INDEX).index(), Sym::MAX_INDEX as usize);
    }

    #[test]
    #[should_panic(expected = "Sym index out of range")]
    fn sym_rejects_max() {
        let _ = Sym::from_index(u32::MAX);
    }


    #[test]
    fn hash_shard_bits_spread() {
        // The shard bits (low bits of the tag) must spread like the probe bits do.
        let mut buckets = [0u32; SHARDS];
        for i in 0..64_000u32 {
            let s = format!("attr{i}");
            buckets[shard_of(tag_of(s.as_bytes()))] += 1;
        }
        let (min, max) = (buckets.iter().min().unwrap(), buckets.iter().max().unwrap());
        assert!(*min > 800 && *max < 1200, "poor shard spread: min {min} max {max}");
    }

    #[test]
    fn interner_grows_and_finds() {
        let mut it = Interner::new();
        let ws = words(10_000);
        for (i, w) in ws.iter().enumerate() {
            assert_eq!(it.intern(w).index(), i, "ids are dense in insertion order");
        }
        for (i, w) in ws.iter().enumerate() {
            assert_eq!(it.intern(w).index(), i);
            assert_eq!(it.get(w), Some(Sym::from_index(i as u32)));
            assert_eq!(it.resolve(Sym::from_index(i as u32)), &w[..]);
        }
        assert_eq!(it.len(), ws.len());
        assert_eq!(it.get(b"missing"), None);
        let mut occupied = 0;
        for ix in it.shards.iter() {
            assert!(ix.slots.is_empty() || ix.slots.len() * 3 >= ix.len as usize * 4);
            occupied += ix.entries().count();
            assert_eq!(ix.entries().count(), ix.len as usize);
        }
        assert_eq!(occupied, ws.len());
        for (sym, s) in it.iter() {
            assert_eq!(s, &ws[sym.index()][..]);
        }
        let total: usize = ws.iter().map(Vec::len).sum();
        assert_eq!(it.string_bytes(), total);
        assert!(it.heap_bytes() >= total + 4 * ws.len());
    }

    #[test]
    fn index_handles_full_tag_collisions() {
        // Same tag for every key: correctness must come from the byte comparison.
        let (mut bytes, mut offs, mut ix) = (Vec::new(), vec![0u32], Index::default());
        for i in 0..500u32 {
            let k = format!("k{i}");
            assert_eq!(intern_into(&mut bytes, &mut offs, &mut ix, k.as_bytes(), 7), i);
        }
        for i in 0..500u32 {
            let k = format!("k{i}");
            assert_eq!(ix.find(&bytes, &offs, k.as_bytes(), 7), Some(i));
            assert_eq!(intern_into(&mut bytes, &mut offs, &mut ix, k.as_bytes(), 7), i);
        }
        assert_eq!(ix.find(&bytes, &offs, b"k500", 7), None);
        assert_eq!(ix.len, 500);
    }

    #[test]
    fn empty_string_and_empty_interner() {
        let mut it = Interner::new();
        assert!(it.is_empty());
        assert_eq!(it.get(b""), None);
        assert_eq!(it.iter().count(), 0);
        let e = it.intern(b"");
        assert_eq!(it.resolve(e), b"");
        assert_eq!(it.intern(b""), e);
        assert_eq!(it.get(b""), Some(e));
        let f = it.intern(b"\0");
        assert_ne!(e, f);
        assert_eq!(it.resolve(f), b"\0");
    }

    #[test]
    fn reserve_prevents_regrowth() {
        let mut it = Interner::new();
        it.reserve(5_000, 50_000);
        let caps: Vec<usize> = it.shards.iter().map(|ix| ix.slots.len()).collect();
        for w in words(3_000) {
            it.intern(&w);
        }
        let after: Vec<usize> = it.shards.iter().map(|ix| ix.slots.len()).collect();
        assert_eq!(caps, after);
    }

    #[test]
    fn merge_preserves_existing_ids() {
        let ws = words(3000);
        let mut g = Interner::new();
        let gi: Vec<Sym> = ws[..2000].iter().map(|w| g.intern(w)).collect();
        let mut o = Interner::new();
        let oi: Vec<Sym> = ws[1000..].iter().rev().map(|w| o.intern(w)).collect();
        let r = g.merge(&o);
        assert!(!r.is_identity());
        assert_eq!(g.len(), 3000);
        for (k, s) in gi.iter().enumerate() {
            assert_eq!(g.resolve(*s), &ws[k][..], "existing ids unchanged");
        }
        for (k, s) in oi.iter().enumerate() {
            assert_eq!(g.resolve(r.get(*s)), &ws[2999 - k][..]);
        }
        // Merging an empty interner is a no-op.
        let r = g.merge(&Interner::new());
        assert_eq!(g.len(), 3000);
        assert_eq!(r.heap_bytes(), 0);
    }

    /// Checks that every part's ids, remapped, resolve to the same strings in `g`, and that `g`
    /// holds exactly the union of the parts with dense, unique ids.
    fn check_merged(parts_strings: &[Vec<Vec<u8>>], g: &Interner, remaps: &[Remap]) {
        let mut union: FxHashSet<&[u8]> = FxHashSet::default();
        for (p, strings) in parts_strings.iter().enumerate() {
            let mut ids: Vec<Sym> = (0..strings.len() as u32).map(Sym::from_index).collect();
            remaps[p].apply(&mut ids);
            for (k, s) in strings.iter().enumerate() {
                assert_eq!(g.resolve(ids[k]), &s[..]);
                assert_eq!(remaps[p].get(Sym::from_index(k as u32)), ids[k]);
                assert_eq!(g.get(s), Some(ids[k]));
                union.insert(s);
            }
        }
        assert_eq!(g.len(), union.len());
        let mut seen = FxHashSet::default();
        for (sym, s) in g.iter() {
            assert!(seen.insert(s), "duplicate string in merged interner");
            assert_eq!(g.get(s), Some(sym));
        }
    }

    fn build(strings: &[Vec<u8>]) -> Interner {
        let mut it = Interner::new();
        for (k, s) in strings.iter().enumerate() {
            assert_eq!(it.intern(s).index(), k, "test inputs must be distinct");
        }
        it
    }

    #[test]
    fn merge_all_overlapping_parts() {
        let ws = words(6000);
        let parts_strings: Vec<Vec<Vec<u8>>> = vec![
            ws[..2000].to_vec(),
            ws[1000..4000].iter().rev().cloned().collect(),
            Vec::new(),
            ws[500..600].to_vec(),
            ws[3500..6000].to_vec(),
        ];
        let parts: Vec<Interner> = parts_strings.iter().map(|p| build(p)).collect();
        let (g, remaps) = Interner::merge_all(parts);
        assert_eq!(remaps.len(), 5);
        assert_eq!(g.len(), 6000);
        check_merged(&parts_strings, &g, &remaps);
        // The merged interner keeps working as a normal one.
        let mut g = g;
        let n = g.intern(b"brand_new");
        assert_eq!(n.index(), 6000);
        assert_eq!(g.intern(&ws[42]), remaps[0].get(Sym::from_index(42)));
    }

    #[test]
    fn merge_all_identical_parts_and_fast_paths() {
        let ws = words(1000);
        let parts_strings = vec![ws.clone(), ws.clone(), ws.clone()];
        let parts: Vec<Interner> = parts_strings.iter().map(|p| build(p)).collect();
        let (g, remaps) = Interner::merge_all(parts);
        check_merged(&parts_strings, &g, &remaps);

        // No parts, only empty parts, one non-empty part: no merge work, identity remaps.
        let (e, r) = Interner::merge_all(Vec::new());
        assert!(e.is_empty() && r.is_empty());
        let (e, r) = Interner::merge_all(vec![Interner::new(), Interner::new()]);
        assert!(e.is_empty() && r.len() == 2 && r.iter().all(Remap::is_identity));
        let only = build(&ws[..10]);
        let (g, r) = Interner::merge_all(vec![Interner::new(), only, Interner::new()]);
        assert_eq!(g.len(), 10);
        assert!(r.iter().all(Remap::is_identity));
        check_merged(&[Vec::new(), ws[..10].to_vec(), Vec::new()], &g, &r);
    }

    #[test]
    fn merge_all_empty_string_and_small_parts() {
        let parts_strings: Vec<Vec<Vec<u8>>> = vec![
            vec![b"".to_vec(), b"a".to_vec()],
            vec![b"a".to_vec(), b"".to_vec(), b"\xff".to_vec()],
        ];
        let parts: Vec<Interner> = parts_strings.iter().map(|p| build(p)).collect();
        let (g, remaps) = Interner::merge_all(parts);
        assert_eq!(g.len(), 3);
        check_merged(&parts_strings, &g, &remaps);
    }

    #[test]
    fn per_thread_concurrent_intern_and_merge() {
        // "Files": overlapping windows over a vocabulary with a hot head (Zipf-like), interned
        // by per-thread interners under rayon, then merged and remapped.
        let ws = words(20_000);
        let pt: PerThread<Interner> = PerThread::new(Interner::new);
        let files: Vec<(usize, Vec<Sym>)> = (0..400usize)
            .into_par_iter()
            .map(|f| {
                pt.with(|slot, it| {
                    let mut ids = Vec::with_capacity(300);
                    for k in 0..300 {
                        let w = if k % 3 == 0 { &ws[k % 17] } else { &ws[(f * 47 + k) % ws.len()] };
                        ids.push(it.intern(w));
                    }
                    (slot, ids)
                })
            })
            .collect();
        let (g, remaps) = Interner::merge_all(pt.into_vec());
        let mut seen = FxHashSet::default();
        for (f, (slot, mut ids)) in files.into_iter().enumerate() {
            remaps[slot].apply(&mut ids);
            for (k, id) in ids.iter().enumerate() {
                let w = if k % 3 == 0 { &ws[k % 17] } else { &ws[(f * 47 + k) % ws.len()] };
                assert_eq!(g.resolve(*id), &w[..]);
                seen.insert(*id);
            }
        }
        assert_eq!(g.len(), seen.len(), "no strings beyond those interned");
    }

    #[test]
    fn merge_all_inside_a_small_pool_matches_sequential_merge() {
        let ws = words(5000);
        let parts_strings: Vec<Vec<Vec<u8>>> =
            (0..8).map(|p| ws[p * 400..p * 400 + 1500].to_vec()).collect();
        let pool = rayon::ThreadPoolBuilder::new().num_threads(3).build().unwrap();
        let parts: Vec<Interner> = parts_strings.iter().map(|p| build(p)).collect();
        let (g, remaps) = pool.install(|| Interner::merge_all(parts));
        check_merged(&parts_strings, &g, &remaps);
        let mut seq = Interner::new();
        for p in &parts_strings {
            seq.merge(&build(p));
        }
        assert_eq!(seq.len(), g.len());
    }

    #[test]
    fn concurrent_threads_agree_after_merge() {
        // Every worker interns the same words in a different order; after the merge all ids
        // for a word agree.
        let ws = words(30_000);
        let pt: PerThread<Interner> = PerThread::new(Interner::new);
        let per_task: Vec<(usize, Vec<(usize, Sym)>)> = (0..16usize)
            .into_par_iter()
            .map(|t| {
                pt.with(|slot, it| {
                    let mut v = Vec::with_capacity(ws.len());
                    for k in 0..ws.len() {
                        let w = (k * 7919 + t * 104_729) % ws.len();
                        v.push((w, it.intern(&ws[w])));
                    }
                    (slot, v)
                })
            })
            .collect();
        let (g, remaps) = Interner::merge_all(pt.into_vec());
        assert_eq!(g.len(), ws.len());
        let mut global: HashMap<usize, Sym> = HashMap::new();
        for (slot, v) in per_task {
            for (w, id) in v {
                let id = remaps[slot].get(id);
                assert_eq!(*global.entry(w).or_insert(id), id);
                assert_eq!(g.resolve(id), &ws[w][..]);
            }
        }
    }
}
