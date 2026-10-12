//! The `human` format (default; design: ISSUES O1): findings grouped by category (or by file
//! with `group-by = "file"`), metric findings merged per symbol, cross-file findings shown
//! once, short symbols, a stable id on every item, a per-category footer tally, ANSI colour
//! when [`Options::color`] is set. Never capped.
//!
//! PLACEHOLDER: until it is built, this writes the concise format.

use super::buf::Out;
use super::{Options, Report, Summary};

pub(super) fn write(r: &Report<'_>, opts: &Options, summary: &Summary, out: &mut Out<'_>) {
    super::concise::write(r, opts, summary, out);
}
