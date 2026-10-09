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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pymapper_core::index::{Inputs, load};
    use pymapper_core::resolve::{Attribution, resolve_calls, resolve_imports};
    use pymapper_core::roots::Origin;
    use pymapper_core::walk::{WalkOptions, walk};

    use super::*;

    /// A generated package: modules calling each other's functions at random (repeated calls
    /// give pairs with several sites), and a base class whose method dispatches to overrides
    /// in every module.
    fn generate(dir: &Path) {
        let (mods, funcs) = (6u64, 8u64);
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let pkg = dir.join("pkg");
        std::fs::create_dir_all(&pkg).expect("mkdir");
        std::fs::write(pkg.join("__init__.py"), "").expect("write");
        std::fs::write(pkg.join("base.py"), "class Base:\n    def run(self):\n        return self.step()\n\n    def step(self):\n        return 0\n")
            .expect("write");
        for m in 0..mods {
            let mut src = String::from("from pkg.base import Base\n");
            for o in (0..mods).filter(|&o| o != m) {
                src.push_str(&format!("from pkg import m{o}\n"));
            }
            src.push_str(&format!("\n\nclass K{m}(Base):\n    def step(self):\n        return f0(1)\n"));
            for f in 0..funcs {
                src.push_str(&format!("\n\ndef f{f}(x):\n"));
                for _ in 0..4 {
                    let (tm, tf) = (next(mods), next(funcs));
                    // Half the calls are repeated: pairs with several sites.
                    for _ in 0..=next(2) {
                        if tm == m {
                            src.push_str(&format!("    f{tf}(x)\n"));
                        } else {
                            src.push_str(&format!("    m{tm}.f{tf}(x)\n"));
                        }
                    }
                }
                src.push_str("    return x\n");
            }
            std::fs::write(pkg.join(format!("m{m}.py")), src).expect("write");
        }
    }

    /// Every (caller, callee) pair of a generated project, real and dispatch, located in one
    /// call against `Query::call_sites` per pair.
    #[test]
    fn locate_matches_call_sites() {
        let dir = std::env::temp_dir().join(format!("pymapper-sites-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        generate(&dir);
        let w = walk(&[dir.as_path()], &WalkOptions::everything());
        let ix = load(Inputs::from_root_dir(&dir, &w.files, Origin::FirstParty, b"gen"));
        let imp = resolve_imports(&ix, &Attribution::new());
        let calls = resolve_calls(&ix, &imp);
        let q = Query::new(&ix, &imp, &calls);
        let mut pairs: Vec<Pair> = Vec::new();
        for v in 0..ix.node_count() {
            for (w, _, dispatch) in q.direct_callees(v) {
                pairs.push(Pair { src: v, dst: w, dispatch });
            }
        }
        assert!(pairs.len() > 150, "too few pairs: {}", pairs.len());
        assert!(pairs.iter().any(|p| p.dispatch), "no dispatch pair");
        let got = locate(&q, &pairs);
        let multi = got.iter().filter(|s| s.count > 1).count();
        assert!(multi > 10, "too few pairs with several sites: {multi}");
        for (p, s) in pairs.iter().zip(&got) {
            let want: Vec<Site> =
                q.call_sites(p.src, p.dst).filter(|s| p.dispatch == (s.reason == Reason::Override)).collect();
            assert_eq!(s.count as usize, want.len(), "{}:{}", ix.qid_string(p.src), ix.qid_string(p.dst));
            assert_eq!(s.first, want.first().copied());
            assert!(s.count > 0, "every pair has a site");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
