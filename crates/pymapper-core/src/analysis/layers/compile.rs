//! Contract compilation: module names -> per-module roles over the dense module ids of the
//! module graphs, external (not loaded) forbidden names, ignore patterns.

use crate::config::{Contract, ContractKind};
use crate::graph::module_nodes;
use crate::index::{Index, NONE, NodeId, NodeKind};
use crate::intern::{FxHashMap, Sym};
use crate::resolve::{ModId, ModuleTable, res_flags};

/// No role in a contract.
pub(super) const NO_ROLE: u32 = u32::MAX;

/// The package/module nodes of the index (local ids `0..len`, as in
/// [`ModuleGraph`](crate::graph::ModuleGraph)) and the dotted name of each.
pub(super) struct Space {
    pub nodes: Vec<NodeId>,
    /// [`ModId`] of every local id (`NONE` if the node has no name in the table).
    pub mods: Vec<ModId>,
}

impl Space {
    pub fn build(ix: &Index, table: &ModuleTable) -> Space {
        let nodes = module_nodes(ix);
        let mut mods: Vec<ModId> = Vec::with_capacity(nodes.len());
        for i in 0..nodes.len() {
            let v = nodes[i];
            let m = match ix.file(v) {
                Some(f) => table.file_mod(f),
                // Namespace package: its parent's name plus its own segment. The parent is an
                // earlier node (pre-order), so its name is known.
                None => match ix.parent(v) {
                    Some(p) if ix.kind(p) != NodeKind::Distribution => {
                        let pm = nodes[..i].binary_search(&p).map_or(NONE, |k| mods[k]);
                        if pm == NONE { None } else { table.child(Some(pm), ix.name_sym(v)) }
                    }
                    _ => table.child(None, ix.name_sym(v)),
                },
            };
            mods.push(m.unwrap_or(NONE));
        }
        Space { nodes, mods }
    }

    /// Local id of package/module node `v` (binary search), `NONE` if `v` is not one.
    #[inline]
    pub fn local(&self, v: NodeId) -> u32 {
        if v == NONE {
            return NONE;
        }
        self.nodes.binary_search(&v).map_or(NONE, |i| i as u32)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
}

/// What a role slot stands for. Layers: container (`group`), layer (0 = highest) and the
/// sibling within the layer (`member`). Forbidden: `member` indexes `source` / `forbidden`.
/// Independence: `member` indexes `modules`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Slot {
    pub group: u32,
    pub layer: u32,
    pub member: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Layers { siblings: bool, groups: u32, layers: u32 },
    Forbidden,
    Independence,
}

/// A forbidden module that is not loaded: matched against the dotted names of unresolved
/// absolute imports.
#[derive(Clone, Debug)]
pub(super) struct ExtName {
    pub segs: Box<[Sym]>,
    /// Raw `Sym` shown in findings: the whole dotted name if interned, else its first segment.
    pub display: u32,
}

/// An `ignore-imports` entry, as dotted segments (`*` one segment, `**` any number).
#[derive(Clone, Debug)]
pub(super) struct Pattern {
    pub importer: Vec<Box<[u8]>>,
    pub imported: Vec<Box<[u8]>>,
}

/// A contract over the module space.
pub(super) struct Compiled {
    /// Index into the contract list (the finding's `Contract` argument).
    pub index: u32,
    pub kind: Kind,
    /// Rows with any of these [`res_flags`] are exempt.
    pub exclude: u8,
    pub transitive: bool,
    pub slots: Vec<Slot>,
    /// Role (slot) of every local id; for `Forbidden` the `source` role.
    pub src: Vec<u32>,
    /// `Forbidden` only: the `forbidden` role of every local id (empty otherwise).
    pub dst: Vec<u32>,
    /// `Forbidden` only: forbidden names not loaded.
    pub ext: Vec<ExtName>,
    pub ignore: Vec<Pattern>,
}

impl Compiled {
    /// Whether some local module has a source role (else the contract can never fire).
    pub fn any_source(&self) -> bool {
        self.src.iter().any(|&r| r != NO_ROLE)
    }
}

/// Module names of a contract that match nothing loaded (and, for `forbidden` targets, can
/// not match an unresolved import either).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingModule {
    /// Index into the contract list.
    pub contract: u32,
    /// The dotted name (`container.layer` for layers with containers).
    pub name: String,
}

/// Builds the role array for `(name, slot)` pairs: a module gets the slot of its nearest
/// listed ancestor-or-self name. O(modules x depth) hash lookups.
fn roles(space: &Space, table: &ModuleTable, listed: &FxHashMap<ModId, u32>) -> Vec<u32> {
    let mut out = vec![NO_ROLE; space.len()];
    if listed.is_empty() {
        return out;
    }
    for (i, &m) in space.mods.iter().enumerate() {
        let mut cur = (m != NONE).then_some(m);
        while let Some(c) = cur {
            if let Some(&s) = listed.get(&c) {
                out[i] = s;
                break;
            }
            cur = table.parent(c);
        }
    }
    out
}

fn split(p: &str) -> Vec<Box<[u8]>> {
    p.split('.').map(|s| Box::from(s.as_bytes())).collect()
}

/// Compiles contract `index`. Names that resolve to nothing are appended to `missing`.
pub(super) fn compile(
    ix: &Index,
    table: &ModuleTable,
    space: &Space,
    index: u32,
    c: &Contract,
    missing: &mut Vec<MissingModule>,
) -> Compiled {
    let mut slots: Vec<Slot> = Vec::new();
    let mut listed: FxHashMap<ModId, u32> = FxHashMap::default();
    let mut dst_listed: FxHashMap<ModId, u32> = FxHashMap::default();
    let mut ext: Vec<ExtName> = Vec::new();
    let mut name = String::new();
    let add = |name: &str, slot: Slot, map: &mut FxHashMap<ModId, u32>, slots: &mut Vec<Slot>| -> bool {
        match table.lookup(ix, name.as_bytes()) {
            Some(m) => {
                map.insert(m, slots.len() as u32);
                slots.push(slot);
                true
            }
            None => false,
        }
    };
    let kind = match &c.kind {
        ContractKind::Layers { layers, containers, independent_siblings } => {
            let groups = containers.len().max(1);
            for g in 0..groups {
                for (l, siblings) in layers.iter().enumerate() {
                    for (s, layer) in siblings.iter().enumerate() {
                        name.clear();
                        if let Some(container) = containers.get(g) {
                            name.push_str(container);
                            name.push('.');
                        }
                        name.push_str(layer);
                        let slot = Slot { group: g as u32, layer: l as u32, member: s as u32 };
                        if !add(&name, slot, &mut listed, &mut slots) {
                            missing.push(MissingModule { contract: index, name: name.clone() });
                        }
                    }
                }
            }
            Kind::Layers { siblings: *independent_siblings, groups: groups as u32, layers: layers.len() as u32 }
        }
        ContractKind::Forbidden { source, forbidden } => {
            for (i, m) in source.iter().enumerate() {
                if !add(m, Slot { group: 0, layer: 0, member: i as u32 }, &mut listed, &mut slots) {
                    missing.push(MissingModule { contract: index, name: m.clone() });
                }
            }
            for (i, m) in forbidden.iter().enumerate() {
                if add(m, Slot { group: 0, layer: 1, member: i as u32 }, &mut dst_listed, &mut slots) {
                    continue;
                }
                // Not loaded: matched against unresolved imports. A segment that was never
                // interned appears in no import, so such a name can not match anything.
                let segs: Option<Box<[Sym]>> = m.split('.').map(|s| ix.strings().get(s.as_bytes())).collect();
                match segs {
                    Some(segs) => {
                        let display = ix.strings().get(m.as_bytes()).unwrap_or(segs[0]).to_raw();
                        ext.push(ExtName { segs, display });
                    }
                    None => missing.push(MissingModule { contract: index, name: m.clone() }),
                }
            }
            Kind::Forbidden
        }
        ContractKind::Independence { modules } => {
            for (i, m) in modules.iter().enumerate() {
                if !add(m, Slot { group: 0, layer: 0, member: i as u32 }, &mut listed, &mut slots) {
                    missing.push(MissingModule { contract: index, name: m.clone() });
                }
            }
            Kind::Independence
        }
    };
    let src = roles(space, table, &listed);
    let dst = if kind == Kind::Forbidden { roles(space, table, &dst_listed) } else { Vec::new() };
    let ignore = c
        .ignore_imports
        .iter()
        .map(|p| Pattern { importer: split(&p.importer), imported: split(&p.imported) })
        .collect();
    Compiled {
        index,
        kind,
        exclude: if c.allow_type_checking { res_flags::TYPE_ONLY } else { 0 },
        transitive: c.transitive,
        slots,
        src,
        dst,
        ext,
        ignore,
    }
}

/// Whether dotted `name` (as segments) matches `pat`: `*` matches exactly one segment, `**`
/// zero or more, anything else one equal segment. Two-pointer backtracking on the last `**`:
/// O(pattern x name) worst case, no allocation.
pub(super) fn pattern_matches(pat: &[Box<[u8]>], name: &[&[u8]]) -> bool {
    let (mut p, mut n) = (0usize, 0usize);
    // Position after the last `**` and the name position it is currently matched up to.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pat.len() && &*pat[p] == b"**" {
            star = Some((p + 1, n));
            p += 1;
        } else if p < pat.len() && (&*pat[p] == b"*" || *pat[p] == *name[n]) {
            p += 1;
            n += 1;
        } else if let Some((sp, sn)) = star {
            // Let the last `**` absorb one more segment.
            p = sp;
            n = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    while p < pat.len() && &*pat[p] == b"**" {
        p += 1;
    }
    p == pat.len()
}
