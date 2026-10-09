//! Symbol lookup: exact qualified id, dotted qualified name, segment suffix, fuzzy.
//!
//! Forms accepted, tried in order (every form stops at the first that matches, except suffix
//! matches, which are all returned):
//!
//! 1. Exact qualified id: `@dist`, `pkg.mod`, `pkg.mod:Class.method`, `#N` suffixes
//!    ([`Index::find`]).
//! 2. Qualified dotted name: `pkg.mod.Class.method`. The longest prefix that names a
//!    package/module is the module part; the rest walks defs. One hash lookup per prefix.
//! 3. Suffix: the last segments of a qualified name (`method`, `Class.method`,
//!    `mod.Class.method`; `:` counts as `.`). Candidates come from the [`NameIndex`] by the last
//!    segment's name; each is checked against the earlier segments by walking up its parents.
//! 4. Fuzzy (only when nothing else matched): the last segment against every distinct node
//!    name, case-insensitively: equal, prefix, substring, then edit distance (optimal string
//!    alignment, at most a third of the length). Ranked by that order.
//!
//! Matches are ranked by form, fuzzy score, origin (first-party, third-party, stdlib), then
//! node order.

use std::collections::BinaryHeap;
use std::ops::Range;
use std::path::Path;

use rayon::prelude::*;

use super::Query;
use crate::index::qid::split_dup;
use crate::index::{FileId, Index, NodeId, NodeKind};
use crate::intern::Sym;
use crate::roots::Origin;

/// How a lookup matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MatchKind {
    /// The input is the node's qualified id.
    Exact,
    /// The input is the node's dotted qualified name (`pkg.mod.Class.method`).
    Qualified,
    /// The input is a segment suffix of the node's qualified id.
    Suffix,
    /// Approximate match of the last segment ([`Match::score`]: lower is closer).
    Fuzzy,
}

impl MatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchKind::Exact => "exact",
            MatchKind::Qualified => "qualified",
            MatchKind::Suffix => "suffix",
            MatchKind::Fuzzy => "fuzzy",
        }
    }
}

/// One lookup result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    pub node: NodeId,
    pub kind: MatchKind,
    /// Fuzzy rank: `tier << 16 | detail` (tier 0 case-insensitive equal, 1 prefix, 2
    /// substring, 3 edit distance); 0 for the other kinds.
    pub score: u32,
}

/// Options of [`Query::lookup`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LookupOptions {
    /// Most matches returned (the best ones).
    pub limit: usize,
    /// Try fuzzy matching when nothing else matched.
    pub fuzzy: bool,
}

impl Default for LookupOptions {
    fn default() -> Self {
        LookupOptions { limit: 20, fuzzy: true }
    }
}

/// Every package, module, class and function node sorted by (name `Sym`, node): the nodes
/// with one name are a contiguous run found by binary search. Built by an LSD radix sort on
/// the `Sym` index (11-bit digits, stable, so runs stay in node order): O(V) per digit, `4 V`
/// bytes kept, `4 V` transient.
#[derive(Clone, Debug, Default)]
pub struct NameIndex {
    nodes: Vec<NodeId>,
}

const DIGIT: u32 = 11;

impl NameIndex {
    pub fn build(ix: &Index) -> NameIndex {
        let n = ix.node_count();
        let mut nodes: Vec<NodeId> = Vec::with_capacity(n as usize);
        let mut max = 0u32;
        for v in 0..n {
            if ix.kind(v) != NodeKind::Distribution {
                nodes.push(v);
                max = max.max(ix.name_sym(v).index() as u32);
            }
        }
        let mut keys: Vec<u32> = nodes.iter().map(|&v| ix.name_sym(v).index() as u32).collect();
        let mut tmp_n = vec![0u32; nodes.len()];
        let mut tmp_k = vec![0u32; nodes.len()];
        let mut count = vec![0u32; 1 << DIGIT];
        let mut shift = 0;
        while shift < 32 && (shift == 0 || max >> shift != 0) {
            count.fill(0);
            for &k in &keys {
                count[((k >> shift) & ((1 << DIGIT) - 1)) as usize] += 1;
            }
            let mut acc = 0;
            for c in count.iter_mut() {
                let x = *c;
                *c = acc;
                acc += x;
            }
            for (&k, &v) in keys.iter().zip(&nodes) {
                let d = ((k >> shift) & ((1 << DIGIT) - 1)) as usize;
                tmp_k[count[d] as usize] = k;
                tmp_n[count[d] as usize] = v;
                count[d] += 1;
            }
            std::mem::swap(&mut keys, &mut tmp_k);
            std::mem::swap(&mut nodes, &mut tmp_n);
            shift += DIGIT;
        }
        drop((keys, tmp_k, tmp_n));
        NameIndex { nodes }
    }

    /// Nodes of `ix` named `sym`, ascending. O(log V).
    pub fn of(&self, ix: &Index, sym: Sym) -> &[NodeId] {
        let k = sym.index();
        let key = |v: &NodeId| ix.name_sym(*v).index();
        let lo = self.nodes.partition_point(|v| key(v) < k);
        let hi = lo + self.nodes[lo..].partition_point(|v| key(v) == k);
        &self.nodes[lo..hi]
    }

    /// Distinct names with their nodes, by `Sym` index (`ix`: the index it was built from).
    pub fn runs<'s>(&'s self, ix: &'s Index) -> impl Iterator<Item = (Sym, &'s [NodeId])> + 's {
        self.runs_in(ix, 0..self.nodes.len())
    }

    /// The runs starting in `range` of the sorted nodes (`range.start` must start a run).
    fn runs_in<'s>(&'s self, ix: &'s Index, range: Range<usize>) -> impl Iterator<Item = (Sym, &'s [NodeId])> + 's {
        let mut i = range.start;
        std::iter::from_fn(move || {
            if i >= range.end {
                return None;
            }
            let sym = ix.name_sym(self.nodes[i]);
            let mut j = i + 1;
            while j < self.nodes.len() && ix.name_sym(self.nodes[j]) == sym {
                j += 1;
            }
            let run = &self.nodes[i..j];
            i = j;
            Some((sym, run))
        })
    }

    /// About `k` ranges of the sorted nodes covering them, each starting a run.
    fn chunks(&self, ix: &Index, k: usize) -> Vec<Range<usize>> {
        let n = self.nodes.len();
        let step = n.div_ceil(k.max(1)).max(1);
        let mut out = Vec::with_capacity(k);
        let mut start = 0;
        while start < n {
            let mut end = (start + step).min(n);
            while end < n && ix.name_sym(self.nodes[end]) == ix.name_sym(self.nodes[end - 1]) {
                end += 1;
            }
            out.push(start..end);
            start = end;
        }
        out
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn heap_bytes(&self) -> usize {
        self.nodes.capacity() * 4
    }
}

/// Outcome of [`Query::resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// One node: an exact or qualified match, or the only suffix match.
    Unique(NodeId),
    /// Several suffix matches (ranked).
    Ambiguous(Vec<Match>),
    /// No match; fuzzy suggestions (possibly empty).
    NotFound(Vec<Match>),
}

/// Rank of an origin (first-party first).
fn origin_rank(o: Origin) -> u8 {
    match o {
        Origin::FirstParty => 0,
        Origin::ThirdParty => 1,
        Origin::Stdlib => 2,
    }
}

/// Splits on `.` and `:`.
fn segments(q: &[u8]) -> impl DoubleEndedIterator<Item = &[u8]> {
    q.split(|&c| c == b'.' || c == b':')
}

impl Query<'_> {
    /// Looks `q` up (see the module docs), best matches first.
    pub fn lookup(&self, q: &[u8], opts: &LookupOptions) -> Vec<Match> {
        let ix = self.ix;
        let mut out = Vec::new();
        if q.is_empty() || opts.limit == 0 {
            return out;
        }
        if let Some(v) = ix.find(q) {
            out.push(Match { node: v, kind: MatchKind::Exact, score: 0 });
        } else if !q.contains(&b':')
            && let Some(v) = self.qualified(q)
        {
            out.push(Match { node: v, kind: MatchKind::Qualified, score: 0 });
        }
        if q.starts_with(b"@") {
            return out;
        }
        let first = out.first().map(|m| m.node);
        let mut sfx = self.suffix(q);
        sfx.retain(|&v| Some(v) != first);
        self.rank(&mut sfx, opts.limit.saturating_sub(out.len()));
        out.extend(sfx.into_iter().map(|v| Match { node: v, kind: MatchKind::Suffix, score: 0 }));
        if out.is_empty() && opts.fuzzy {
            out = self.fuzzy(q, opts.limit);
        }
        out
    }

    /// Resolves `q` to one node when the match is unambiguous (see [`Resolved`]).
    pub fn resolve(&self, q: &[u8], opts: &LookupOptions) -> Resolved {
        let m = self.lookup(q, opts);
        match m.first() {
            None => Resolved::NotFound(m),
            Some(f) if f.kind == MatchKind::Fuzzy => Resolved::NotFound(m),
            Some(f) if f.kind != MatchKind::Suffix || m.len() == 1 => Resolved::Unique(f.node),
            Some(_) => Resolved::Ambiguous(m),
        }
    }

    /// Sorts nodes by (origin, node) and keeps the first `limit`.
    fn rank(&self, v: &mut Vec<NodeId>, limit: usize) {
        let key = |&n: &NodeId| (origin_rank(self.symbol(n).origin()), n);
        if v.len() > limit && limit > 0 {
            v.select_nth_unstable_by_key(limit - 1, key);
        }
        v.truncate(limit);
        v.sort_unstable_by_key(key);
    }

    /// The node of dotted name `q` (`pkg.mod.Class.method`): longest module prefix first.
    fn qualified(&self, q: &[u8]) -> Option<NodeId> {
        let ix = self.ix;
        let mut cut = q.len();
        loop {
            let (module, rest) = (&q[..cut], q.get(cut + 1..).unwrap_or(&[]));
            if let Some((dotted, dup)) = split_dup(module)
                && let Some(m) = ix.module_node(dotted, dup)
                && let Some(v) = self.defs_below(m, rest)
            {
                return Some(v);
            }
            cut = q[..cut].iter().rposition(|&c| c == b'.')?;
        }
    }

    /// Walks the dotted def path `rest` below `m` (empty: `m`).
    fn defs_below(&self, m: NodeId, rest: &[u8]) -> Option<NodeId> {
        let ix = self.ix;
        let mut cur = m;
        if rest.is_empty() {
            return Some(cur);
        }
        for seg in rest.split(|&c| c == b'.') {
            let (name, dup) = split_dup(seg)?;
            let sym = ix.strings().get(name)?;
            cur = ix.children(cur).find(|&c| ix.kind(c).is_def() && ix.name_sym(c) == sym && ix.dup(c) == dup)?;
        }
        Some(cur)
    }

    /// Nodes whose qualified id ends with the segments of `q`, ascending.
    fn suffix(&self, q: &[u8]) -> Vec<NodeId> {
        let ix = self.ix;
        // (name, dup) of every segment, last first; any unknown name means no match.
        let mut segs: Vec<(Sym, u16)> = Vec::new();
        for seg in segments(q).rev() {
            let Some((name, dup)) = split_dup(seg) else { return Vec::new() };
            let Some(sym) = ix.strings().get(name) else { return Vec::new() };
            segs.push((sym, dup));
        }
        let Some(&(last, dup)) = segs.first() else { return Vec::new() };
        let mut out = Vec::new();
        'cand: for &c in self.names().of(ix, last) {
            if ix.dup(c) != dup {
                continue;
            }
            let mut p = c;
            for &(sym, dup) in &segs[1..] {
                match ix.parent(p) {
                    Some(a) if ix.kind(a) != NodeKind::Distribution && ix.name_sym(a) == sym && ix.dup(a) == dup => p = a,
                    _ => continue 'cand,
                }
            }
            out.push(c);
        }
        out
    }

    /// Fuzzy matches of the last segment of `q` (see the module docs), best `limit`.
    fn fuzzy(&self, q: &[u8], limit: usize) -> Vec<Match> {
        let ix = self.ix;
        let last = segments(q).next_back().unwrap_or(q);
        let last = split_dup(last).map_or(last, |(n, _)| n);
        if last.is_empty() {
            return Vec::new();
        }
        let lower: Vec<u8> = last.to_ascii_lowercase();
        let fq = Fuzzy { mask: char_mask(&lower), max_d: (lower.len() / 3).max(1), lower };
        // In parallel over run-aligned chunks of the name index, each keeping its best `limit`
        // (score, origin rank, node) in a bounded heap (root: the weakest kept); then merged.
        let names = self.names();
        let chunks = names.chunks(ix, 4 * rayon::current_num_threads());
        let heap = chunks
            .into_par_iter()
            .map(|range| {
                let mut rows = Vec::new();
                let mut heap: BinaryHeap<(u32, u8, NodeId)> = BinaryHeap::with_capacity(limit + 1);
                for (sym, run) in names.runs_in(ix, range) {
                    let Some(score) = fuzzy_score(&fq, ix.str(sym), &mut rows) else { continue };
                    if heap.len() == limit && heap.peek().is_some_and(|w| w.0 < score) {
                        continue;
                    }
                    for &v in run {
                        push_bounded(&mut heap, limit, (score, origin_rank(self.symbol(v).origin()), v));
                    }
                }
                heap
            })
            .reduce(BinaryHeap::new, |mut a, b| {
                for item in b {
                    push_bounded(&mut a, limit, item);
                }
                a
            });
        heap.into_sorted_vec().into_iter().map(|(score, _, node)| Match { node, kind: MatchKind::Fuzzy, score }).collect()
    }

    /// The innermost def of file `f` whose line span contains `line`, else the file's
    /// package/module node. O(defs of the file).
    pub fn symbol_at(&self, f: FileId, line: u32) -> NodeId {
        let ix = self.ix;
        let ff = ix.file_facts(f);
        let defs = &ix.facts().defs;
        let mut best = 0u32;
        // Defs are in pre-order: a later containing def is nested in an earlier one.
        for d in 1..ff.defs.len {
            let r = (ff.defs.start + d) as usize;
            if defs.start_line[r] <= line && line <= defs.end_line[r] {
                best = d;
            }
        }
        ix.def_node(f, best)
    }

    /// The file at `path`: an exact match, else the only file whose path ends with `path`
    /// (whole components). O(files).
    pub fn file_by_path(&self, path: &Path) -> Option<FileId> {
        let ix = self.ix;
        let mut found = None;
        for f in 0..ix.file_count() {
            let p = ix.file_path(f);
            if p == path {
                return Some(f);
            }
            if p.ends_with(path) {
                if found.is_some() {
                    return None;
                }
                found = Some(f);
            }
        }
        found
    }
}

/// Keeps the `limit` smallest items in a max-heap.
#[inline]
fn push_bounded<T: Ord>(heap: &mut BinaryHeap<T>, limit: usize, item: T) {
    if heap.len() < limit {
        heap.push(item);
    } else if let Some(mut weakest) = heap.peek_mut()
        && item < *weakest
    {
        *weakest = item;
    }
}

/// Fuzzy score of `name` against the lower-case query (see [`Match::score`]); `None` when
/// too far.
fn fuzzy_score(q: &Fuzzy, name: &[u8], rows: &mut Vec<u32>) -> Option<u32> {
    let (ql, nl) = (q.lower.len(), name.len());
    // No tier applies to a name shorter than the query by more than the edit budget.
    if nl + q.max_d < ql {
        return None;
    }
    // Every distinct query byte missing from the name costs at least one edit, and the exact
    // tiers need none missing: most names stop here, after one pass over their bytes.
    let missing = (q.mask & !char_mask(name)).count_ones() as usize;
    if missing > q.max_d {
        return None;
    }
    let eq = |a: u8, b: u8| a == b.to_ascii_lowercase();
    if missing == 0 && nl >= ql {
        if q.lower.iter().zip(name).all(|(&a, &b)| eq(a, b)) {
            return Some(if nl == ql { 0 } else { (1 << 16) | (nl - ql).min(0xffff) as u32 });
        }
        if let Some(pos) = name.windows(ql).position(|w| q.lower.iter().zip(w).all(|(&a, &b)| eq(a, b))) {
            return Some((2 << 16) | pos.min(0xffff) as u32);
        }
    }
    if nl.abs_diff(ql) > q.max_d {
        return None;
    }
    let d = osa_bounded(&q.lower, name, q.max_d, rows);
    (d <= q.max_d).then_some((3 << 16) | d as u32)
}

/// A fuzzy query: the lower-case last segment, its byte set and the edit budget.
struct Fuzzy {
    lower: Vec<u8>,
    mask: u64,
    max_d: usize,
}

/// Set of the (lower-cased) bytes of `s` folded into 64 bits: letters, digits and `_` get a
/// bit each (37 bits), other bytes share the remaining 27 by value. A superset test on these
/// masks is a necessary condition for "contains every byte" (shared bits only weaken it).
#[inline]
fn char_mask(s: &[u8]) -> u64 {
    let mut m = 0u64;
    for &c in s {
        let bit = match c.to_ascii_lowercase() {
            c @ b'a'..=b'z' => c - b'a',
            c @ b'0'..=b'9' => 26 + c - b'0',
            b'_' => 36,
            c => 37 + c % 27,
        };
        m |= 1 << bit;
    }
    m
}

/// Optimal string alignment distance between the lower-case `a` and `b` (compared
/// lower-case), capped: `max_d + 1` when it is larger. Only the band `|i - j| <= max_d` of the
/// table is computed (cells outside it are above the cap), and the search stops at the first
/// row whose minimum is above the cap (row minima never decrease, transpositions included).
/// O(len * max_d); `rows` is reused scratch (three rolling rows plus the lower-cased `b`).
fn osa_bounded(a: &[u8], b: &[u8], max_d: usize, rows: &mut Vec<u32>) -> usize {
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > max_d {
        return max_d + 1;
    }
    if n == 0 || m == 0 {
        return n.max(m);
    }
    let inf = (max_d + 1) as u32;
    let w = m + 1;
    rows.clear();
    rows.resize(3 * w + m, inf);
    let (dp, lb) = rows.split_at_mut(3 * w);
    for (d, &c) in lb.iter_mut().zip(b) {
        *d = u32::from(c.to_ascii_lowercase());
    }
    for j in 0..=m.min(max_d) {
        dp[w + j] = j as u32;
    }
    // Offsets of rows i - 2, i - 1 and i.
    let (mut p2, mut p1, mut cu) = (0, w, 2 * w);
    for i in 1..=n {
        let lo = i.saturating_sub(max_d).max(1);
        let hi = (i + max_d).min(m);
        dp[cu..cu + w].fill(inf);
        if i <= max_d {
            dp[cu] = i as u32;
        }
        let mut row_min = dp[cu];
        let ai = u32::from(a[i - 1]);
        for j in lo..=hi {
            let cost = u32::from(ai != lb[j - 1]);
            let mut v = (dp[p1 + j] + 1).min(dp[cu + j - 1] + 1).min(dp[p1 + j - 1] + cost);
            if i > 1 && j > 1 && ai == lb[j - 2] && u32::from(a[i - 2]) == lb[j - 1] {
                v = v.min(dp[p2 + j - 2] + 1);
            }
            let v = v.min(inf);
            dp[cu + j] = v;
            row_min = row_min.min(v);
        }
        if row_min >= inf {
            return max_d + 1;
        }
        (p2, p1, cu) = (p1, cu, p2);
    }
    dp[p1 + m] as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }

        fn string(&mut self, alphabet: &[u8], max_len: usize, lower: bool) -> Vec<u8> {
            let n = self.below(max_len + 1);
            (0..n)
                .map(|_| {
                    let c = alphabet[self.below(alphabet.len())];
                    if lower { c.to_ascii_lowercase() } else { c }
                })
                .collect()
        }
    }

    const ALPHABET: &[u8] = b"abcdeABCDE_019p\xc3";

    /// Full-table optimal string alignment distance (the reference of [`osa_bounded`]).
    fn osa_distance(a: &[u8], b: &[u8]) -> usize {
        let w = b.len() + 1;
        let mut rows = vec![0usize; 3 * w];
        for (j, cell) in rows[w..2 * w].iter_mut().enumerate() {
            *cell = j;
        }
        let lb = |j: usize| b[j].to_ascii_lowercase();
        for i in 1..=a.len() {
            let (prev2, rest) = rows.split_at_mut(w);
            let (prev, cur) = rest.split_at_mut(w);
            cur[0] = i;
            for j in 1..=b.len() {
                let cost = usize::from(a[i - 1] != lb(j - 1));
                let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
                if i > 1 && j > 1 && a[i - 1] == lb(j - 2) && a[i - 2] == lb(j - 1) {
                    v = v.min(prev2[j - 2] + 1);
                }
                cur[j] = v;
            }
            rows.copy_within(w..3 * w, 0);
        }
        rows[2 * w - 1]
    }

    /// The scoring without the byte-set prefilter and the band (the reference).
    fn reference(q: &[u8], name: &[u8], max_d: usize) -> Option<u32> {
        let eq = |a: u8, b: u8| a == b.to_ascii_lowercase();
        let (ql, nl) = (q.len(), name.len());
        if nl == ql && q.iter().zip(name).all(|(&a, &b)| eq(a, b)) {
            return Some(0);
        }
        if nl > ql {
            if q.iter().zip(name).all(|(&a, &b)| eq(a, b)) {
                return Some((1 << 16) | (nl - ql) as u32);
            }
            if let Some(pos) = name.windows(ql).position(|w| q.iter().zip(w).all(|(&a, &b)| eq(a, b))) {
                return Some((2 << 16) | pos as u32);
            }
        }
        let d = osa_distance(q, name);
        (d <= max_d).then_some((3 << 16) | d as u32)
    }

    #[test]
    fn prefilter_and_band_never_change_a_score() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let mut rows = Vec::new();
        let mut hits = [0usize; 4];
        for _ in 0..200_000 {
            let q = rng.string(ALPHABET, 7, true);
            if q.is_empty() {
                continue;
            }
            let name = rng.string(ALPHABET, 10, false);
            let fq = Fuzzy { mask: char_mask(&q), max_d: (q.len() / 3).max(1), lower: q.clone() };
            let want = reference(&q, &name, fq.max_d);
            assert_eq!(fuzzy_score(&fq, &name, &mut rows), want, "{q:?} {name:?}");
            if let Some(s) = want {
                hits[(s >> 16) as usize] += 1;
            }
        }
        assert!(hits.iter().all(|&h| h > 100), "{hits:?}");
    }

    #[test]
    fn banded_distance_matches_the_full_table() {
        let mut rng = Rng(0x0dd_ba11);
        let mut rows = Vec::new();
        for _ in 0..100_000 {
            let a = rng.string(ALPHABET, 12, true);
            let b = rng.string(ALPHABET, 12, false);
            let max_d = rng.below(6);
            let full = osa_distance(&a, &b);
            assert_eq!(osa_bounded(&a, &b, max_d, &mut rows), full.min(max_d + 1), "{a:?} {b:?} {max_d}");
        }
        for (a, b, d) in [("", "", 0), ("abc", "", 3), ("abc", "abc", 0), ("abc", "ABC", 0), ("ab", "ba", 1), ("helper", "helpr", 1), ("kitten", "sitting", 3)] {
            assert_eq!(osa_distance(a.as_bytes(), b.as_bytes()), d, "{a} {b}");
            assert_eq!(osa_bounded(a.as_bytes(), b.as_bytes(), 3, &mut rows), d, "{a} {b}");
        }
    }
}
