//! Call sites behind many (caller, callee) pairs, located in one pass per file.
//!
//! `Query::call_sites` scans a caller's file per pair; for a long result list in one big file
//! that is O(pairs x file edges). Here the pairs are sorted once and every file holding one is
//! scanned once, each edge looked up by binary search: O(K log K + E_f log K) for K pairs and
//! E_f edges in their files.

use pymapper_core::index::NodeId;
use pymapper_core::query::{Query, Site};
use pymapper_core::resolve::Reason;

/// One pair to locate. `dispatch`: the pair is linked only by an override edge (its single
/// "site" is the overriding def); otherwise only real call sites count.
#[derive(Clone, Copy)]
pub struct Pair {
    pub src: NodeId,
    pub dst: NodeId,
    pub dispatch: bool,
}

/// The first site (in file order) of a pair and how many sites it has.
#[derive(Clone, Copy, Default)]
pub struct Sites {
    pub first: Option<Site>,
    pub count: u32,
}

/// Locates every pair (see the module docs); the result is parallel to `pairs`.
pub fn locate(q: &Query<'_>, pairs: &[Pair]) -> Vec<Sites> {
    let ix = q.index();
    let mut out = vec![Sites::default(); pairs.len()];
    // (src, dst, row): one key per pair, sorted for binary search.
    let mut keys: Vec<(NodeId, NodeId, u32)> = pairs.iter().enumerate().map(|(i, p)| (p.src, p.dst, i as u32)).collect();
    keys.sort_unstable();
    // Real call edges live in the caller's file, override edges in the overriding class's.
    let mut files: Vec<u32> =
        pairs.iter().filter_map(|p| ix.file(if p.dispatch { p.dst } else { p.src })).collect();
    files.sort_unstable();
    files.dedup();
    let calls = q.calls();
    for &f in &files {
        for e in calls.file_edges(f) {
            let lo = keys.partition_point(|k| (k.0, k.1) < (e.src, e.dst));
            for k in keys[lo..].iter().take_while(|k| (k.0, k.1) == (e.src, e.dst)) {
                let i = k.2 as usize;
                if pairs[i].dispatch != (e.reason == Reason::Override) {
                    continue;
                }
                let s = &mut out[i];
                s.count += 1;
                if s.first.is_none() {
                    s.first = Some(q.edge_site(e));
                }
            }
        }
    }
    out
}

/// The first import row (in source order) of module node `src`'s file that adds the module
/// edge to `dst`: `(file, line, 1-based col)`. O(import rows of the file).
pub fn import_site(q: &Query<'_>, src: NodeId, dst: NodeId) -> Option<(u32, u32, u32)> {
    let ix = q.index();
    let f = ix.file(src)?;
    let st = &ix.facts().imports;
    let imp = q.imports();
    ix.file_facts(f).imports.range().find(|&row| imp.module_node(row) == dst).map(|row| (f, st.line[row], st.col[row] + 1))
}
