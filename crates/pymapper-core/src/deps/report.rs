//! The `pymapper deps` report: one row per third-party distribution, concise or JSON.
//!
//! # Concise
//!
//! A table, rows sorted by normalized name then version, then a footer:
//!
//! ```text
//! distribution  version  grade  score  loc     hot/kloc  mi    cc   deps  importers  api used  loc reached
//! requests      2.32.3   B      78.2   5301    1.2       62.1  3.1  4     3          14/152    1203/5301 (23%)
//!
//! 1 of 31 distributions used by first-party code (128 first-party files); --all lists every one.
//! Scorecards: 3 computed, 28 cached.
//! ```
//!
//! Without `--score` only the usage columns (`importers`, `api used`, `loc reached`) follow
//! the version, and `api used` has no denominator. A distribution imported but without loaded
//! code shows `-` for what needs its code.
//!
//! # JSON (`version` 2, the conventions of `check`'s JSON)
//!
//! ```text
//! {"version":2,"tool":{"name":"pymapper","version":"0.3.0"},"distributions":[
//!   {"name":"requests","version":"2.32.3","loaded":true,"used":true,"importers":3,
//!    "import_rows":5,"modules_used":2,"api_used":14,"call_sites":20,"loc_reached":1203,
//!    "loc":5301,"scorecard":{"grade":"B","score":78.25,"files":17,...}},
//!   ...
//! ],"summary":{"distributions":31,"used":12,"listed":12,"first_party_files":128,
//!   "scorecards_computed":3,"scorecards_cached":28}}
//! ```
//!
//! One distribution per line. `scorecard` is present with `--score` (`null` for a
//! distribution without loaded code); its `grade` and `score` are `null` without functions.
//! Reals have at most two decimals. The `scorecards_*` counters are present with `--score`.

use std::fmt::Write as _;
use std::io;

use crate::output::json::push_str;
use crate::output::{JSON_VERSION, Out, Outcome};

use super::score::Scorecard;
use super::store::ScoreStats;
use super::usage::DistUsage;

/// What the report shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReportOptions {
    /// Scorecard columns / objects.
    pub score: bool,
    /// Every distribution, not only those first-party code uses.
    pub all: bool,
    /// JSON instead of the concise table.
    pub json: bool,
}

/// One distribution.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DepRow<'a> {
    pub name: &'a [u8],
    /// Normalized name (sort key).
    pub key: &'a [u8],
    pub version: &'a [u8],
    /// Its code is loaded (it has a group).
    pub loaded: bool,
    pub usage: DistUsage,
    /// With `--score`, for loaded distributions.
    pub score: Option<Scorecard>,
}

/// The rows and totals of a run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DepsReport<'a> {
    rows: Vec<DepRow<'a>>,
    pub first_party_files: u32,
    /// Scorecard counters (with `--score`).
    pub scores: Option<ScoreStats>,
}

/// `num / den` as a whole percentage.
fn percent(num: u32, den: u32) -> u32 {
    if den == 0 { 0 } else { ((u64::from(num) * 100 + u64::from(den) / 2) / u64::from(den)) as u32 }
}

/// A real with at most two decimals (JSON), `null` when not finite.
fn push_real(out: &mut Vec<u8>, v: f32) {
    if v.is_finite() {
        let _ = io::Write::write_fmt(out, format_args!("{}", (f64::from(v) * 100.0).round() / 100.0));
    } else {
        out.extend_from_slice(b"null");
    }
}

fn key(out: &mut Vec<u8>, k: &str) {
    out.push(b',');
    out.push(b'"');
    out.extend_from_slice(k.as_bytes());
    out.extend_from_slice(b"\":");
}

fn key_u32(out: &mut Vec<u8>, k: &str, v: u32) {
    key(out, k);
    let _ = io::Write::write_fmt(out, format_args!("{v}"));
}

fn key_real(out: &mut Vec<u8>, k: &str, v: f32) {
    key(out, k);
    push_real(out, v);
}

impl<'a> DepsReport<'a> {
    /// A report over `rows` (any order; sorted here by normalized name, then version).
    pub fn new(mut rows: Vec<DepRow<'a>>, first_party_files: u32, scores: Option<ScoreStats>) -> DepsReport<'a> {
        rows.sort_unstable_by(|a, b| a.key.cmp(b.key).then_with(|| a.version.cmp(b.version)).then(a.name.cmp(b.name)));
        DepsReport { rows, first_party_files, scores }
    }

    /// Every row, sorted.
    pub fn rows(&self) -> &[DepRow<'a>] {
        &self.rows
    }

    /// Rows that first-party code uses.
    pub fn used(&self) -> usize {
        self.rows.iter().filter(|r| r.usage.is_used()).count()
    }

    /// Writes the report; a closed pipe is [`Outcome::Closed`].
    pub fn write(&self, opts: &ReportOptions, w: &mut dyn io::Write) -> io::Result<Outcome> {
        let mut out = Out::new(w);
        if opts.json {
            self.write_json(opts, &mut out);
        } else {
            self.write_concise(opts, &mut out);
        }
        out.finish()
    }

    fn listed(&self, opts: &ReportOptions) -> impl Iterator<Item = &DepRow<'a>> + '_ {
        let all = opts.all;
        self.rows.iter().filter(move |r| all || r.usage.is_used())
    }
}

/// Table cells rendered into one byte pool (`ends[i]` is the end of cell `i`, row-major).
struct Table {
    cols: usize,
    pool: String,
    ends: Vec<usize>,
}

impl Table {
    fn new(cols: usize) -> Table {
        Table { cols, pool: String::new(), ends: Vec::new() }
    }

    /// Ends the cell being written.
    fn cell(&mut self) {
        self.ends.push(self.pool.len());
    }

    fn text(&mut self, s: &[u8]) {
        self.pool.push_str(&String::from_utf8_lossy(s));
        self.cell();
    }

    fn get(&self, i: usize) -> &str {
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        &self.pool[start..self.ends[i]]
    }

    /// Writes the rows, columns padded to their widest cell (`right[c]`: right-aligned), the
    /// last column unpadded.
    fn write(&self, right: &[bool], out: &mut Out<'_>) {
        let rows = self.ends.len() / self.cols;
        let mut width = vec![0usize; self.cols];
        for i in 0..self.ends.len() {
            let c = i % self.cols;
            width[c] = width[c].max(self.get(i).chars().count());
        }
        for r in 0..rows {
            for c in 0..self.cols {
                let s = self.get(r * self.cols + c);
                let pad = width[c] - s.chars().count();
                if c > 0 {
                    out.str("  ");
                }
                if c + 1 == self.cols {
                    out.str(s);
                } else if right[c] {
                    for _ in 0..pad {
                        out.byte(b' ');
                    }
                    out.str(s);
                } else {
                    out.str(s);
                    for _ in 0..pad {
                        out.byte(b' ');
                    }
                }
            }
            out.byte(b'\n');
        }
    }

    /// Ends a cell holding `args`.
    fn fmt(&mut self, args: std::fmt::Arguments<'_>) {
        let _ = self.pool.write_fmt(args);
        self.cell();
    }
}

const SCORE_HEADER: [&str; 12] =
    ["distribution", "version", "grade", "score", "loc", "hot/kloc", "mi", "cc", "deps", "importers", "api used", "loc reached"];
const SCORE_RIGHT: [bool; 12] = [false, false, false, true, true, true, true, true, true, true, true, false];
const USAGE_HEADER: [&str; 5] = ["distribution", "version", "importers", "api used", "loc reached"];
const USAGE_RIGHT: [bool; 5] = [false, false, true, true, false];

impl DepsReport<'_> {
    fn write_concise(&self, opts: &ReportOptions, out: &mut Out<'_>) {
        let total = self.rows.len();
        let used = self.used();
        let (header, right): (&[&str], &[bool]) =
            if opts.score { (&SCORE_HEADER, &SCORE_RIGHT) } else { (&USAGE_HEADER, &USAGE_RIGHT) };
        let mut t = Table::new(header.len());
        for h in header {
            t.text(h.as_bytes());
        }
        let mut listed = 0usize;
        for r in self.listed(opts) {
            listed += 1;
            t.text(r.name);
            t.text(if r.version.is_empty() { b"-" } else { r.version });
            if opts.score {
                score_cells(&mut t, r.score.as_ref());
            }
            let u = &r.usage;
            t.fmt(format_args!("{}", u.importers));
            match (r.score.filter(|_| opts.score), r.loaded) {
                (Some(sc), true) => t.fmt(format_args!("{}/{}", u.api_used, sc.public_api)),
                (None, true) => t.fmt(format_args!("{}", u.api_used)),
                (_, false) => t.text(b"-"),
            }
            if r.loaded {
                t.fmt(format_args!("{}/{} ({}%)", u.loc_reached, u.loc, percent(u.loc_reached, u.loc)));
            } else {
                t.text(b"- (code not loaded)");
            }
        }
        if listed > 0 {
            t.write(right, out);
            out.byte(b'\n');
        }
        let s = |n: usize| if n == 1 { "" } else { "s" };
        let fp = self.first_party_files as usize;
        let _ = write!(out, "{used} of {total} distribution{} used by first-party code ({fp} first-party file{})", s(total), s(fp));
        if !opts.all && listed < total {
            out.str("; --all lists every one");
        }
        out.str(".\n");
        if let Some(s) = &self.scores {
            let _ = writeln!(out, "Scorecards: {} computed, {} cached.", s.computed, s.cached);
        }
    }

    fn write_json(&self, opts: &ReportOptions, out: &mut Out<'_>) {
        let _ = write!(
            out,
            "{{\"version\":{JSON_VERSION},\"tool\":{{\"name\":\"pymapper\",\"version\":\"{}\"}},\"distributions\":[",
            env!("CARGO_PKG_VERSION")
        );
        let mut listed = 0u32;
        for r in self.listed(opts) {
            let b = out.buf();
            b.extend_from_slice(if listed == 0 { b"\n" } else { b",\n" });
            listed += 1;
            b.extend_from_slice(b"{\"name\":");
            push_str(b, r.name);
            key(b, "version");
            push_str(b, r.version);
            key(b, "loaded");
            b.extend_from_slice(if r.loaded { b"true" } else { b"false" });
            key(b, "used");
            b.extend_from_slice(if r.usage.is_used() { b"true" } else { b"false" });
            let u = &r.usage;
            key_u32(b, "importers", u.importers);
            key_u32(b, "import_rows", u.import_rows);
            key_u32(b, "modules_used", u.modules_used);
            key_u32(b, "api_used", u.api_used);
            key_u32(b, "call_sites", u.call_sites);
            key_u32(b, "loc_reached", u.loc_reached);
            key_u32(b, "loc", u.loc);
            if opts.score {
                key(b, "scorecard");
                match &r.score {
                    Some(sc) => push_scorecard(b, sc),
                    None => b.extend_from_slice(b"null"),
                }
            }
            b.push(b'}');
            out.maybe_flush();
        }
        let b = out.buf();
        if listed > 0 {
            b.push(b'\n');
        }
        b.extend_from_slice(b"],\"summary\":{\"distributions\":");
        let _ = io::Write::write_fmt(b, format_args!("{}", self.rows.len()));
        key_u32(b, "used", self.used() as u32);
        key_u32(b, "listed", listed);
        key_u32(b, "first_party_files", self.first_party_files);
        if let Some(s) = &self.scores {
            key_u32(b, "scorecards_computed", s.computed);
            key_u32(b, "scorecards_cached", s.cached);
        }
        b.extend_from_slice(b"}}\n");
        out.maybe_flush();
    }
}

/// The scorecard cells of a row: grade, score, loc, hot/kloc, mi, cc, deps.
fn score_cells(t: &mut Table, sc: Option<&Scorecard>) {
    let Some(sc) = sc else {
        for _ in 0..7 {
            t.text(b"-");
        }
        return;
    };
    match sc.grade() {
        Some(g) => t.text(g.as_str().as_bytes()),
        None => t.text(b"-"),
    }
    if sc.functions > 0 {
        t.fmt(format_args!("{:.1}", sc.score));
        t.fmt(format_args!("{}", sc.loc));
        t.fmt(format_args!("{:.1}", sc.hotspot_density));
        t.fmt(format_args!("{:.1}", sc.mi_mean));
        t.fmt(format_args!("{:.1}", sc.cc_mean));
    } else {
        t.text(b"-");
        t.fmt(format_args!("{}", sc.loc));
        for _ in 0..3 {
            t.text(b"-");
        }
    }
    t.fmt(format_args!("{}", sc.external_deps));
}

/// A scorecard as a JSON object.
fn push_scorecard(b: &mut Vec<u8>, sc: &Scorecard) {
    b.extend_from_slice(b"{\"grade\":");
    match sc.grade() {
        Some(g) => push_str(b, g.as_str().as_bytes()),
        None => b.extend_from_slice(b"null"),
    }
    key_real(b, "score", sc.score);
    key_u32(b, "files", sc.files);
    key_u32(b, "stub_files", sc.stub_files);
    key_u32(b, "loc", sc.loc);
    key_u32(b, "code_lines", sc.code_lines);
    key_u32(b, "classes", sc.classes);
    key_u32(b, "functions", sc.functions);
    key_u32(b, "public_api", sc.public_api);
    key_real(b, "cc_mean", sc.cc_mean);
    key_u32(b, "cc_max", sc.cc_max);
    key_real(b, "cognitive_mean", sc.cog_mean);
    key_u32(b, "cognitive_max", sc.cog_max);
    key_u32(b, "nesting_max", sc.nesting_max);
    key_u32(b, "hotspots", sc.hotspots);
    key_real(b, "hotspot_density", sc.hotspot_density);
    key_real(b, "mi_mean", sc.mi_mean);
    key_real(b, "mi_min", sc.mi_min);
    key_u32(b, "internal_edges", sc.internal_edges);
    key_real(b, "fan_out_mean", sc.fan_out_mean);
    key_u32(b, "fan_out_max", sc.fan_out_max);
    key_u32(b, "cyclic_modules", sc.cyclic_modules);
    key_u32(b, "external_deps", sc.external_deps);
    b.push(b'}');
}
