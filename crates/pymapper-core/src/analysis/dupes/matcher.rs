//! Whole-program matching: fingerprint buckets -> seeds -> verified, extended clone pairs ->
//! clusters and groups.

use rayon::prelude::*;

use super::norm::{Normaliser, exact};
use super::winnow::{Fingerprints, for_each_token};
use super::{ClonePair, DupesParams, Region};
use crate::lex::TokenKind;
use crate::timing;

/// Buckets up to this size seed every pair; larger ones seed paths by context (see
/// [`make_seeds`]): linear, and grouping recovers the transitive pairs.
const ALL_PAIRS_MAX: usize = 8;

/// Largest gap (in tokens, in each copy) bridged between two matching segments of the same
/// pair of files: tolerates a few inserted, deleted or changed tokens.
const MAX_GAP: u32 = 8;

/// Merge candidates looked at per segment (most recent chains of the file pair).
const CHAIN_LOOKBACK: usize = 8;

/// Repetition filter: periods tried, and the share of positions that must repeat.
const MAX_PERIOD: usize = 24;
const REPEAT_PCT: usize = 90;

/// Data filter: code has at least one name or keyword per this many tokens.
const WORD_SHARE: usize = 8;

/// Granularity of splitting boilerplate out of a segment (two periods of the
/// repetition filter, so it can see a period).
const TRIM_BLOCK: u32 = 2 * MAX_PERIOD as u32;

/// The most similar other copy of a [`DupesReport::members`] entry: the pair `this` /
/// `other`, where `this` is the member's own copy in that pair (inside the member, which may
/// be the union of several overlapping copies).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Best {
    pub this: Region,
    pub other: Region,
    pub similarity: u8,
}

/// Counters of one [`find_clones`] run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DupesStats {
    pub fingerprints: u64,
    /// Seed position pairs generated from shared fingerprints.
    pub seeds: u64,
    /// Seeds whose tokens matched and were extended.
    pub extended: u64,
    /// Seeds inside an earlier extension on the same diagonal (nothing to do).
    pub covered: u64,
    /// Seeds whose k tokens differ in the re-lexed files: 32-bit hash collisions, files that
    /// changed since extraction, or files where the parser's token stream diverges from the
    /// lexer's after a syntax error (every later position shifts, so recall drops there).
    /// `seeds = extended + covered + unverified`.
    pub unverified: u64,
    /// Tokens of the files with seeds held while extending (2 bytes each).
    pub tokens_cached: u64,
    /// File reads of the matching stage: every file with seeds, then every file with a
    /// candidate clone.
    pub files_read: u64,
    pub bytes_read: u64,
}

/// Result of [`find_clones`].
#[derive(Clone, Debug, Default)]
pub struct DupesReport {
    /// Every clone pair, sorted by (a, b) position.
    pub pairs: Vec<ClonePair>,
    /// Copies: overlapping regions of the pairs in one file merged (see [`find_clones`]),
    /// sorted by (file, position). Every member has a partner in another member.
    pub members: Vec<Region>,
    /// Per member, its most similar other copy.
    pub best: Vec<Best>,
    /// Per member, its group (groups are numbered in order of their first member).
    pub group: Vec<u32>,
    /// Per group, the range of its members in `group_members`.
    pub groups: Vec<(u32, u32)>,
    /// Member indices, grouped (ascending within a group).
    pub group_members: Vec<u32>,
    pub stats: DupesStats,
}

impl DupesReport {
    /// Member indices of group `g`.
    pub fn group(&self, g: usize) -> &[u32] {
        let (s, e) = self.groups[g];
        &self.group_members[s as usize..e as usize]
    }
}

/// Two positions with the same fingerprint; `(a_file, a_pos) < (b_file, b_pos)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Seed {
    a_file: u32,
    b_file: u32,
    a_pos: u32,
    b_pos: u32,
}

impl Seed {
    #[inline]
    fn diag(&self) -> i64 {
        i64::from(self.b_pos) - i64::from(self.a_pos)
    }
}

/// A fingerprint with its contexts: the hashes of the next and previous fingerprints of the
/// same file ([`NO_CTX`] at the ends).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Keyed {
    hash: u32,
    next: u32,
    prev: u32,
    file: u32,
    pos: u32,
}

/// Context of the last (first) fingerprint of a file.
const NO_CTX: u32 = u32::MAX;

/// Fingerprints with contexts. Rows of one file must be contiguous and in position order, as
/// taken (otherwise contexts are arbitrary: fewer clones are found, none is wrong).
fn keyed(fps: &Fingerprints) -> Vec<Keyed> {
    let rows = fps.as_slice();
    let mut out: Vec<Keyed> = Vec::with_capacity(rows.len());
    let ctx = |o: Option<&super::Fingerprint>, file: u32| match o {
        Some(n) if n.file == file => n.hash,
        _ => NO_CTX,
    };
    for (i, f) in rows.iter().enumerate() {
        let prev = ctx(i.checked_sub(1).map(|j| &rows[j]), f.file);
        out.push(Keyed { hash: f.hash, next: ctx(rows.get(i + 1), f.file), prev, file: f.file, pos: f.pos });
    }
    out
}

/// Seeds from the fingerprints (reordered in place). A bucket (one hash) of at most
/// [`ALL_PAIRS_MAX`] occurrences seeds every pair. A larger one (a common k-gram) is split by
/// context, once by next and once by previous fingerprint, and within a context each
/// occurrence seeds its successor: occurrences of a real clone share their neighbouring
/// fingerprints (inside the clone), so they meet in one context and end up in one group, at
/// two seeds per occurrence. Same-file seeds closer than `min_tokens` are skipped (their
/// copies would overlap).
///
/// In a large bucket, an occurrence less than `min_tokens` after the previous occurrence of
/// its file and context is dropped: such runs of hits come from periodic code (data tables,
/// long lists of like statements), where every occurrence would seed its own diagonal and
/// extending each over the whole periodic region would be quadratic in its length. The first
/// occurrence of the run still seeds the region.
fn make_seeds(fps: &mut [Keyed], params: &DupesParams) -> Vec<Seed> {
    let min_d = params.min_tokens;
    let mut seeds = Vec::new();
    fps.par_sort_unstable_by_key(|f| (f.hash, f.next, f.file, f.pos));
    for bucket in fps.chunk_by(|x, y| x.hash == y.hash) {
        if bucket.len() < 2 {
            continue;
        }
        if bucket.len() > ALL_PAIRS_MAX {
            context_paths(bucket, |f| f.next, min_d, &mut seeds);
            continue;
        }
        // Every pair, in (file, pos) order whatever the contexts.
        for (i, a) in bucket.iter().enumerate() {
            for b in &bucket[i + 1..] {
                let (a, b) = if (a.file, a.pos) < (b.file, b.pos) { (a, b) } else { (b, a) };
                push_seed(a, b, min_d, &mut seeds);
            }
        }
    }
    fps.par_sort_unstable_by_key(|f| (f.hash, f.prev, f.file, f.pos));
    for bucket in fps.chunk_by(|x, y| x.hash == y.hash) {
        if bucket.len() > ALL_PAIRS_MAX {
            context_paths(bucket, |f| f.prev, min_d, &mut seeds);
        }
    }
    seeds
}

/// Seeds a path through each context of `bucket` (sorted by (context, file, pos)), skipping
/// runs of close hits in one file.
fn context_paths(bucket: &[Keyed], ctx: impl Fn(&Keyed) -> u32, min_d: u32, seeds: &mut Vec<Seed>) {
    for group in bucket.chunk_by(|x, y| ctx(x) == ctx(y)) {
        let (mut prev, mut kept): (Option<&Keyed>, Option<&Keyed>) = (None, None);
        for f in group {
            let close = matches!(prev, Some(p) if p.file == f.file && f.pos - p.pos < min_d);
            prev = Some(f);
            if close {
                continue;
            }
            if let Some(k) = kept {
                push_seed(k, f, min_d, seeds);
            }
            kept = Some(f);
        }
    }
}

/// Seeds `a` / `b` (`a` first in (file, pos) order) unless they are in one file closer than
/// `min_d`.
#[inline]
fn push_seed(a: &Keyed, b: &Keyed, min_d: u32, seeds: &mut Vec<Seed>) {
    if a.file != b.file || b.pos - a.pos >= min_d {
        seeds.push(Seed { a_file: a.file, b_file: b.file, a_pos: a.pos, b_pos: b.pos });
    }
}

/// Finds clone pairs and groups from the fingerprints of all files. `read(file, buf)` appends a
/// file's source to `buf` (empty when called) and returns false when it cannot. Sources are not
/// retained by extraction, so files are re-read here: every file with seeds once to re-lex it
/// (only one byte of normalised code and one byte of exact hash per token are kept until
/// matching ends), then every file with a candidate clone once more for line numbers. Runs on
/// the current rayon pool; deterministic.
pub fn find_clones<R>(fps: Fingerprints, params: &DupesParams, read: R) -> DupesReport
where
    R: Fn(u32, &mut Vec<u8>) -> bool + Sync,
{
    let mut stats = DupesStats { fingerprints: fps.len() as u64, ..DupesStats::default() };
    let nfiles = fps.iter().map(|f| f.file as usize + 1).max().unwrap_or(0);
    let seeds = timing::time("dupes.seeds", || {
        let mut keyed = keyed(&fps);
        drop(fps);
        let mut seeds = make_seeds(&mut keyed, params);
        drop(keyed);
        seeds.par_sort_unstable_by_key(|s| (s.a_file, s.b_file, s.diag(), s.a_pos));
        // Both contexts may seed the same pair.
        seeds.dedup();
        seeds.shrink_to_fit();
        seeds
    });
    stats.seeds = seeds.len() as u64;
    let cache = timing::time("dupes.relex", || CodeCache::build(&seeds, nfiles, &read, &mut stats));
    let cands = timing::time("dupes.extend", || {
        let groups: Vec<&[Seed]> = seeds.chunk_by(|x, y| x.a_file == y.a_file).collect();
        let per_file: Vec<(Vec<Cand>, SeedCounts)> = groups
            .par_iter()
            .map_init(Scratch::default, |sc, g| {
                let mut out = Vec::new();
                let counts = match_file(g, params, &cache, sc, &mut out);
                (out, counts)
            })
            .collect();
        let mut cands = Vec::with_capacity(per_file.iter().map(|p| p.0.len()).sum());
        for (mut c, n) in per_file {
            cands.append(&mut c);
            stats.extended += n.extended;
            stats.covered += n.covered;
            stats.unverified += n.unverified;
        }
        cands
    });
    drop(cache);
    drop(seeds);
    let mut pairs = timing::time("dupes.locate", || locate(&cands, params, &read, &mut stats));
    drop(cands);
    timing::time("dupes.group", || {
        pairs.sort_unstable_by_key(|p| (p.a.file, p.a.start_tok, p.b.file, p.b.start_tok, p.a.end_tok));
        let mut report = cluster(pairs);
        report.stats = stats;
        report
    })
}

/// Normalised codes and 8-bit exact hashes of every file with seeds (2 bytes per token).
#[derive(Debug, Default)]
struct CodeCache {
    /// Index into `codes` / `exact` per file (`u32::MAX`: no seeds).
    slot: Vec<u32>,
    codes: Vec<Box<[u8]>>,
    exact: Vec<Box<[u8]>>,
}

impl CodeCache {
    /// Re-reads and re-lexes every file of `seeds` in parallel.
    fn build<R>(seeds: &[Seed], nfiles: usize, read: &R, stats: &mut DupesStats) -> CodeCache
    where
        R: Fn(u32, &mut Vec<u8>) -> bool + Sync,
    {
        const SEEDED: u32 = u32::MAX - 1;
        let mut slot = vec![u32::MAX; nfiles];
        for s in seeds {
            slot[s.a_file as usize] = SEEDED;
            slot[s.b_file as usize] = SEEDED;
        }
        let mut files = Vec::new();
        for (f, s) in slot.iter_mut().enumerate() {
            if *s == SEEDED {
                *s = files.len() as u32;
                files.push(f as u32);
            }
        }
        // Codes, exact hashes and bytes read of each file.
        type Lexed = (Box<[u8]>, Box<[u8]>, u64);
        let lexed: Vec<Lexed> = files
            .par_iter()
            .map_init(
                || (Vec::new(), Vec::new(), Vec::new()),
                |(buf, codes, exact8), &f| {
                    buf.clear();
                    codes.clear();
                    exact8.clear();
                    if read(f, buf) {
                        let src = &buf[..];
                        let mut norm = Normaliser::default();
                        for_each_token(src, |t| {
                            if let Some(c) = norm.push(src, t) {
                                codes.push(c);
                                exact8.push(exact(src, t) as u8);
                            }
                        });
                    }
                    let bytes = buf.len() as u64;
                    // Keep the read buffer unless one huge file grew it.
                    if buf.capacity() > 16 << 20 {
                        *buf = Vec::new();
                    }
                    (codes.as_slice().into(), exact8.as_slice().into(), bytes)
                },
            )
            .collect();
        stats.files_read += files.len() as u64;
        let mut cache = CodeCache { slot, codes: Vec::with_capacity(files.len()), exact: Vec::with_capacity(files.len()) };
        for (c, e, bytes) in lexed {
            stats.bytes_read += bytes;
            stats.tokens_cached += c.len() as u64;
            cache.codes.push(c);
            cache.exact.push(e);
        }
        cache
    }

    /// Codes and exact hashes of `file` (which must have seeds).
    #[inline]
    fn get(&self, file: u32) -> (&[u8], &[u8]) {
        let i = self.slot[file as usize] as usize;
        (&self.codes[i], &self.exact[i])
    }
}

/// Per-thread buffers of the matching stage.
#[derive(Debug, Default)]
struct Scratch {
    segs: Vec<Segment>,
    chains: Vec<Segment>,
}

/// A run of aligned tokens: `a0..a1` in A matches `b0..b1` in B. A raw segment is a maximal
/// run of equal codes on one diagonal; a chain joins segments across small gaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Segment {
    a0: u32,
    a1: u32,
    b0: u32,
    b1: u32,
    /// Aligned positions with identical tokens (by 8-bit exact hash).
    same: u32,
}

/// A clone found by matching, before line numbers are known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cand {
    a_file: u32,
    b_file: u32,
    a0: u32,
    a1: u32,
    b0: u32,
    b1: u32,
    similarity: u8,
}

/// What happened to the seeds of one file (see [`DupesStats`]).
#[derive(Clone, Copy, Debug, Default)]
struct SeedCounts {
    extended: u64,
    covered: u64,
    unverified: u64,
}

/// Matches every seed of file A (`seeds`, sorted by partner, diagonal, position) and appends
/// the chains of at least `min_tokens` tokens.
fn match_file(
    seeds: &[Seed],
    params: &DupesParams,
    cache: &CodeCache,
    sc: &mut Scratch,
    out: &mut Vec<Cand>,
) -> SeedCounts {
    let a_file = seeds[0].a_file;
    let a = cache.get(a_file);
    let mut n = SeedCounts::default();
    let Scratch { segs, chains } = sc;
    for sub in seeds.chunk_by(|x, y| x.b_file == y.b_file) {
        let b_file = sub[0].b_file;
        let same_file = b_file == a_file;
        let b = cache.get(b_file);
        segs.clear();
        // Extent of the last extension: later seeds inside it (same diagonal) add nothing.
        let mut cover: Option<(i64, u32, u32)> = None;
        for s in sub {
            if let Some((d, c0, c1)) = cover
                && d == s.diag()
                && (c0..c1).contains(&s.a_pos)
            {
                n.covered += 1;
                continue;
            }
            match extend(a, b, same_file, s.a_pos, s.b_pos, params.k()) {
                Some(seg) => {
                    n.extended += 1;
                    // The cover is the untrimmed extent, so no seed re-extends a trimmed tail.
                    cover = Some((s.diag(), seg.a0, seg.a1));
                    split(seg, a, b, segs);
                }
                None => n.unverified += 1,
            }
        }
        chain_segments(segs, chains, same_file);
        for c in chains.iter() {
            let len = (c.a1 - c.a0).max(c.b1 - c.b0);
            if len < params.min_tokens {
                continue;
            }
            let similarity = (u64::from(c.same) * 100 / u64::from(len)) as u8;
            if similarity < 100 && boilerplate(&a.0[c.a0 as usize..c.a1 as usize]) {
                continue;
            }
            out.push(Cand { a_file, b_file, a0: c.a0, a1: c.a1, b0: c.b0, b1: c.b1, similarity });
        }
    }
    n
}

/// Verifies a seed (`k` equal codes at `pa` / `pb`) and extends it to the maximal run of equal
/// codes on its diagonal (`same` is not counted yet, see [`split`]). `a` / `b` are (codes,
/// exact hashes). In one file (`pa < pb`) the copies may not overlap.
fn extend(a: (&[u8], &[u8]), b: (&[u8], &[u8]), same_file: bool, pa: u32, pb: u32, k: u32) -> Option<Segment> {
    let ((ca, _), (cb, _)) = (a, b);
    let (na, nb) = (ca.len() as u32, cb.len() as u32);
    // Longest allowed run: the copies of one file may not overlap.
    let max_len = if same_file { pb - pa } else { u32::MAX };
    if k > max_len || pa + k > na || pb + k > nb {
        return None;
    }
    if ca[pa as usize..(pa + k) as usize] != cb[pb as usize..(pb + k) as usize] {
        return None;
    }
    let (mut a0, mut b0) = (pa, pb);
    while a0 > 0 && b0 > 0 && ca[a0 as usize - 1] == cb[b0 as usize - 1] && pa + k - a0 < max_len {
        a0 -= 1;
        b0 -= 1;
    }
    let (mut a1, mut b1) = (pa + k, pb + k);
    while a1 < na && b1 < nb && ca[a1 as usize] == cb[b1 as usize] && a1 - a0 < max_len {
        a1 += 1;
        b1 += 1;
    }
    Some(Segment { a0, a1, b0, b1, same: 0 })
}

/// Splits an extended segment around its noise: runs of at least two [`TRIM_BLOCK`]-token
/// blocks that are [`boilerplate`] and not identical in both copies. Each remaining piece is
/// pushed to `out` with its identical tokens counted. Code is often next to data of the same
/// shape (a table after its accessor class): the extension runs through the table, and
/// without splitting the whole run, code included, would be rejected as data. Single noisy
/// blocks stay (a short list of like statements inside a function is code).
fn split(seg: Segment, a: (&[u8], &[u8]), b: (&[u8], &[u8]), out: &mut Vec<Segment>) {
    let ((ca, ea), (_, eb)) = (a, b);
    // Position in B of position `i` in A (the segment is one diagonal).
    let in_b = |i: u32| i - seg.a0 + seg.b0;
    let mut piece = |lo: u32, hi: u32| {
        let (ra, rb) = (lo as usize..hi as usize, in_b(lo) as usize..in_b(hi) as usize);
        let same = ea[ra].iter().zip(&eb[rb]).filter(|(x, y)| x == y).count() as u32;
        out.push(Segment { a0: lo, a1: hi, b0: in_b(lo), b1: in_b(hi), same });
    };
    let len = seg.a1 - seg.a0;
    if len < 2 * TRIM_BLOCK {
        piece(seg.a0, seg.a1);
        return;
    }
    // Blocks of TRIM_BLOCK tokens from a0; the last one takes the remainder.
    let nblocks = len / TRIM_BLOCK;
    let block = |i: u32| {
        let s = seg.a0 + i * TRIM_BLOCK;
        (s, if i + 1 == nblocks { seg.a1 } else { s + TRIM_BLOCK })
    };
    let noisy = |i: u32| {
        let (s, e) = block(i);
        let (ra, rb) = (s as usize..e as usize, in_b(s) as usize..in_b(e) as usize);
        ea[ra.clone()] != eb[rb] && boilerplate(&ca[ra])
    };
    // Start of the current piece, and of the current run of noisy blocks.
    let mut start = Some(seg.a0);
    let mut run: Option<u32> = None;
    for i in 0..=nblocks {
        let n = i < nblocks && noisy(i);
        match (n, run) {
            (true, None) => run = Some(i),
            (false, Some(r)) => {
                if i - r >= 2 {
                    if let Some(st) = start.filter(|&st| st < block(r).0) {
                        piece(st, block(r).0);
                    }
                    start = (i < nblocks).then(|| block(i).0);
                }
                run = None;
            }
            _ => {}
        }
    }
    if let Some(st) = start {
        piece(st, seg.a1);
    }
}

/// Per-thread buffers of [`locate`].
#[derive(Debug, Default)]
struct LocScratch {
    buf: Vec<u8>,
    /// Byte span per kept token.
    start: Vec<u32>,
    end: Vec<u32>,
    /// Byte offset of every line start (line 1 at index 0).
    line_starts: Vec<u32>,
}

impl LocScratch {
    /// Re-reads and re-lexes `file`; false when it cannot be read.
    fn load<R: Fn(u32, &mut Vec<u8>) -> bool>(&mut self, file: u32, read: &R) -> bool {
        let LocScratch { buf, start, end, line_starts } = self;
        buf.clear();
        start.clear();
        end.clear();
        line_starts.clear();
        line_starts.push(0);
        if !read(file, buf) {
            return false;
        }
        let src = &buf[..];
        let mut norm = Normaliser::default();
        for_each_token(src, |t| {
            if norm.push(src, t).is_some() {
                start.push(t.start);
                end.push(t.end());
            }
        });
        for (i, &b) in src.iter().enumerate() {
            if b == b'\n' || (b == b'\r' && src.get(i + 1) != Some(&b'\n')) {
                line_starts.push(i as u32 + 1);
            }
        }
        true
    }

    /// 1-based line of byte offset `off`.
    #[inline]
    fn line(&self, off: u32) -> u32 {
        self.line_starts.partition_point(|&s| s <= off) as u32
    }

    /// The region of tokens `t0..t1` of `file`, or `None` if the file has fewer tokens (it
    /// changed since extraction).
    fn region(&self, file: u32, t0: u32, t1: u32) -> Option<Region> {
        if t0 >= t1 || t1 as usize > self.start.len() {
            return None;
        }
        let off = self.start[t0 as usize];
        let start_line = self.line(off);
        let col = off - self.line_starts[start_line as usize - 1] + 1;
        let end_line = self.line(self.end[t1 as usize - 1].saturating_sub(1).max(off));
        Some(Region { file, start_line, end_line, col, start_tok: t0, end_tok: t1 })
    }
}

/// Resolves the line ranges of both copies of every candidate (each file is re-read once) and
/// keeps those spanning at least `min_lines` lines in both copies.
fn locate<R>(cands: &[Cand], params: &DupesParams, read: &R, stats: &mut DupesStats) -> Vec<ClonePair>
where
    R: Fn(u32, &mut Vec<u8>) -> bool + Sync,
{
    // (file, t0, t1, candidate * 2 + side)
    let mut refs: Vec<(u32, u32, u32, u32)> = Vec::with_capacity(cands.len() * 2);
    for (i, c) in cands.iter().enumerate() {
        refs.push((c.a_file, c.a0, c.a1, i as u32 * 2));
        refs.push((c.b_file, c.b0, c.b1, i as u32 * 2 + 1));
    }
    refs.par_sort_unstable();
    let by_file: Vec<&[(u32, u32, u32, u32)]> = refs.chunk_by(|x, y| x.0 == y.0).collect();
    stats.files_read += by_file.len() as u64;
    let located: Vec<(Vec<(u32, Region)>, u64)> = by_file
        .par_iter()
        .map_init(LocScratch::default, |sc, rs| {
            let file = rs[0].0;
            let mut out = Vec::new();
            if sc.load(file, read) {
                for &(_, t0, t1, id) in rs.iter() {
                    if let Some(r) = sc.region(file, t0, t1) {
                        out.push((id, r));
                    }
                }
            }
            let bytes = sc.buf.len() as u64;
            if sc.buf.capacity() > 16 << 20 {
                sc.buf = Vec::new();
            }
            (out, bytes)
        })
        .collect();
    drop(by_file);
    drop(refs);
    let mut regions: Vec<Option<Region>> = vec![None; cands.len() * 2];
    for (rs, bytes) in located {
        stats.bytes_read += bytes;
        for (id, r) in rs {
            regions[id as usize] = Some(r);
        }
    }
    let mut pairs = Vec::new();
    for (i, c) in cands.iter().enumerate() {
        if let (Some(a), Some(b)) = (regions[i * 2], regions[i * 2 + 1])
            && a.lines() >= params.min_lines
            && b.lines() >= params.min_lines
        {
            pairs.push(ClonePair { a, b, similarity: c.similarity });
        }
    }
    pairs
}

/// Chains segments that follow each other in both files with gaps of at most [`MAX_GAP`]
/// tokens (in one file, without making the copies overlap). Gap tokens count as different.
fn chain_segments(segs: &mut [Segment], chains: &mut Vec<Segment>, same_file: bool) {
    segs.sort_unstable_by_key(|s| (s.a0, s.b0));
    chains.clear();
    for s in segs.iter() {
        let lo = chains.len().saturating_sub(CHAIN_LOOKBACK);
        let found = chains[lo..].iter_mut().rev().find(|c| {
            s.a0 >= c.a1
                && s.a0 - c.a1 <= MAX_GAP
                && s.b0 >= c.b1
                && s.b0 - c.b1 <= MAX_GAP
                && (!same_file || s.a1 <= c.b0)
        });
        match found {
            Some(c) => {
                c.a1 = s.a1;
                c.b1 = s.b1;
                c.same += s.same;
            }
            None => chains.push(*s),
        }
    }
}

/// Builds members, best partners and groups from the sorted pairs.
///
/// Members: the copies of all pairs sorted by (file, position). A member starts at a copy
/// (its anchor); a later copy that overlaps the anchor by at least half of the shorter of the
/// two joins it, and the member grows to the union. Comparing with the fixed anchor, not the
/// grown union, bounds the growth: a chain of copies each overlapping the next by half (A+B,
/// B+C, C+D in one file) makes several members, not one ever-growing one, so clones between
/// its parts stay visible. Members whose pairs all lie within the member itself (overlapping
/// copies of periodic code) are dropped. Groups: connected components of members linked by
/// pairs.
fn cluster(pairs: Vec<ClonePair>) -> DupesReport {
    // (region, pair index * 2 + side)
    let mut ends: Vec<(Region, u32)> = Vec::with_capacity(pairs.len() * 2);
    for (i, p) in pairs.iter().enumerate() {
        ends.push((p.a, i as u32 * 2));
        ends.push((p.b, i as u32 * 2 + 1));
    }
    ends.sort_unstable_by_key(|(r, i)| (r.file, r.start_tok, r.end_tok, *i));
    let mut members: Vec<Region> = Vec::new();
    let mut anchor: Option<Region> = None;
    // Member of each pair end.
    let mut member_of = vec![0u32; pairs.len() * 2];
    for (r, i) in &ends {
        let joins = anchor.is_some_and(|a| {
            let overlap = a.end_tok.min(r.end_tok).saturating_sub(a.start_tok.max(r.start_tok));
            a.file == r.file && overlap * 2 >= a.tokens().min(r.tokens())
        });
        match members.last_mut() {
            Some(m) if joins => {
                if r.end_tok > m.end_tok {
                    m.end_tok = r.end_tok;
                    m.end_line = m.end_line.max(r.end_line);
                }
            }
            _ => {
                members.push(*r);
                anchor = Some(*r);
            }
        }
        member_of[*i as usize] = members.len() as u32 - 1;
    }
    drop(ends);

    // Best partner per member (the member's own end of the pair first).
    let mut best: Vec<Option<Best>> = vec![None; members.len()];
    for (i, p) in pairs.iter().enumerate() {
        let (ma, mb) = (member_of[i * 2], member_of[i * 2 + 1]);
        if ma != mb {
            offer(&mut best[ma as usize], p.a, p.b, p.similarity);
            offer(&mut best[mb as usize], p.b, p.a, p.similarity);
        }
    }
    // Drop members without a partner; renumber.
    let mut new_id = vec![u32::MAX; members.len()];
    let mut kept = 0usize;
    for m in 0..members.len() {
        if best[m].is_some() {
            new_id[m] = kept as u32;
            members[kept] = members[m];
            best.swap(kept, m);
            kept += 1;
        }
    }
    members.truncate(kept);
    members.shrink_to_fit();
    let best: Vec<Best> = best.into_iter().take(kept).flatten().collect();
    debug_assert_eq!(best.len(), kept);

    // Union-find over members.
    let mut parent: Vec<u32> = (0..members.len() as u32).collect();
    for i in 0..pairs.len() {
        let (ma, mb) = (member_of[i * 2], member_of[i * 2 + 1]);
        if ma == mb {
            continue;
        }
        let (ma, mb) = (new_id[ma as usize], new_id[mb as usize]);
        let (ra, rb) = (find(&mut parent, ma), find(&mut parent, mb));
        if ra != rb {
            // Lower index as root: roots are the first member of their group.
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            parent[hi as usize] = lo;
        }
    }
    drop(member_of);
    drop(new_id);

    // Group ids in order of first member; counting sort of members by group.
    let n = members.len();
    let mut group = vec![u32::MAX; n];
    let mut groups: Vec<(u32, u32)> = Vec::new();
    for m in 0..n {
        let r = find(&mut parent, m as u32) as usize;
        if group[r] == u32::MAX {
            group[r] = groups.len() as u32;
            groups.push((0, 0));
        }
        group[m] = group[r];
        groups[group[m] as usize].1 += 1;
    }
    let mut at = 0u32;
    for g in &mut groups {
        let size = g.1;
        *g = (at, at);
        at += size;
    }
    let mut group_members = vec![0u32; n];
    for (m, &g) in group.iter().enumerate() {
        let slot = &mut groups[g as usize].1;
        group_members[*slot as usize] = m as u32;
        *slot += 1;
    }
    DupesReport { pairs, members, best, group, groups, group_members, stats: DupesStats::default() }
}

/// Keeps the more similar pair (ties: the larger own copy, then the earlier partner).
#[inline]
fn offer(best: &mut Option<Best>, this: Region, other: Region, similarity: u8) {
    let better = match best {
        None => true,
        Some(b) => {
            let key = |r: &Region| (u32::MAX - r.tokens(), r.start_tok);
            (u32::MAX - u32::from(similarity), key(&this), other.file, other.start_tok)
                < (u32::MAX - u32::from(b.similarity), key(&b.this), b.other.file, b.other.start_tok)
        }
    };
    if better {
        *best = Some(Best { this, other, similarity });
    }
}

/// Union-find root with path halving.
#[inline]
fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let gp = parent[parent[x as usize] as usize];
        parent[x as usize] = gp;
        x = gp;
    }
    x
}

/// Runs that are reported only as exact copies: data (fewer than one token in [`WORD_SHARE`]
/// is a name or keyword, e.g. tables of tuples or dict literals) or [`repetitive`] code.
fn boilerplate(codes: &[u8]) -> bool {
    let words = codes.iter().filter(|&&c| is_word(c)).count();
    words * WORD_SHARE < codes.len() || repetitive(codes)
}

/// The code of a name or a keyword.
#[inline]
fn is_word(c: u8) -> bool {
    c == TokenKind::Name as u8 || (TokenKind::False as u8..=TokenKind::Yield as u8).contains(&c)
}

/// The run is (nearly) periodic with a short period: at least [`REPEAT_PCT`]% of its positions
/// repeat the code [`MAX_PERIOD`] or fewer positions earlier.
fn repetitive(codes: &[u8]) -> bool {
    let n = codes.len();
    for p in 1..=MAX_PERIOD.min(n / 2) {
        let same = codes[p..].iter().zip(codes).filter(|(x, y)| x == y).count();
        if same * 100 >= (n - p) * REPEAT_PCT {
            return true;
        }
    }
    false
}
