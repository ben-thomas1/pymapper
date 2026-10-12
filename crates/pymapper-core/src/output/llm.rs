//! The `llm` format (agent mode; design: ISSUES O2): a compact, prioritised report in a
//! Markdown-like layout, capped at [`Options::limit`] items with an "N more findings not
//! displayed" trailer, stable ids, evidence (confidence, reasons, metric values), a fix hint and
//! a verification command per item, and header facts (root, mode, counts, what the exit code
//! means, blind spots). Never coloured.
//!
//! PLACEHOLDER: until it is built, this writes the concise format.

use super::buf::Out;
use super::{Options, Report, Summary};

pub(super) fn write(r: &Report<'_>, opts: &Options, summary: &Summary, out: &mut Out<'_>) {
    super::concise::write(r, opts, summary, out);
}
