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

use std::path::Path;

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
        let mut i = 0;
        std::iter::from_fn(move || {
            let &first = self.nodes.get(i)?;
            let sym = ix.name_sym(first);
            let mut j = i + 1;
            while j < self.nodes.len() && ix.name_sym(self.nodes[j]) == sym {
                j += 1;
            }
            let run = &self.nodes[i..j];
            i = j;
            Some((sym, run))
        })
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
        let max_d = (lower.len() / 3).max(1);
        let mut rows = Vec::new();
        // (score, origin rank, node).
        let mut hits: Vec<(u32, u8, NodeId)> = Vec::new();
        for (sym, run) in self.names().runs(ix) {
            let Some(score) = fuzzy_score(&lower, ix.str(sym), max_d, &mut rows) else { continue };
            for &v in run {
                hits.push((score, origin_rank(self.symbol(v).origin()), v));
            }
        }
        if hits.len() > limit && limit > 0 {
            hits.select_nth_unstable(limit - 1);
        }
        hits.truncate(limit);
        hits.sort_unstable();
        hits.into_iter().map(|(score, _, node)| Match { node, kind: MatchKind::Fuzzy, score }).collect()
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

/// Fuzzy score of `name` against the lower-case query (see [`Match::score`]); `None` when
/// too far.
fn fuzzy_score(q: &[u8], name: &[u8], max_d: usize, rows: &mut Vec<usize>) -> Option<u32> {
    let eq = |a: u8, b: u8| a == b.to_ascii_lowercase();
    if name.len() == q.len() && q.iter().zip(name).all(|(&a, &b)| eq(a, b)) {
        return Some(0);
    }
    if name.len() > q.len() {
        if q.iter().zip(name).all(|(&a, &b)| eq(a, b)) {
            return Some((1 << 16) | (name.len() - q.len()).min(0xffff) as u32);
        }
        if let Some(pos) = name.windows(q.len()).position(|w| q.iter().zip(w).all(|(&a, &b)| eq(a, b))) {
            return Some((2 << 16) | pos.min(0xffff) as u32);
        }
    }
    if name.len().abs_diff(q.len()) > max_d {
        return None;
    }
    let d = osa_distance(q, name, rows);
    (d <= max_d).then_some((3 << 16) | d as u32)
}

/// Optimal string alignment distance between the lower-case `a` and `b` (compared lower-case),
/// with three reused rolling rows.
fn osa_distance(a: &[u8], b: &[u8], rows: &mut Vec<usize>) -> usize {
    if a.is_empty() || b.is_empty() {
        return a.len().max(b.len());
    }
    let w = b.len() + 1;
    rows.clear();
    rows.resize(3 * w, 0);
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
