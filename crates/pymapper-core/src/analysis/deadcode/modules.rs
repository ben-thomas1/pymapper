//! Module levels (PM404): propagation over package/module nodes (see the module docs of
//! `deadcode`).

use super::prop::{Expand, Levels};
use super::{Why, level};
use crate::graph::{CallFilter, ModuleGraph, import_graph, module_call_graph};
use crate::index::{Index, NodeId, NodeKind};
use crate::reach::{RootSet, root_kind};
use crate::resolve::{Confidence, Imports, Reason, ResolvedCalls};

/// Module names (last segment) of files that tools run directly: build scripts, task runners,
/// web server entry modules, Sphinx configuration.
pub const SCRIPT_NAMES: &[&[u8]] =
    &[b"setup", b"manage", b"noxfile", b"fabfile", b"tasks", b"dodo", b"pavement", b"wsgi", b"asgi", b"conftest", b"conf"];

/// Levels of every package/module node.
#[derive(Clone, Debug, Default)]
pub(super) struct ModuleLevels {
    /// Package/module nodes, ascending (local ids).
    nodes: Vec<NodeId>,
    level: Vec<u8>,
    why: Vec<u8>,
}

impl ModuleLevels {
    #[inline]
    pub fn local(&self, v: NodeId) -> Option<usize> {
        self.nodes.binary_search(&v).ok()
    }

    pub fn level(&self, v: NodeId) -> Option<u8> {
        self.local(v).map(|i| self.level[i])
    }

    pub fn why(&self, v: NodeId) -> Option<Why> {
        self.local(v).map(|i| Why::from_u8(self.why[i]))
    }

    pub fn heap_bytes(&self) -> usize {
        self.nodes.capacity() * 4 + self.level.capacity() + self.why.capacity()
    }
}

/// The module graph: imports, lifted calls, parent packages.
pub(super) struct ModGraph<'a> {
    ix: &'a Index,
    imports: ModuleGraph,
    calls: ModuleGraph,
}

impl Expand for ModGraph<'_> {
    fn expand(&self, lv: &mut Levels, m: u32, l: u8) {
        for &w in self.imports.graph().succ(m) {
            lv.raise(w, l, Why::Import);
        }
        let fwd = self.calls.graph().forward();
        let labels = fwd.neighbour_labels(m);
        for (i, &w) in fwd.neighbours(m).iter().enumerate() {
            if labels[i] <= Confidence::Likely as u8 {
                lv.raise(w, l, Why::Call);
            } else {
                lv.raise(w, l.min(level::POSSIBLE), Why::PossibleCall);
            }
        }
        if let Some(p) = self.ix.parent(self.imports.node(m))
            && let Some(lp) = self.imports.local(p)
        {
            lv.raise(lp, l, Why::Submodule);
        }
    }
}

/// Module propagation state between the two phases.
pub(super) struct ModuleProp<'a> {
    pub g: ModGraph<'a>,
    pub lv: Levels,
}

impl<'a> ModuleProp<'a> {
    /// Builds the graphs and seeds the roots (see the module docs of `deadcode`), then runs
    /// the first phase. `included(f)`: the file's roots count.
    pub fn new(ix: &'a Index, imp: &Imports, calls: &ResolvedCalls, roots: &RootSet, included: &[bool]) -> ModuleProp<'a> {
        let imports = import_graph(ix, imp, 0);
        let mcalls = module_call_graph(ix, calls, CallFilter::default().without(Reason::Override));
        let g = ModGraph { ix, imports, calls: mcalls };
        let mut lv = Levels::new(g.imports.len());
        let module_of = |v: NodeId| -> Option<u32> {
            let m = match ix.kind(v) {
                NodeKind::Package | NodeKind::Module => v,
                NodeKind::Distribution => return None,
                _ => ix.file_node(ix.file(v)?),
            };
            g.imports.local(m)
        };
        for v in roots.iter() {
            if roots.kind(v) & !root_kind::MODULE != 0
                && let Some(m) = module_of(v)
            {
                lv.raise(m, level::LIVE, Why::Root);
            }
        }
        for (i, &v) in g.imports.nodes().iter().enumerate() {
            let Some(f) = ix.file(v) else { continue };
            if !included[f as usize] || ix.kind(v) != NodeKind::Module {
                continue;
            }
            if SCRIPT_NAMES.contains(&ix.name(v)) {
                lv.raise(i as u32, level::LIVE, Why::Root);
            } else if ix.parent(v).is_none_or(|p| ix.kind(p) == NodeKind::Distribution || ix.file(p).is_none()) {
                lv.raise(i as u32, level::SCRIPT_LIKE, Why::ScriptLike);
            }
        }
        lv.run(&g);
        ModuleProp { g, lv }
    }

    /// Level of module node `v` (`None` if `v` is not a package/module).
    pub fn level(&self, v: NodeId) -> Option<u8> {
        self.g.imports.local(v).map(|i| self.lv.level[i as usize])
    }

    /// Raises module node `v` to `l` (call [`ModuleProp::finish`] or `lv.run` afterwards).
    pub fn raise(&mut self, v: NodeId, l: u8, why: Why) {
        if let Some(i) = self.g.imports.local(v) {
            self.lv.raise(i, l, why);
        }
    }

    /// Runs the propagation to completion and returns the levels.
    pub fn finish(mut self) -> (ModuleLevels, u64) {
        self.lv.run(&self.g);
        let ModGraph { imports, .. } = self.g;
        let nodes = imports.nodes().to_vec();
        (ModuleLevels { nodes, level: self.lv.level, why: self.lv.why }, self.lv.expansions)
    }
}
