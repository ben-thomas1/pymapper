//! Items of the llm format: findings merged into the decisions an agent acts on, in priority
//! order (see the parent module for the rules).
//!
//! ```text
//!   findings (output order) ──> section per finding (O1 order)
//!     ──> dead-chain roots: referrer extra arg ──> nearest reported ancestor ──> root
//!     ──> PM501 pairs: the second copy of each pair dropped
//!     ──> group key per finding ──> sort (key, root first, position) ──> one item per key
//!     ──> items sorted by section, class, severity, confidence, overshoot, size, position
//! ```
//!
//! Work is O(n log n) in the findings, with a handful of allocations per report (none per
//! finding).

use std::cmp::Reverse;

use super::super::Report;
use crate::intern::{FxHashMap, FxHashSet};
use crate::rules::codes::{DUPLICATE_CODE, TRANSITIVE_DEPENDENCY, UNDECLARED_DEPENDENCY, UNUSED_IMPORT, UNUSED_MODULE};
use crate::rules::{Arg, Finding, Severity, SymbolId, ThresholdKey};

/// Sections, in output order (ISSUES O1 rule 1).
pub(super) const SECTIONS: [&str; 7] =
    ["Architecture", "Dependencies", "Complexity", "Coupling", "Duplication", "Dead code", "Unused imports"];
pub(super) const ARCH: u8 = 0;
pub(super) const DEPS: u8 = 1;
pub(super) const COMPLEXITY: u8 = 2;
pub(super) const COUPLING: u8 = 3;
pub(super) const DUPLICATION: u8 = 4;
pub(super) const DEAD: u8 = 5;
pub(super) const IMPORTS: u8 = 6;

/// Section of a finding.
pub(super) fn section(f: &Finding) -> u8 {
    match f.rule.code() / 100 {
        1 => COMPLEXITY,
        2 => COUPLING,
        3 => ARCH,
        4 if f.rule == UNUSED_IMPORT => IMPORTS,
        4 => DEAD,
        5 => DUPLICATION,
        _ => DEPS,
    }
}

/// How an item's findings are related.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// One finding.
    Single,
    /// Metric findings of one symbol (PM1xx, PM2xx).
    Metrics,
    /// A dead definition (or module) and what is only used by it: the root first.
    Chain,
    /// Dead definitions of one file with the same verdict and confidence.
    DeadGroup,
    /// Unused imports of one file with the same confidence.
    Imports,
    /// One dependency finding reported at several import sites (PM601, PM604).
    Sites,
}

/// One item: members `start..start + len` of [`Groups::members`] (output positions), the
/// representative first.
#[derive(Clone, Copy, Debug)]
pub(super) struct Item {
    pub section: u8,
    pub kind: Kind,
    pub start: u32,
    pub len: u32,
    /// Dead code: 0 for chains and modules, 1 for single symbols (shown later).
    class: u8,
    sev: Severity,
    conf: u8,
    /// Worst metric overshoot (value / limit), for complexity and coupling.
    weight: f32,
}

/// The items of a report, in display order.
#[derive(Debug, Default)]
pub(super) struct Groups {
    pub items: Vec<Item>,
    pub members: Vec<u32>,
}

impl Groups {
    /// Member positions of `item`.
    #[inline]
    pub fn of(&self, item: &Item) -> &[u32] {
        &self.members[item.start as usize..(item.start + item.len) as usize]
    }
}

const NONE: u32 = u32::MAX;
const ON_STACK: u32 = u32::MAX - 1;
const UNRESOLVED: u32 = u32::MAX - 2;

/// Group key kinds (second key slot).
const K_UNIQUE: u32 = 0;
const K_SYMBOL: u32 = 1;
const K_CHAIN: u32 = 2;
const K_DEAD: u32 = 3;
const K_IMPORTS: u32 = 4;
const K_SITES: u32 = 5;

/// Builds the items of `fs` (the report's findings in output order).
pub(super) fn build(r: &Report<'_>, fs: &[&Finding]) -> Groups {
    let n = fs.len();
    let roots = chain_roots(r, fs);
    let dropped = second_copies(r, fs);
    // Dead-chain roots that have dependents.
    let mut has_deps = vec![false; n];
    for (pos, &root) in roots.iter().enumerate() {
        if root != NONE && root as usize != pos {
            has_deps[root as usize] = true;
        }
    }
    // (key, order within the group, position); order 0 puts a chain root first.
    let mut keys: Vec<([u32; 5], u32, u32)> = Vec::with_capacity(n);
    for (pos, f) in fs.iter().enumerate() {
        if dropped.contains(&(pos as u32)) {
            continue;
        }
        let sec = section(f);
        let p = pos as u32;
        let file = f.loc.file.0;
        let key = match sec {
            COMPLEXITY | COUPLING if f.symbol != SymbolId::NONE => [u32::from(sec), K_SYMBOL, f.symbol.0, 0, 0],
            DEAD if roots[pos] != NONE && (roots[pos] != p || has_deps[pos]) => {
                [u32::from(sec), K_CHAIN, roots[pos], 0, 0]
            }
            DEAD if f.rule != UNUSED_MODULE => {
                let phrase = match r.findings().args(f).get(1) {
                    Some(&Arg::Phrase(i)) => i,
                    _ => NONE,
                };
                [u32::from(sec), K_DEAD, file, (phrase << 8) | u32::from(f.confidence), f.reasons]
            }
            IMPORTS => [u32::from(sec), K_IMPORTS, file, u32::from(f.confidence), f.reasons],
            DEPS if f.rule == UNDECLARED_DEPENDENCY || f.rule == TRANSITIVE_DEPENDENCY => {
                let dist = match r.findings().args(f).get(1) {
                    Some(&Arg::Name(d)) => d,
                    _ => p,
                };
                [u32::from(sec), K_SITES, u32::from(f.rule.code()), dist, u32::from(f.confidence)]
            }
            _ => [u32::from(sec), K_UNIQUE, p, 0, 0],
        };
        let order = if key[1] == K_CHAIN && roots[pos] == p { 0 } else { p + 1 };
        keys.push((key, order, p));
    }
    keys.sort_unstable();

    let mut g = Groups { items: Vec::with_capacity(keys.len()), members: Vec::with_capacity(keys.len()) };
    let mut i = 0;
    while i < keys.len() {
        let key = keys[i].0;
        let start = g.members.len() as u32;
        let mut sev = Severity::Info;
        let mut conf = 0u8;
        let mut weight = 0f32;
        while i < keys.len() && keys[i].0 == key {
            let pos = keys[i].2;
            let f = fs[pos as usize];
            sev = sev.max(f.rule.def().severity);
            conf = conf.max(f.confidence);
            weight = weight.max(overshoot(r, f));
            g.members.push(pos);
            i += 1;
        }
        let len = g.members.len() as u32 - start;
        let kind = match key[1] {
            K_SYMBOL if len > 1 => Kind::Metrics,
            K_CHAIN if len > 1 => Kind::Chain,
            K_DEAD if len > 1 => Kind::DeadGroup,
            K_IMPORTS if len > 1 => Kind::Imports,
            K_SITES if len > 1 => Kind::Sites,
            _ => Kind::Single,
        };
        let section = key[0] as u8;
        let class = u8::from(section == DEAD && kind != Kind::Chain && fs[g.members[start as usize] as usize].rule != UNUSED_MODULE);
        g.items.push(Item { section, kind, start, len, class, sev, conf, weight });
    }
    let members = &g.members;
    g.items.sort_by(|a, b| {
        let first = |it: &Item| members[it.start as usize];
        (a.section, a.class, Reverse(a.sev), Reverse(a.conf))
            .cmp(&(b.section, b.class, Reverse(b.sev), Reverse(b.conf)))
            .then(b.weight.total_cmp(&a.weight))
            .then(if a.section == DEAD { b.len.cmp(&a.len) } else { std::cmp::Ordering::Equal })
            .then(first(a).cmp(&first(b)))
    });
    g
}

/// How far a metric finding is past its limit (value / limit, or limit / value for lower
/// bounds); 0 for other findings.
pub(super) fn overshoot(r: &Report<'_>, f: &Finding) -> f32 {
    let code = f.rule.code();
    if !(100..300).contains(&code) {
        return 0.0;
    }
    let args = r.findings().args(f);
    let num = |a: Option<&Arg>| match a {
        Some(&Arg::U32(v)) => v as f32,
        Some(&Arg::F32(v)) => v,
        _ => 0.0,
    };
    let (value, limit) = (num(args.get(1)), num(args.get(2)));
    let lower = f.rule.def().thresholds.first().is_some_and(|k| *k == ThresholdKey::MinMaintainability);
    let (a, b) = if lower { (limit, value) } else { (value, limit) };
    if b > 0.0 { a / b } else { a }
}

/// Per position: the position of the root of its dead chain (itself when nothing reported
/// refers to it), or `NONE` for findings that are not PM401-PM404. A finding's referrer is the
/// symbol in its extra arguments (set by the dead-code analysis for "only used by unused code"),
/// attributed to its nearest reported ancestor (a method of a reported class, a def of a
/// reported module); referrer cycles take their first member as the root.
fn chain_roots(r: &Report<'_>, fs: &[&Finding]) -> Vec<u32> {
    let n = fs.len();
    let dead = |f: &Finding| section(f) == DEAD;
    let count = fs.iter().filter(|f| dead(f)).count();
    let mut by_symbol: FxHashMap<u32, u32> = FxHashMap::with_capacity_and_hasher(count, Default::default());
    for (pos, f) in fs.iter().enumerate() {
        if dead(f) && f.symbol != SymbolId::NONE {
            by_symbol.entry(f.symbol.0).or_insert(pos as u32);
        }
    }
    let mut up = vec![NONE; n];
    let mut root = vec![NONE; n];
    for (pos, f) in fs.iter().enumerate() {
        if !dead(f) {
            continue;
        }
        root[pos] = UNRESOLVED;
        if let [Arg::Symbol(w)] = *r.findings().extra(f) {
            let mut s = w;
            // Bounded: hierarchies are shallow; a malformed context cannot loop.
            for _ in 0..64 {
                if s == SymbolId::NONE {
                    break;
                }
                if let Some(&p) = by_symbol.get(&s.0) {
                    if p as usize != pos {
                        up[pos] = p;
                    }
                    break;
                }
                s = r.ctx().parent_symbol(s);
            }
        }
    }
    let mut stack: Vec<u32> = Vec::new();
    for pos in 0..n {
        if root[pos] != UNRESOLVED {
            continue;
        }
        let mut cur = pos as u32;
        let found = loop {
            match root[cur as usize] {
                ON_STACK => {
                    // A referrer cycle: its smallest position is the root.
                    let at = stack.iter().position(|&s| s == cur).unwrap_or(0);
                    break stack[at..].iter().copied().min().unwrap_or(cur);
                }
                v if v != UNRESOLVED => break v,
                _ => {}
            }
            root[cur as usize] = ON_STACK;
            stack.push(cur);
            let next = up[cur as usize];
            if next == NONE {
                break cur;
            }
            cur = next;
        };
        for &s in &stack {
            root[s as usize] = found;
        }
        stack.clear();
    }
    root
}

/// Positions of the second copy of each PM501 pair (the analysis reports both directions).
fn second_copies(r: &Report<'_>, fs: &[&Finding]) -> FxHashSet<u32> {
    let mut seen: FxHashSet<((u32, u32), (u32, u32))> = FxHashSet::default();
    let mut dropped = FxHashSet::default();
    for (pos, f) in fs.iter().enumerate() {
        if f.rule != DUPLICATE_CODE {
            continue;
        }
        if let [Arg::File(file), Arg::U32(line), ..] = *r.findings().extra(f) {
            let (a, b) = ((f.loc.file.0, f.loc.line), (file.0, line));
            if !seen.insert((a.min(b), a.max(b))) {
                dropped.insert(pos as u32);
            }
        }
    }
    dropped
}
