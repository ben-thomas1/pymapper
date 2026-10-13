//! `pymapper explain` subcommand (design: ISSUES O3): everything pymapper knows about one
//! finding (by id, `PM401-3fa2`) or every finding on one symbol, so an agent can verify and
//! act on it without rebuilding the context by hand. Stateless: re-runs the analysis (warm
//! cache) with the same configuration as `check` and looks the finding up by its stable id
//! (`pymapper_core::output::FindingIds::find`).
//!
//! ```text
//!   config ──> Setup (as check) ──> load ──> calls ──> Setup::analyse (findings below the
//!   gate and the dead-code result kept) ──> Report (all) + ids (gated report, as check)
//!   TARGET ──┬─ id ─────> FindingIds::find (gated ids, then all) ──> the finding (+ the other
//!            │            metric findings of its symbol)
//!            └─ symbol ─> Query::resolve (ambiguous / unknown: candidates, exit 2) ──> its
//!                         findings + connectivity (callers, callees, importers)
//!   ──> per family sections (family modules below) ──> human | llm ──> exit 0
//! ```
//!
//! The ids match `check`'s for the same options: they are computed over the findings at or
//! above the confidence gate (a baseline is not applied, so baselined findings stay
//! explainable); findings below the gate get the ids `check --min-confidence 0` shows. Source
//! lines are re-read from disk. Exit codes: 0 explained, 2 usage error, unknown or ambiguous
//! target. Dependency findings (PM6xx) come from `deps --check` and are not covered.

mod arch;
mod complexity;
mod dead;
mod doc;
mod fix;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use pymapper_core::analysis::deadcode::DeadCode;
use pymapper_core::analysis::dupes::DupesParams;
use pymapper_core::config::{Config, OutputFormat};
use pymapper_core::index::{Index, NodeId, NodeKind};
use pymapper_core::output::{self, FindError, FindingId, FindingIds, IndexContext, Report};
use pymapper_core::query::{LookupOptions, Query, Resolved};
use pymapper_core::resolve::{Confidence, Imports, resolve_calls};
use pymapper_core::rules::codes::{DUPLICATE_CODE, IMPORT_CYCLE};
use pymapper_core::rules::{Arg, Category, Finding, Findings, SymbolId, reason_names};

use self::doc::{Doc, Sources};
use super::check::{Input, Keep, Setup};
use super::config::ConfigArgs;
use super::query::{Mode, Writer, finish};
use crate::pipeline;

#[derive(clap::Args)]
pub struct Args {
    /// A finding id from `check` (PM401-3fa2), or a symbol: a qualified id
    /// (pkg.mod:Class.method), a dotted name or its last segments.
    target: String,
    /// Project to analyse (default: the working directory); config and project-root discovery
    /// start here.
    path: Option<PathBuf>,
    #[command(flatten)]
    config: ConfigArgs,
}

/// What every section renderer reads.
pub(crate) struct Cx<'a> {
    pub ix: &'a Index,
    pub imp: &'a Imports,
    pub q: &'a Query<'a>,
    pub config: &'a Config,
    pub root: &'a Path,
    pub findings: &'a Findings,
    pub ctx: &'a IndexContext<'a>,
    /// Every finding (gated or not) in output order, and its id.
    pub items: Vec<&'a Finding>,
    pub ids: Vec<FindingId>,
    pub min_confidence: u8,
    pub dead: Option<&'a DeadCode>,
    /// Dead-code mode per project unit (`check --stats` lines).
    pub modes: &'a [String],
}

impl Cx<'_> {
    /// The message of `f` without backticks.
    pub fn message(&self, f: &Finding) -> String {
        let mut s = String::new();
        let _ = self.findings.write_message(f, self.ctx, &mut s);
        output::strip_backticks(&mut s);
        s
    }

    /// Position (in `items`) of a finding on symbol `v` that `pred` accepts.
    pub fn finding_on(&self, v: NodeId, pred: impl Fn(&Finding) -> bool) -> Option<usize> {
        self.items.iter().position(|f| f.symbol == SymbolId(v) && pred(f))
    }

    /// The node of a finding's symbol (`None` without one).
    pub fn node(f: &Finding) -> Option<NodeId> {
        (f.symbol != SymbolId::NONE).then_some(f.symbol.0)
    }

    /// `path:line:col` of a finding, relative to the working directory.
    pub fn at(&self, doc: &Doc, f: &Finding) -> String {
        if f.loc.file.0 >= self.ix.file_count() {
            return String::from("(no file)");
        }
        let path = doc.rel(self.ix.file_path(f.loc.file.0));
        match (f.loc.line, f.loc.col) {
            (0, _) => path,
            (l, 0) => format!("{path}:{l}"),
            (l, c) => format!("{path}:{l}:{c}"),
        }
    }

    /// `path:line` of the definition of `v` relative to the working directory.
    pub fn def_at(&self, doc: &Doc, v: NodeId) -> String {
        let Some(f) = self.ix.file(v) else { return String::from("(no file)") };
        let path = doc.rel(self.ix.file_path(f));
        match self.ix.def_row(v).filter(|_| self.ix.kind(v).is_def()) {
            Some(r) => format!("{path}:{}", self.ix.facts().defs.name_line[r]),
            None => path,
        }
    }

    /// The project root as a shell argument.
    pub fn root_arg(&self, doc: &Doc) -> String {
        shell_word(&doc.rel(self.root))
    }
}

/// `s` as one shell word (single-quoted when it holds other than `[A-Za-z0-9._/-]`).
pub(crate) fn shell_word(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"._/-".contains(&c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub fn run(args: Args) -> anyhow::Result<ExitCode> {
    let cwd = std::env::current_dir().context("cannot determine the working directory")?;
    let loaded = args.config.load(args.path.into_iter().collect())?;
    let llm = match loaded.config.output_format {
        OutputFormat::Human | OutputFormat::Concise => false,
        OutputFormat::Llm => true,
        other => anyhow::bail!("`pymapper explain` writes the human and llm formats, not `{}`", other.as_str()),
    };
    let mut setup = Setup::new(loaded, &cwd, false, None, false)?;
    // The path only locates the project: explain looks at every finding.
    setup.scope.clear();
    pipeline::configure_threads(setup.config().threads)?;
    let mut project = pipeline::load(setup.config(), setup.root(), DupesParams::from_engine(&setup.engine));
    let fps = project.fingerprints.take();
    let calls = resolve_calls(&project.index, &project.imports);
    let input = Input {
        ix: &project.index,
        imp: &project.imports,
        calls: Some(&calls),
        units: &project.units,
        config_evidence: project.config_evidence.as_ref(),
    };
    let keep = Keep { below_confidence: true, dead_code: true };
    let analysed = setup.analyse(&input, fps, true, keep)?;
    let config = setup.config();
    let root = setup.root();
    let ix = &project.index;
    let ctx = IndexContext::new(ix, root, &config.contracts).relative_to(&cwd);
    let all = Report::new(&analysed.findings, &ctx);
    let min = config.min_confidence;
    let mut gated = Report::new(&analysed.findings, &ctx);
    gated.retain(|f| f.confidence >= min);
    let (all_ids, gated_ids) = (all.ids(), gated.ids());
    // Display ids: the gated report's for findings at or above the gate (as `check`), the full
    // report's below it. Both reports share one order, so the gated one is a subsequence.
    let items: Vec<&Finding> = all.iter().collect();
    let mut ids = Vec::with_capacity(items.len());
    let mut gated_pos = Vec::with_capacity(gated_ids.len());
    for (i, f) in items.iter().enumerate() {
        if f.confidence >= min {
            ids.push(gated_ids.get(gated_pos.len()));
            gated_pos.push(i);
        } else {
            ids.push(all_ids.get(i));
        }
    }
    let q = Query::new(ix, &project.imports, &calls);
    let cx = Cx {
        ix,
        imp: &project.imports,
        q: &q,
        config,
        root,
        findings: &analysed.findings,
        ctx: &ctx,
        items,
        ids,
        min_confidence: min,
        dead: analysed.dead_code.as_ref(),
        modes: &analysed.modes,
    };
    let target = args.target.trim();
    let selected = match lookup_id(target, &gated_ids, &all_ids, &gated_pos) {
        Some(Ok(pos)) => Selection::Finding(pos),
        Some(Err(e)) => {
            let hint = match e {
                FindError::NotFound if target.to_ascii_uppercase().starts_with("PM6") => {
                    " (dependency findings come from `pymapper deps --check`; explain does not cover them yet)"
                }
                FindError::NotFound => {
                    " (the code changed since, or other options hide it: run `pymapper check` with the same options for current ids)"
                }
                _ => "",
            };
            return fail(&format!("`{target}`: {e}{hint}"));
        }
        None => match q.resolve(target.as_bytes(), &LookupOptions::default()) {
            Resolved::Unique(v) => Selection::Symbol(v),
            Resolved::Ambiguous(m) => return candidates(ix, &cwd, &project.roots, target, true, &m),
            Resolved::NotFound(m) if !m.is_empty() => return candidates(ix, &cwd, &project.roots, target, false, &m),
            Resolved::NotFound(_) => return fail(&format!("`{target}` is neither a finding id (PM401-3fa2) nor a known symbol")),
        },
    };
    let mut doc = Doc::new(llm, &cwd);
    match selected {
        Selection::Finding(pos) => explain_finding(&cx, &mut doc, pos, target),
        Selection::Symbol(v) => explain_symbol(&cx, &mut doc, v, target),
    }
    let mut out = io::stdout().lock();
    match out.write_all(doc.out.as_bytes()).and_then(|()| out.flush()) {
        // A closed pipe (`pymapper explain x | head`) is not an error.
        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e).context("cannot write to stdout")?,
        _ => {}
    }
    Ok(ExitCode::SUCCESS)
}

enum Selection {
    Finding(usize),
    Symbol(NodeId),
}

/// Writes `pymapper: error: msg` and returns exit code 2.
fn fail(msg: &str) -> anyhow::Result<ExitCode> {
    // Ignore write failures: stderr may be closed and there is nowhere else to report.
    let _ = writeln!(io::stderr().lock(), "pymapper: error: {msg}");
    Ok(ExitCode::from(crate::EXIT_ERROR))
}

/// Lists the candidates of an ambiguous or unknown symbol (stdout, as the queries do) and
/// fails with exit 2.
fn candidates(
    ix: &Index,
    cwd: &Path,
    roots: &pymapper_core::roots::Roots,
    target: &str,
    ambiguous: bool,
    m: &[pymapper_core::query::Match],
) -> anyhow::Result<ExitCode> {
    let mut stdout = io::stdout().lock();
    let mut w = Writer::new(ix, cwd, roots, Mode::Text, &mut stdout);
    w.candidates("explain", target, if ambiguous { "ambiguous" } else { "not-found" }, m);
    finish(w)?;
    fail(&if ambiguous {
        let example = m.first().map_or_else(String::new, |m| ix.qid_string(m.node));
        format!("`{target}` is ambiguous: {} matches (listed on stdout); pass a qualified id such as `{example}`", m.len())
    } else {
        format!("no finding id or symbol matches `{target}`; similar names are listed on stdout")
    })
}

/// `target` as a finding id: `None` when it is not one (a symbol), else the position in the
/// full report: looked up among the gated ids (as `check` shows them) first, then among all.
fn lookup_id(target: &str, gated: &FindingIds, all: &FindingIds, gated_pos: &[usize]) -> Option<Result<usize, FindError>> {
    match gated.find(target) {
        Ok(j) => Some(Ok(gated_pos[j])),
        Err(FindError::Malformed) => None,
        Err(FindError::Ambiguous) => Some(Err(FindError::Ambiguous)),
        Err(FindError::NotFound) => Some(all.find(target)),
    }
}

/// The family a finding is explained by.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Complexity,
    Cycle,
    Contract,
    Duplication,
    DeadCode,
    UnusedImport,
    Other,
}

fn family(f: &Finding) -> Family {
    match f.rule.code() {
        100..=199 => Family::Complexity,
        _ if f.rule == IMPORT_CYCLE => Family::Cycle,
        302..=304 => Family::Contract,
        _ if f.rule == DUPLICATE_CODE => Family::Duplication,
        401..=404 => Family::DeadCode,
        405 => Family::UnusedImport,
        _ => Family::Other,
    }
}

/// `PM401-3fa2 · Dead code · unused-function: function legacy_discount is never used`
fn head_line(cx: &Cx<'_>, pos: usize) -> String {
    let f = cx.items[pos];
    let category = f.rule.category().map_or("Other", Category::title);
    format!("{} · {category} · {}: {}", cx.ids[pos], f.rule.def().name, cx.message(f))
}

/// The location and confidence line under a head.
fn head_details(cx: &Cx<'_>, doc: &mut Doc, pos: usize) {
    let f = cx.items[pos];
    let mut line = format!("at {}", cx.at(doc, f));
    if let Some(v) = Cx::node(f) {
        line.push_str(&format!(" · {} ({})", cx.ix.qid_string(v), cx.ix.kind(v).as_str()));
    }
    line.push_str(&format!(" · confidence {}", f.confidence));
    let reasons: Vec<&str> = reason_names(f).collect();
    if !reasons.is_empty() {
        line.push_str(&format!(" ({})", reasons.join(", ")));
    }
    if f.confidence < cx.min_confidence {
        line.push_str(&format!(" · below the gate {}: `check` hides it unless --min-confidence {}", cx.min_confidence, f.confidence));
    }
    doc.sub(&line);
}

/// Renders the finding at `pos` (a metric finding brings the other metrics of its symbol).
fn explain_finding(cx: &Cx<'_>, doc: &mut Doc, pos: usize, target: &str) {
    if doc.llm {
        doc.title(&format!("pymapper explain {target}"));
    }
    let f = cx.items[pos];
    let mut group = vec![pos];
    if family(f) == Family::Complexity && f.symbol != SymbolId::NONE {
        for (i, g) in cx.items.iter().enumerate() {
            if i != pos && family(g) == Family::Complexity && g.symbol == f.symbol {
                group.push(i);
            }
        }
    }
    render(cx, doc, &group);
    let next = next_step(cx, doc, pos);
    doc.next(&next);
}

/// Renders every finding on symbol `v` (a class: also on its members; a module: also the
/// cycles and duplicates it takes part in), after its connectivity.
fn explain_symbol(cx: &Cx<'_>, doc: &mut Doc, v: NodeId, target: &str) {
    let ix = cx.ix;
    if doc.llm {
        doc.title(&format!("pymapper explain {target}"));
    }
    let subtree = if ix.kind(v) == NodeKind::Class { ix.subtree(v) } else { v..v + 1 };
    let on = |f: &Finding| -> bool {
        if f.symbol != SymbolId::NONE && subtree.contains(&f.symbol.0) {
            return true;
        }
        (f.rule == IMPORT_CYCLE || f.rule == DUPLICATE_CODE) && cx.findings.extra(f).contains(&Arg::Symbol(SymbolId(v)))
    };
    let mut hits: Vec<usize> = (0..cx.items.len()).filter(|&i| on(cx.items[i])).collect();
    // A duplicate pair is reported from both copies: keep the one on the symbol itself.
    let copy_at = |f: &Finding| match *cx.findings.extra(f) {
        [Arg::File(file), Arg::U32(line), ..] => Some((file, line)),
        _ => None,
    };
    let mirrored: Vec<usize> = hits
        .iter()
        .copied()
        .filter(|&i| {
            let f = cx.items[i];
            f.rule == DUPLICATE_CODE
                && !subtree.contains(&f.symbol.0)
                && hits.iter().any(|&j| {
                    let g = cx.items[j];
                    g.rule == DUPLICATE_CODE && copy_at(f) == Some((g.loc.file, g.loc.line)) && copy_at(g) == Some((f.loc.file, f.loc.line))
                })
        })
        .collect();
    hits.retain(|i| !mirrored.contains(i));
    doc.head(&format!("{} ({}) at {}", ix.qid_string(v), ix.kind(v).as_str(), cx.def_at(doc, v)));
    let gated = hits.iter().filter(|&&i| cx.items[i].confidence >= cx.min_confidence).count();
    doc.sub(&format!(
        "{gated} finding{} at or above confidence {}, {} below",
        if gated == 1 { "" } else { "s" },
        cx.min_confidence,
        hits.len() - gated
    ));
    connectivity(cx, doc, v);
    // One block per finding; the metric findings of one function render together.
    let mut done = vec![false; hits.len()];
    for k in 0..hits.len() {
        if done[k] {
            continue;
        }
        done[k] = true;
        let f = cx.items[hits[k]];
        let mut group = vec![hits[k]];
        if family(f) == Family::Complexity {
            for j in k + 1..hits.len() {
                let g = cx.items[hits[j]];
                if !done[j] && family(g) == Family::Complexity && g.symbol == f.symbol {
                    group.push(hits[j]);
                    done[j] = true;
                }
            }
        }
        render(cx, doc, &group);
    }
    let id = qid_arg(ix, v);
    doc.next(&if hits.is_empty() {
        format!("no findings on it; `pymapper callers {id}` and `pymapper impact {id}` before changing it")
    } else {
        format!("`pymapper explain <id>` for one finding · `pymapper impact {id}` before changing it")
    });
}

/// Callers, callees and importers of `v`, counted (ISSUES O3: the connectivity summary).
fn connectivity(cx: &Cx<'_>, doc: &mut Doc, v: NodeId) {
    let ix = cx.ix;
    doc.section("Connectivity");
    let count = |it: &mut dyn Iterator<Item = (NodeId, Confidence, bool)>| -> (usize, usize, usize) {
        let (mut real, mut possible, mut dispatch) = (0, 0, 0);
        for (_, c, d) in it {
            if d {
                dispatch += 1;
            } else if c == Confidence::Possible {
                possible += 1;
            } else {
                real += 1;
            }
        }
        (real, possible, dispatch)
    };
    let callers = count(&mut cx.q.direct_callers(v));
    let callees = count(&mut cx.q.direct_callees(v));
    for (name, (real, possible, dispatch)) in [("callers", callers), ("callees", callees)] {
        doc.line(&format!("{name}: {real} certain/likely, {possible} possible, {dispatch} dispatch only"));
    }
    let module = ix.file(v).map_or(v, |f| ix.file_node(f));
    let importers = arch::importers_of(cx, module);
    let mut line = format!("modules importing {}: {}", ix.qid_string(module), importers.len());
    for (k, m) in importers.iter().take(5).enumerate() {
        line.push_str(if k == 0 { " (" } else { ", " });
        line.push_str(&ix.qid_string(m.module));
    }
    if !importers.is_empty() {
        line.push_str(if importers.len() > 5 { ", ...)" } else { ")" });
    }
    doc.line(&line);
}

/// Renders one group of findings (several only for the metrics of one function).
fn render(cx: &Cx<'_>, doc: &mut Doc, group: &[usize]) {
    let pos = group[0];
    let f = cx.items[pos];
    doc.head(&head_line(cx, pos));
    for &other in &group[1..] {
        let g = cx.items[other];
        doc.sub(&format!("also {} · {}: {}", cx.ids[other], g.rule.def().name, cx.message(g)));
    }
    head_details(cx, doc, pos);
    let mut sources = Sources::default();
    match family(f) {
        Family::Complexity => complexity::explain(cx, doc, &mut sources, group),
        Family::Cycle => arch::cycle(cx, doc, &mut sources, f),
        Family::Contract => arch::contract(cx, doc, &mut sources, f),
        Family::Duplication => arch::duplication(cx, doc, &mut sources, f),
        Family::DeadCode => dead::explain(cx, doc, &mut sources, pos),
        Family::UnusedImport => dead::unused_import(cx, doc, &mut sources, f),
        Family::Other => generic(cx, doc, &mut sources, f),
    }
    if let Some(hint) = fix::hint(cx, f) {
        doc.section("Fix");
        doc.line(&hint);
    }
    suppression(cx, doc, f);
}

/// The measure and source of a finding without a family-specific explanation (coupling).
fn generic(cx: &Cx<'_>, doc: &mut Doc, sources: &mut Sources, f: &Finding) {
    let args = cx.findings.args(f);
    if let (Some(Arg::U32(v)), Some(Arg::U32(t))) = (args.get(1), args.get(2)) {
        doc.section("Measure");
        doc.line(&format!("value {v}, threshold {t}"));
    }
    if f.loc.file.0 < cx.ix.file_count() && f.loc.line > 0 {
        let path = cx.ix.file_path(f.loc.file.0);
        doc.section(&format!("Source {}:{}", doc.rel(path), f.loc.line));
        if let Some(src) = sources.get(path) {
            doc.quote(src, f.loc.line, f.loc.line + 4, &[f.loc.line]);
        }
    }
}

/// How to silence the finding: an inline comment on the reported line.
fn suppression(cx: &Cx<'_>, doc: &mut Doc, f: &Finding) {
    doc.section("Suppress");
    let code = f.rule.code();
    if f.loc.file.0 < cx.ix.file_count() && f.loc.line > 0 {
        doc.line(&format!("add `# pymapper: ignore[PM{code:03}]` at the end of {}:{}", doc.rel(cx.ix.file_path(f.loc.file.0)), f.loc.line));
    } else {
        doc.line(&format!("`--ignore PM{code:03}` or `rules.ignore` in the config"));
    }
}

/// The next-step line after one finding.
fn next_step(cx: &Cx<'_>, doc: &Doc, pos: usize) -> String {
    let f = cx.items[pos];
    let sym = Cx::node(f).map(|v| qid_arg(cx.ix, v));
    match (family(f), sym) {
        (Family::DeadCode, Some(s)) => format!("run the Verify command, then delete it with what Effect lists · `pymapper callers {s}` to double-check"),
        (Family::Complexity, Some(s)) => format!("`pymapper impact {s}` before refactoring · re-run `pymapper explain {}` afterwards", cx.ids[pos]),
        (Family::Cycle | Family::Contract, _) => "`pymapper cycles --llm` for every cycle · `pymapper imports <module>` for one module's imports".to_owned(),
        _ => format!("`pymapper check --llm {}` to see what is left", cx.root_arg(doc)),
    }
}

/// A symbol as a shell argument (quoted when it holds `#`).
pub(crate) fn qid_arg(ix: &Index, v: NodeId) -> String {
    let id = ix.qid_string(v);
    if id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b)) { id } else { format!("'{id}'") }
}
