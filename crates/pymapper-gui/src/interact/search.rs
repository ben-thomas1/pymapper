//! Fuzzy search-and-jump over every package, module, class and function.
//!
//! ```text
//!   "pkg.mod:Cls.meth"  ──split──>  prefix "pkg.mod:Cls"   name "meth"
//!   name:   fuzzy-score every DISTINCT node name once (parallel), keep the best names
//!   expand: the nodes of each kept name (core NameIndex, binary search)
//!   prefix: fuzzy-score the node's parent qualified id (only for expanded candidates)
//!   rank:   name score + prefix score + origin / kind bonuses, ties by node id
//! ```
//!
//! An exact qualified id (`Index::find`) always ranks first. Cost per query: O(distinct
//! names) scoring in parallel, plus O(kept candidates x depth) for qualified ids; no
//! per-node strings are built except the parent ids of kept candidates.

use rayon::prelude::*;

use pymapper_core::index::{Index, NodeId, NodeKind};
use pymapper_core::intern::Sym;
use pymapper_core::query::Query;
use pymapper_core::roots::Origin;

use super::fuzzy::Pattern;

/// Distinct names kept after the name pass (their nodes are then ranked).
pub const MAX_NAMES: usize = 1024;
/// Candidate nodes ranked at most (the best names' nodes first).
pub const MAX_CANDIDATES: usize = 32_768;

/// One search result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub node: NodeId,
    pub score: i32,
}

/// The distinct node names of an index, built once (O(V), 4 bytes per distinct name).
#[derive(Clone, Debug, Default)]
pub struct SearchIndex {
    names: Vec<Sym>,
}

impl SearchIndex {
    pub fn build(q: &Query<'_>) -> SearchIndex {
        let ix = q.index();
        SearchIndex { names: q.names().runs(ix).map(|(s, _)| s).collect() }
    }

    pub fn distinct_names(&self) -> usize {
        self.names.len()
    }

    pub fn heap_bytes(&self) -> usize {
        self.names.capacity() * 4
    }

    /// The best `limit` matches of `text` (see the module docs), best first.
    pub fn search(&self, q: &Query<'_>, text: &str, limit: usize) -> Vec<SearchHit> {
        let ix = q.index();
        let text = text.trim();
        if text.is_empty() || limit == 0 {
            return Vec::new();
        }
        let exact = ix.find(text.as_bytes());
        let cut = text.rfind(['.', ':']);
        let (prefix, name) = match cut {
            Some(i) => (&text[..i], &text[i + 1..]),
            None => ("", text),
        };
        let name_pat = Pattern::new(name.as_bytes());
        let prefix_pat = Pattern::new(prefix.as_bytes());
        // Name pass: every distinct name once.
        let mut names: Vec<(i32, Sym)> = if name_pat.is_empty() {
            Vec::new()
        } else {
            self.names.par_iter().filter_map(|&s| name_pat.score(ix.str(s)).map(|sc| (sc, s))).collect()
        };
        let by_score = |a: &(i32, Sym), b: &(i32, Sym)| b.0.cmp(&a.0).then(a.1.index().cmp(&b.1.index()));
        if names.len() > MAX_NAMES {
            names.select_nth_unstable_by(MAX_NAMES - 1, by_score);
            names.truncate(MAX_NAMES);
        }
        names.sort_unstable_by(by_score);
        // Expand and rank.
        let mut hits: Vec<SearchHit> = Vec::new();
        let mut qid = Vec::new();
        let ni = q.names();
        'outer: for &(sc, s) in &names {
            for &v in ni.of(ix, s) {
                if hits.len() >= MAX_CANDIDATES {
                    break 'outer;
                }
                let mut score = sc + bonus(q, ix, v);
                if !prefix_pat.is_empty() {
                    qid.clear();
                    match ix.parent(v) {
                        Some(p) => ix.qid(p, &mut qid),
                        None => continue,
                    }
                    match prefix_pat.score(&qid) {
                        Some(ps) => score += ps,
                        None => continue,
                    }
                }
                hits.push(SearchHit { node: v, score });
            }
        }
        // A trailing separator ("pkg.mod.") lists the children of the prefix's node; a prefix
        // that names no node is searched as a name instead.
        if name_pat.is_empty() && !prefix_pat.is_empty() {
            match ix.find(prefix.as_bytes()) {
                Some(p) => hits.extend(ix.children(p).map(|v| SearchHit { node: v, score: bonus(q, ix, v) })),
                None => return self.search(q, prefix, limit),
            }
        }
        if let Some(e) = exact {
            hits.retain(|h| h.node != e);
            hits.push(SearchHit { node: e, score: i32::MAX });
        }
        let key = |h: &SearchHit| (std::cmp::Reverse(h.score), h.node);
        if hits.len() > limit {
            hits.select_nth_unstable_by_key(limit - 1, key);
            hits.truncate(limit);
        }
        hits.sort_unstable_by_key(key);
        hits
    }
}

/// Ranking bonus of a node: first-party first, definitions and modules before packages,
/// shallower before deeper.
fn bonus(q: &Query<'_>, ix: &Index, v: NodeId) -> i32 {
    let origin = match q.symbol(v).origin() {
        Origin::FirstParty => 24,
        Origin::ThirdParty => 0,
        Origin::Stdlib => -4,
    };
    let kind = match ix.kind(v) {
        NodeKind::Class => 6,
        NodeKind::Function | NodeKind::Module => 4,
        NodeKind::Package => 2,
        _ => 0,
    };
    origin + kind - ix.depth(v).min(16) as i32
}
