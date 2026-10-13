//! Fix hint of a finding for `explain` (ISSUES O2/O3): the per-rule template next to the rule
//! metadata (`RuleDef::fix`, shared with `check --llm` and the JSON `fix` field) filled with
//! the finding's arguments; never invented advice.

use pymapper_core::output;
use pymapper_core::rules::Finding;

use super::Cx;

/// The one-line fix hint of `f`, rendered from its rule's template and the finding's facts.
pub fn hint(cx: &Cx<'_>, f: &Finding) -> Option<String> {
    let mut s = String::new();
    cx.findings.write_fix(f, cx.ctx, &mut s).ok()?;
    output::strip_backticks(&mut s);
    (!s.is_empty()).then_some(s)
}
