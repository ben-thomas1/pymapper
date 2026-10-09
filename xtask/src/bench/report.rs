//! Aggregation of runs, the JSON results file, the report table and regression comparison.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, bail};
use serde_json::{Value, json};

use super::targets::RunRecord;

/// Version of the results-file layout; bump on incompatible changes.
pub const SCHEMA: u64 = 1;

/// Aggregate of the measured runs of one (corpus, target): best-of-N times, worst-of-N RSS.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub corpus: String,
    pub target: String,
    pub root: String,
    pub files: u64,
    pub bytes: u64,
    pub errors: u64,
    pub threads: u64,
    pub runs: u64,
    /// Minimum over runs of each phase, in first-recorded order.
    pub phases: Vec<(String, f64)>,
    /// Minimum total wall time over runs.
    pub total_s: f64,
    /// Median total wall time over runs (informational; comparisons use the minimum).
    pub total_median_s: f64,
    /// Maximum peak RSS over runs.
    pub peak_rss_bytes: u64,
    pub counters: Vec<(String, u64)>,
}

impl Summary {
    /// Aggregates `runs`; `None` if there are none.
    pub fn from_runs(corpus: &str, target: &str, root: &Path, runs: &[RunRecord]) -> Option<Summary> {
        let last = runs.last()?;
        let mut phases: Vec<(String, f64)> = Vec::new();
        let mut totals: Vec<f64> = Vec::with_capacity(runs.len());
        let mut peak = 0;
        for run in runs {
            totals.push(run.total_s);
            peak = peak.max(run.peak_rss_bytes);
            for (name, s) in &run.phases {
                // A handful of phases: a linear lookup is cheapest.
                match phases.iter_mut().find(|(n, _)| n == name) {
                    Some((_, best)) => *best = best.min(*s),
                    None => phases.push((name.clone(), *s)),
                }
            }
        }
        totals.sort_by(f64::total_cmp);
        let mid = totals.len() / 2;
        let median = if totals.len() % 2 == 1 { totals[mid] } else { (totals[mid - 1] + totals[mid]) / 2.0 };
        Some(Summary {
            corpus: corpus.to_owned(),
            target: target.to_owned(),
            root: root.to_string_lossy().into_owned(),
            files: last.files,
            bytes: last.bytes,
            errors: last.errors,
            threads: last.threads,
            runs: runs.len() as u64,
            phases,
            total_s: totals[0],
            total_median_s: median,
            peak_rss_bytes: peak,
            counters: last.counters.clone(),
        })
    }

    fn mb_per_s(&self) -> f64 {
        if self.total_s > 0.0 { self.bytes as f64 / 1e6 / self.total_s } else { 0.0 }
    }

    fn phase(&self, name: &str) -> Option<f64> {
        self.phases.iter().find(|(n, _)| n == name).map(|(_, s)| *s)
    }
}

/// Everything written to / read from a results file.
#[derive(Clone, Debug, PartialEq)]
pub struct ResultsFile {
    pub created_unix: u64,
    pub git_rev: Option<String>,
    pub os: String,
    pub arch: String,
    pub results: Vec<Summary>,
}

// ---- JSON -------------------------------------------------------------------------------------

fn pairs_f64(pairs: &[(String, f64)]) -> Value {
    Value::Array(pairs.iter().map(|(n, s)| json!({ "name": n, "s": s })).collect())
}

fn pairs_u64(pairs: &[(String, u64)]) -> Value {
    Value::Array(pairs.iter().map(|(n, v)| json!({ "name": n, "n": v })).collect())
}

fn field<'a>(v: &'a Value, key: &str) -> anyhow::Result<&'a Value> {
    v.get(key).with_context(|| format!("missing field {key:?}"))
}

fn get_u64(v: &Value, key: &str) -> anyhow::Result<u64> {
    field(v, key)?.as_u64().with_context(|| format!("field {key:?} is not an unsigned integer"))
}

fn get_f64(v: &Value, key: &str) -> anyhow::Result<f64> {
    field(v, key)?.as_f64().with_context(|| format!("field {key:?} is not a number"))
}

fn get_str(v: &Value, key: &str) -> anyhow::Result<String> {
    Ok(field(v, key)?.as_str().with_context(|| format!("field {key:?} is not a string"))?.to_owned())
}

fn get_array<'a>(v: &'a Value, key: &str) -> anyhow::Result<&'a Vec<Value>> {
    field(v, key)?.as_array().with_context(|| format!("field {key:?} is not an array"))
}

fn get_pairs_f64(v: &Value, key: &str) -> anyhow::Result<Vec<(String, f64)>> {
    get_array(v, key)?.iter().map(|p| Ok((get_str(p, "name")?, get_f64(p, "s")?))).collect()
}

fn get_pairs_u64(v: &Value, key: &str) -> anyhow::Result<Vec<(String, u64)>> {
    get_array(v, key)?.iter().map(|p| Ok((get_str(p, "name")?, get_u64(p, "n")?))).collect()
}

impl RunRecord {
    pub fn to_json(&self) -> Value {
        json!({
            "files": self.files,
            "bytes": self.bytes,
            "errors": self.errors,
            "threads": self.threads,
            "phases": pairs_f64(&self.phases),
            "total_s": self.total_s,
            "peak_rss_bytes": self.peak_rss_bytes,
            "counters": pairs_u64(&self.counters),
        })
    }

    pub fn from_json(v: &Value) -> anyhow::Result<RunRecord> {
        Ok(RunRecord {
            files: get_u64(v, "files")?,
            bytes: get_u64(v, "bytes")?,
            errors: get_u64(v, "errors")?,
            threads: get_u64(v, "threads")?,
            phases: get_pairs_f64(v, "phases")?,
            total_s: get_f64(v, "total_s")?,
            peak_rss_bytes: get_u64(v, "peak_rss_bytes")?,
            counters: get_pairs_u64(v, "counters")?,
        })
    }
}

impl Summary {
    fn to_json(&self) -> Value {
        json!({
            "corpus": self.corpus,
            "target": self.target,
            "root": self.root,
            "files": self.files,
            "bytes": self.bytes,
            "errors": self.errors,
            "threads": self.threads,
            "runs": self.runs,
            "phases": pairs_f64(&self.phases),
            "total_s": self.total_s,
            "total_median_s": self.total_median_s,
            "peak_rss_bytes": self.peak_rss_bytes,
            "counters": pairs_u64(&self.counters),
        })
    }

    fn from_json(v: &Value) -> anyhow::Result<Summary> {
        Ok(Summary {
            corpus: get_str(v, "corpus")?,
            target: get_str(v, "target")?,
            root: get_str(v, "root")?,
            files: get_u64(v, "files")?,
            bytes: get_u64(v, "bytes")?,
            errors: get_u64(v, "errors")?,
            threads: get_u64(v, "threads")?,
            runs: get_u64(v, "runs")?,
            phases: get_pairs_f64(v, "phases")?,
            total_s: get_f64(v, "total_s")?,
            total_median_s: get_f64(v, "total_median_s")?,
            peak_rss_bytes: get_u64(v, "peak_rss_bytes")?,
            counters: get_pairs_u64(v, "counters")?,
        })
    }
}

impl ResultsFile {
    pub fn to_json_string(&self) -> String {
        let v = json!({
            "schema": SCHEMA,
            "created_unix": self.created_unix,
            "git_rev": self.git_rev,
            "os": self.os,
            "arch": self.arch,
            "results": Value::Array(self.results.iter().map(Summary::to_json).collect()),
        });
        let mut s = serde_json::to_string_pretty(&v).expect("a serde_json::Value always serialises");
        s.push('\n');
        s
    }

    pub fn parse(text: &str) -> anyhow::Result<ResultsFile> {
        let v: Value = serde_json::from_str(text).context("invalid JSON")?;
        let schema = get_u64(&v, "schema")?;
        if schema != SCHEMA {
            bail!("unsupported results schema {schema} (expected {SCHEMA})");
        }
        let git_rev = match field(&v, "git_rev")? {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            _ => bail!("field \"git_rev\" is not a string or null"),
        };
        let results = get_array(&v, "results")?
            .iter()
            .enumerate()
            .map(|(i, r)| Summary::from_json(r).with_context(|| format!("results[{i}]")))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(ResultsFile {
            created_unix: get_u64(&v, "created_unix")?,
            git_rev,
            os: get_str(&v, "os")?,
            arch: get_str(&v, "arch")?,
            results,
        })
    }
}

// ---- Formatting -------------------------------------------------------------------------------

pub fn fmt_secs(s: f64) -> String {
    let ms = s * 1e3;
    if ms < 10.0 {
        format!("{ms:.2}ms")
    } else if ms < 1000.0 {
        format!("{ms:.1}ms")
    } else {
        format!("{s:.2}s")
    }
}

pub fn fmt_mb(bytes: f64) -> String {
    let mb = bytes / 1e6;
    if mb < 10.0 { format!("{mb:.2}") } else { format!("{mb:.1}") }
}

/// Lays out rows as aligned columns separated by two spaces; `right[i]` right-aligns column `i`.
fn layout(rows: &[Vec<String>], right: &[bool]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; cols];
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            let pad = widths[i] - cell.chars().count();
            if right.get(i).copied().unwrap_or(false) {
                line.extend(std::iter::repeat_n(' ', pad));
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.extend(std::iter::repeat_n(' ', pad));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The results table: one row per (corpus, target), one column per phase seen in any row.
pub fn render_table(results: &[Summary]) -> String {
    let mut phase_names: Vec<&str> = Vec::new();
    for r in results {
        for (n, _) in &r.phases {
            if !phase_names.contains(&n.as_str()) {
                phase_names.push(n);
            }
        }
    }
    let with_counters = results.iter().any(|r| !r.counters.is_empty());
    let mut header: Vec<String> = ["corpus", "target", "files", "MB"].map(String::from).to_vec();
    header.extend(phase_names.iter().map(|n| (*n).to_owned()));
    header.extend(["total", "MB/s", "peak RSS MB"].map(String::from));
    if with_counters {
        header.push("counters".to_owned());
    }
    let mut right = vec![false, false];
    right.resize(header.len(), true);
    if with_counters {
        right[header.len() - 1] = false;
    }
    let mut rows = vec![header];
    for r in results {
        let mut row = vec![r.corpus.clone(), r.target.clone(), r.files.to_string(), fmt_mb(r.bytes as f64)];
        for name in &phase_names {
            row.push(r.phase(name).map_or_else(|| "-".to_owned(), fmt_secs));
        }
        row.push(fmt_secs(r.total_s));
        row.push(format!("{:.0}", r.mb_per_s()));
        row.push(fmt_mb(r.peak_rss_bytes as f64));
        if with_counters {
            let mut c = String::new();
            for (i, (n, v)) in r.counters.iter().enumerate() {
                let sep = if i == 0 { "" } else { " " };
                let _ = write!(c, "{sep}{n}={v}");
            }
            row.push(c);
        }
        rows.push(row);
    }
    layout(&rows, &right)
}

// ---- Comparison -------------------------------------------------------------------------------

/// When a metric counts as regressed: worse by more than `pct` percent AND by at least the
/// absolute minimum for its unit (so noise on tiny corpora is not flagged).
#[derive(Clone, Copy, Debug)]
pub struct Thresholds {
    pub pct: f64,
    pub min_delta_s: f64,
    pub min_delta_bytes: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Seconds,
    Bytes,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Delta {
    pub corpus: String,
    pub target: String,
    pub metric: String,
    pub unit: Unit,
    pub base: f64,
    pub cur: f64,
    pub regression: bool,
}

#[derive(Debug, Default)]
pub struct Comparison {
    pub deltas: Vec<Delta>,
    pub notes: Vec<String>,
}

impl Comparison {
    pub fn regressions(&self) -> usize {
        self.deltas.iter().filter(|d| d.regression).count()
    }
}

/// Higher is worse for every metric compared here (time, memory).
pub fn is_regression(base: f64, cur: f64, pct: f64, min_delta: f64) -> bool {
    let delta = cur - base;
    delta > 0.0 && delta >= min_delta && cur > base * (1.0 + pct / 100.0)
}

/// Compares `cur` against `base`, matching entries by (corpus, target).
pub fn compare(base: &[Summary], cur: &[Summary], th: &Thresholds) -> Comparison {
    let index: HashMap<(&str, &str), &Summary> =
        base.iter().map(|s| ((s.corpus.as_str(), s.target.as_str()), s)).collect();
    let mut cmp = Comparison::default();
    for c in cur {
        let key = format!("{}/{}", c.corpus, c.target);
        let Some(b) = index.get(&(c.corpus.as_str(), c.target.as_str())) else {
            cmp.notes.push(format!("{key}: not in baseline"));
            continue;
        };
        if b.files != c.files || b.bytes != c.bytes {
            cmp.notes.push(format!(
                "{key}: corpus changed since baseline ({} -> {} files, {} -> {} MB); comparison is approximate",
                b.files,
                c.files,
                fmt_mb(b.bytes as f64),
                fmt_mb(c.bytes as f64)
            ));
        }
        if b.threads != c.threads {
            cmp.notes.push(format!("{key}: thread count changed ({} -> {})", b.threads, c.threads));
        }
        let mut push = |metric: String, unit: Unit, base: f64, cur: f64| {
            let min = match unit {
                Unit::Seconds => th.min_delta_s,
                Unit::Bytes => th.min_delta_bytes,
            };
            let regression = is_regression(base, cur, th.pct, min);
            cmp.deltas.push(Delta { corpus: c.corpus.clone(), target: c.target.clone(), metric, unit, base, cur, regression });
        };
        for (name, s) in &c.phases {
            match b.phase(name) {
                Some(bs) => push(name.clone(), Unit::Seconds, bs, *s),
                None => cmp.notes.push(format!("{key}: phase {name:?} not in baseline")),
            }
        }
        push("total".to_owned(), Unit::Seconds, b.total_s, c.total_s);
        push("peak RSS".to_owned(), Unit::Bytes, b.peak_rss_bytes as f64, c.peak_rss_bytes as f64);
    }
    cmp
}

pub fn render_comparison(cmp: &Comparison) -> String {
    let mut rows = vec![["corpus", "target", "metric", "baseline", "current", "change", ""].map(String::from).to_vec()];
    for d in &cmp.deltas {
        let fmt = |v: f64| match d.unit {
            Unit::Seconds => fmt_secs(v),
            Unit::Bytes => format!("{} MB", fmt_mb(v)),
        };
        let change = if d.base > 0.0 { format!("{:+.1}%", (d.cur / d.base - 1.0) * 100.0) } else { "new".to_owned() };
        let flag = if d.regression { "REGRESSION" } else { "" };
        rows.push(vec![d.corpus.clone(), d.target.clone(), d.metric.clone(), fmt(d.base), fmt(d.cur), change, flag.to_owned()]);
    }
    let mut out = layout(&rows, &[false, false, false, true, true, true, false]);
    for n in &cmp.notes {
        out.push_str("note: ");
        out.push_str(n);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(total: f64, walk: f64, read: f64, rss: u64) -> RunRecord {
        RunRecord {
            files: 10,
            bytes: 2_000_000,
            errors: 0,
            threads: 4,
            phases: vec![("walk".to_owned(), walk), ("read".to_owned(), read)],
            total_s: total,
            peak_rss_bytes: rss,
            counters: vec![],
        }
    }

    fn summary(corpus: &str, total: f64, rss: u64) -> Summary {
        Summary::from_runs(corpus, "read", Path::new("/c"), &[run(total, total / 4.0, total / 2.0, rss)]).unwrap()
    }

    const TH: Thresholds = Thresholds { pct: 10.0, min_delta_s: 0.005, min_delta_bytes: 1e6 };

    #[test]
    fn from_runs_takes_min_times_max_rss_and_median() {
        let runs = [run(0.30, 0.10, 0.15, 5), run(0.20, 0.12, 0.05, 9), run(0.25, 0.08, 0.10, 7)];
        let s = Summary::from_runs("tests", "read", Path::new("/c"), &runs).unwrap();
        assert_eq!(s.runs, 3);
        assert_eq!(s.total_s, 0.20);
        assert_eq!(s.total_median_s, 0.25);
        assert_eq!(s.peak_rss_bytes, 9);
        assert_eq!(s.phases, vec![("walk".to_owned(), 0.08), ("read".to_owned(), 0.05)]);
        let even = Summary::from_runs("t", "read", Path::new("/c"), &runs[..2]).unwrap();
        assert!((even.total_median_s - 0.25).abs() < 1e-12);
        assert!(Summary::from_runs("t", "read", Path::new("/c"), &[]).is_none());
    }

    #[test]
    fn from_runs_unions_phases_in_first_seen_order() {
        let mut a = run(1.0, 0.1, 0.2, 1);
        a.phases.truncate(1);
        let mut b = run(1.0, 0.3, 0.4, 1);
        b.phases.reverse();
        b.phases.push(("lex".to_owned(), 0.5));
        let s = Summary::from_runs("t", "read", Path::new("/c"), &[a, b]).unwrap();
        let names: Vec<&str> = s.phases.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["walk", "read", "lex"]);
        assert_eq!(s.phases[0].1, 0.1);
    }

    #[test]
    fn run_record_json_round_trips() {
        let mut r = run(1.5, 0.5, 1.0, 123_456_789);
        r.counters = vec![("tokens".to_owned(), 42)];
        let text = r.to_json().to_string();
        assert_eq!(RunRecord::from_json(&serde_json::from_str(&text).unwrap()).unwrap(), r);
    }

    #[test]
    fn results_file_round_trips() {
        let mut s = summary("tests", 0.25, 10_000_000);
        s.counters = vec![("tokens".to_owned(), 7), ("defs".to_owned(), 3)];
        let file = ResultsFile {
            created_unix: 1_700_000_000,
            git_rev: Some("abc123-dirty".to_owned()),
            os: "macos".to_owned(),
            arch: "aarch64".to_owned(),
            results: vec![s, summary("stdlib", 1.0, 50_000_000)],
        };
        assert_eq!(ResultsFile::parse(&file.to_json_string()).unwrap(), file);
        let none = ResultsFile { git_rev: None, ..file };
        assert_eq!(ResultsFile::parse(&none.to_json_string()).unwrap(), none);
    }

    #[test]
    fn results_file_rejects_bad_input() {
        assert!(ResultsFile::parse("not json").is_err());
        assert!(ResultsFile::parse("{}").is_err());
        let wrong_schema = r#"{"schema": 999, "created_unix": 0, "git_rev": null, "os": "", "arch": "", "results": []}"#;
        let err = ResultsFile::parse(wrong_schema).unwrap_err().to_string();
        assert!(err.contains("schema"), "{err}");
        let bad_entry = r#"{"schema": 1, "created_unix": 0, "git_rev": null, "os": "", "arch": "", "results": [{"corpus": 1}]}"#;
        let err = format!("{:#}", ResultsFile::parse(bad_entry).unwrap_err());
        assert!(err.contains("results[0]"), "{err}");
    }

    #[test]
    fn regression_needs_both_relative_and_absolute_worsening() {
        assert!(is_regression(1.0, 1.2, 10.0, 0.005));
        assert!(!is_regression(1.0, 1.05, 10.0, 0.005), "within threshold");
        assert!(!is_regression(0.001, 0.003, 10.0, 0.005), "tripled but below absolute floor");
        assert!(!is_regression(1.0, 0.5, 10.0, 0.0), "improvement");
        assert!(!is_regression(1.0, 1.0, 0.0, 0.0), "unchanged");
        assert!(is_regression(0.0, 1.0, 10.0, 0.5), "from zero");
        assert!(!is_regression(1.0, f64::NAN, 10.0, 0.0), "NaN never flags");
    }

    #[test]
    fn compare_flags_regressions_and_notes() {
        let base = vec![summary("tests", 1.0, 10_000_000), summary("stdlib", 1.0, 10_000_000)];
        let mut changed = summary("stdlib", 0.9, 30_000_000);
        changed.files = 11;
        let cur = vec![summary("tests", 1.5, 10_000_000), changed, summary("venv", 1.0, 1)];
        let cmp = compare(&base, &cur, &TH);
        let flagged: Vec<(&str, &str)> =
            cmp.deltas.iter().filter(|d| d.regression).map(|d| (d.corpus.as_str(), d.metric.as_str())).collect();
        // tests: walk 0.25->0.375, read 0.5->0.75, total 1.0->1.5; stdlib: RSS 10->30 MB.
        assert_eq!(flagged, [("tests", "walk"), ("tests", "read"), ("tests", "total"), ("stdlib", "peak RSS")]);
        assert_eq!(cmp.regressions(), 4);
        assert_eq!(cmp.deltas.len(), 8, "4 metrics for each matched entry");
        assert!(cmp.notes.iter().any(|n| n.contains("stdlib/read: corpus changed")), "{:?}", cmp.notes);
        assert!(cmp.notes.iter().any(|n| n == "venv/read: not in baseline"), "{:?}", cmp.notes);
        let text = render_comparison(&cmp);
        assert_eq!(text.matches("REGRESSION").count(), 4, "{text}");
        assert!(text.contains("+50.0%"), "{text}");
    }

    #[test]
    fn compare_same_results_is_clean() {
        let base = vec![summary("tests", 1.0, 10_000_000)];
        let cmp = compare(&base, &base, &TH);
        assert_eq!(cmp.regressions(), 0);
        assert!(cmp.notes.is_empty());
    }

    #[test]
    fn compare_notes_new_phases() {
        let base = vec![summary("tests", 1.0, 1)];
        let mut cur = base.clone();
        cur[0].phases.push(("lex".to_owned(), 0.1));
        let cmp = compare(&base, &cur, &TH);
        assert!(cmp.notes.iter().any(|n| n.contains("phase \"lex\" not in baseline")), "{:?}", cmp.notes);
    }

    #[test]
    fn table_has_a_column_per_phase() {
        let mut a = summary("tests", 0.002, 3_000_000);
        a.counters = vec![("tokens".to_owned(), 99)];
        let mut b = summary("stdlib", 2.5, 120_000_000);
        b.phases.push(("lex".to_owned(), 1.0));
        let text = render_table(&[a, b]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        for col in ["corpus", "target", "files", "MB", "walk", "read", "lex", "total", "MB/s", "peak RSS MB", "counters"] {
            assert!(lines[0].contains(col), "missing {col}: {text}");
        }
        assert!(lines[1].starts_with("tests "), "{text}");
        assert!(lines[1].contains("2.00ms"), "{text}");
        assert!(lines[1].contains("tokens=99"), "{text}");
        assert!(lines[1].contains(" - "), "missing phase shown as '-': {text}");
        assert!(lines[2].contains("2.50s"), "{text}");
        assert!(lines[2].contains("120.0"), "{text}");
        // 2 MB in 2.5 s.
        assert!(lines[2].contains(" 1 "), "{text}");
    }

    #[test]
    fn formatting() {
        assert_eq!(fmt_secs(0.001234), "1.23ms");
        assert_eq!(fmt_secs(0.0456), "45.6ms");
        assert_eq!(fmt_secs(3.2), "3.20s");
        assert_eq!(fmt_mb(620_000_000.0), "620.0");
        assert_eq!(fmt_mb(1_500_000.0), "1.50");
    }
}
