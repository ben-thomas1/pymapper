//! Third-party scorecards and first-party usage of distributions (design: Checks / CLI,
//! "Third-party"). Owned by the deps step; drives `pymapper deps`.
//!
//! ```text
//!   Index + Imports (deps loaded) ──> GroupFiles       files bucketed per group (one pass)
//!     ├─> scorecards (per third-party group, only with --score)
//!     │     ScoreStore (cache dir, `scores.pmc`): hit by distribution cache key ──> Scorecard
//!     │     miss ──> score::compute(group)        one pass over the group's def rows and
//!     │                                           module edges, O(defs + edges) of the group
//!     │     ──> ScoreStore::save                  rewritten only when an entry was computed
//!     │                                           or dropped
//!     └─> usage (per run, never cached)
//!           Imports of first-party files          importers, import rows, modules used
//!           ResolvedCalls + import targets        API surface used (distinct symbols)
//!           call graph ∪ import graph             LOC reached: one multi-source BFS from
//!                                                 every first-party node, O(V + E)
//!   ──> DepsReport ──> concise | json (schema `version` 2, as `check`)
//! ```
//!
//! Scorecards never produce findings and never affect any exit code. See `docs/deps.md` for
//! the metrics and the grade formula, and [`score`] for their exact definitions.
//!
//! # Cost
//!
//! Scorecards: O(defs + module edges) over the scored distributions, single pass, a few
//! column reads per function (no allocation per def); cached per distribution, keyed like the
//! distribution's facts segment (normalized name, version, `RECORD` hash) plus the
//! scorecard version, so a warm run computes none. Usage: one pass over the first-party
//! import rows and over the call edges, one bit per node for the symbols used, and a BFS
//! over the union of the call graph (edges at least `likely`) and the runtime import graph:
//! O(V + E), 8 bytes per edge for the graph plus 9 bytes per node. Call resolution of the
//! whole program (third-party code included) dominates; see "Deps (S6.1)" in
//! `docs/benchmarks.md`.

mod report;
mod score;
mod store;
mod usage;

#[cfg(test)]
mod tests;

pub use report::{DepRow, DepsReport, ReportOptions};
pub use score::{Grade, SCORE_VERSION, ScoreCtx, Scorecard, Scratch, compute as compute_scorecard};
pub use store::{SCORES_FILE, ScoreStats, ScoreStore};
pub use usage::{DistUsage, Usage, UsageOptions, usage};

use rayon::prelude::*;

use crate::index::{FileId, Group, Index};
use crate::roots::{Origin, Roots};

/// Files of every group, bucketed in one pass (counting sort, FileId order within a group).
#[derive(Clone, Debug, Default)]
pub struct GroupFiles {
    start: Vec<u32>,
    files: Vec<FileId>,
}

impl GroupFiles {
    pub fn new(ix: &Index) -> GroupFiles {
        let groups = ix.groups().len();
        let mut start = vec![0u32; groups + 1];
        for f in 0..ix.file_count() {
            start[ix.file_group(f) as usize + 1] += 1;
        }
        for g in 0..groups {
            start[g + 1] += start[g];
        }
        let mut next = start.clone();
        let mut files = vec![0; ix.file_count() as usize];
        for f in 0..ix.file_count() {
            let g = ix.file_group(f) as usize;
            files[next[g] as usize] = f;
            next[g] += 1;
        }
        GroupFiles { start, files }
    }

    /// Files of group `g`, ascending.
    #[inline]
    pub fn of(&self, g: u32) -> &[FileId] {
        &self.files[self.start[g as usize] as usize..self.start[g as usize + 1] as usize]
    }
}

/// The scorecard of every third-party group (`None` for the other groups): from `store` when
/// cached there under `key(group)` (normally [`ScoreStore::key`]), else computed (in
/// parallel, one [`Scratch`] per worker) and recorded in it.
pub fn scorecards(cx: &ScoreCtx<'_>, key: impl Fn(&Group) -> Option<u64>, store: &mut ScoreStore) -> Vec<Option<Scorecard>> {
    let groups = cx.ix.groups();
    let mut out: Vec<Option<Scorecard>> = vec![None; groups.len()];
    let mut missing: Vec<(u32, Option<u64>)> = Vec::new();
    for (g, grp) in groups.iter().enumerate() {
        if grp.origin != Origin::ThirdParty {
            continue;
        }
        let key = key(grp);
        match key.and_then(|k| store.get(k)) {
            Some(sc) => out[g] = Some(sc),
            None => missing.push((g as u32, key)),
        }
    }
    let computed: Vec<Scorecard> =
        missing.par_iter().map_init(Scratch::default, |s, &(g, _)| score::compute(cx, g, s)).collect();
    for (&(g, key), sc) in missing.iter().zip(computed) {
        store.put(key, sc);
        out[g as usize] = Some(sc);
    }
    out
}

/// One report row per third-party group and per distribution imported but not loaded, with
/// its usage and (when given, per group) its scorecard.
pub fn rows<'a>(ix: &'a Index, roots: &'a Roots, usage: &Usage, scores: Option<&[Option<Scorecard>]>) -> Vec<DepRow<'a>> {
    let mut out = Vec::new();
    for (g, grp) in ix.groups().iter().enumerate() {
        if grp.origin != Origin::ThirdParty {
            continue;
        }
        let (name, version): (&[u8], &[u8]) = match grp.dist {
            Some(d) => (roots.dists.name(d), roots.dists.version(d)),
            None => (&grp.name, b""),
        };
        out.push(DepRow {
            name,
            key: &grp.name,
            version,
            loaded: true,
            usage: *usage.group(g as u32),
            score: scores.and_then(|s| s[g]),
        });
    }
    for &(d, u) in usage.unloaded() {
        if d.index() >= roots.dists.len() {
            continue;
        }
        out.push(DepRow {
            name: roots.dists.name(d),
            key: roots.dists.normalized_name(d),
            version: roots.dists.version(d),
            loaded: false,
            usage: u,
            score: None,
        });
    }
    out
}
