//! Output tests: ordering, every format against golden files, footer, exit status, EPIPE,
//! buffering, baselines and the JSON reader.
//!
//! Golden files live in `tests/output_golden/`; regenerate after an intended change with
//! `PYMAPPER_BLESS=1 cargo test -p pymapper-core output::` and review the diff.

use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;

use super::jsonread::Reader;
use super::*;
use crate::rules::codes::*;
use crate::rules::{Arg, ArgResolver, Location};

/// A context over plain tables: file `i` is `paths[i]`, symbol `i` is `symbols[i]`, name `i`
/// is `names[i]`, contract `i` is `contracts[i]`.
pub(super) struct TestCtx {
    pub paths: Vec<Vec<u8>>,
    pub symbols: Vec<&'static str>,
    pub names: Vec<&'static str>,
    pub contracts: Vec<&'static str>,
}

impl Context for TestCtx {
    fn write_path(&self, file: FileId, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.paths[file.0 as usize]);
    }
    fn write_symbol_id(&self, sym: SymbolId, out: &mut Vec<u8>) {
        out.extend_from_slice(self.symbols[sym.0 as usize].as_bytes());
    }
    fn def_end_line(&self, sym: SymbolId) -> u32 {
        // `pkg.zeta:handle` and `pkg.alpha:Big.run` are definitions.
        match sym.0 {
            0 => 30,
            2 => 25,
            _ => 0,
        }
    }
}

impl ArgResolver for TestCtx {
    fn write_symbol(&self, id: SymbolId, out: &mut dyn fmt::Write) -> fmt::Result {
        out.write_str(self.symbols[id.0 as usize])
    }
    fn write_name(&self, id: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        out.write_str(self.names[id as usize])
    }
    fn write_file(&self, id: FileId, out: &mut dyn fmt::Write) -> fmt::Result {
        super::context::write_lossy(&self.paths[id.0 as usize], out)
    }
    fn write_contract(&self, index: u32, out: &mut dyn fmt::Write) -> fmt::Result {
        out.write_str(self.contracts[index as usize])
    }
}

fn ctx() -> TestCtx {
    TestCtx {
        paths: vec![
            b"pkg/zeta.py".to_vec(),
            b"pkg/alpha.py".to_vec(),
            b"odd dir/a,b:c%d\"e.py".to_vec(),
            b"pkg/caf\xc3\xa9/\xff.py".to_vec(),
            b"pkg/__init__.py".to_vec(),
        ],
        symbols: vec!["pkg.zeta:handle", "pkg.alpha", "pkg.alpha:Big.run", "pkg", "pkg.zeta", "odd:f"],
        names: vec!["yaml", "PyYAML", "os.path", "we\"ird\nname", "requests"],
        contracts: vec!["core-layers"],
    }
}

fn loc(file: u32, line: u32, col: u32) -> Location {
    Location::new(FileId(file), line, col)
}

const NOFILE: Location = Location { file: FileId::NONE, line: 0, col: 0 };

/// A sample covering every format feature, pushed out of order.
fn sample() -> Findings {
    let mut f = Findings::new();
    let s = SymbolId;
    f.push(COMPLEX_FUNCTION, loc(0, 12, 5), s(0), 100, &[Arg::Symbol(s(0)), Arg::U32(14), Arg::U32(10)]);
    f.push(UNUSED_IMPORT, loc(1, 3, 1), s(1), 90, &[Arg::Name(2)]);
    let cycle = [Arg::U32(2), Arg::Phrase(1), Arg::Symbol(s(1)), Arg::Symbol(s(4)), Arg::Symbol(s(1))];
    f.push_ext(IMPORT_CYCLE, NOFILE, 0, SymbolId::NONE, 100, &cycle, &[Arg::Symbol(s(1)), Arg::Symbol(s(4))]);
    let other = [Arg::File(FileId(1)), Arg::U32(40), Arg::U32(47), Arg::Symbol(s(2))];
    f.push_ext(DUPLICATE_CODE, loc(0, 14, 9), 21, s(0), 96, &[Arg::U32(8), Arg::File(FileId(1)), Arg::U32(40)], &other);
    f.push(LOW_MAINTAINABILITY, loc(1, 20, 5), s(2), 100, &[Arg::Symbol(s(2)), Arg::F32(12.25), Arg::F32(20.0)]);
    f.push(UNUSED_FUNCTION, loc(2, 1, 1), s(5), 60, &[Arg::Symbol(s(5)), Arg::Phrase(0)]);
    f.push(UNDECLARED_DEPENDENCY, loc(3, 2, 0), SymbolId::NONE, 100, &[Arg::Name(0), Arg::Name(1)]);
    f.push(LONG_MODULE, loc(4, 0, 0), s(3), 100, &[Arg::Symbol(s(3)), Arg::U32(900), Arg::U32(500)]);
    f.push(UNUSED_DEPENDENCY, NOFILE, SymbolId::NONE, 100, &[Arg::Name(4)]);
    f.push(FORBIDDEN_IMPORT, loc(1, 3, 1), s(1), 100, &[Arg::Symbol(s(1)), Arg::Name(3), Arg::Contract(0)]);
    // Same rule, place and symbol as the previous UNUSED_IMPORT: ordered by message.
    f.push(UNUSED_IMPORT, loc(1, 3, 1), s(1), 90, &[Arg::Name(0)]);
    f
}

fn summary() -> Summary {
    Summary {
        files_checked: 42,
        filter: FilterStats { reported: 9, below_confidence: 2, disabled: 0, file_ignored: 3, suppressed: 1 },
    }
}

fn render(findings: &Findings, c: &TestCtx, opts: &Options, sum: &Summary) -> String {
    let r = Report::new(findings, c);
    let mut out = Vec::new();
    assert_eq!(r.write(opts, sum, &mut out).expect("write"), Outcome::Written);
    String::from_utf8(out).expect("output is UTF-8")
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/output_golden")
}

/// Compares with (or, under `PYMAPPER_BLESS`, writes) a golden file. The crate version is
/// replaced by `<version>` so version bumps do not touch the goldens.
fn check_golden(name: &str, got: &str) {
    let got = got.replace(&format!("\"{}\"", env!("CARGO_PKG_VERSION")), "\"<version>\"");
    let path = golden_dir().join(name);
    if std::env::var_os("PYMAPPER_BLESS").is_some() {
        std::fs::create_dir_all(golden_dir()).expect("mkdir golden");
        std::fs::write(&path, &got).expect("write golden");
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(got == want, "{name} differs from the golden file\n--- got ---\n{got}--- want ---\n{want}");
}

/// Validates a JSON document with the reader (well-formed, single value).
fn assert_json(text: &str) {
    let mut rd = Reader::new(text.as_bytes());
    if let Err(e) = rd.skip().and_then(|()| rd.end()) {
        panic!("invalid JSON at {}: {}\n{text}", e.offset, e.what);
    }
}

// ---------------------------------------------------------------------------------------------
// Ordering and formats
// ---------------------------------------------------------------------------------------------

#[test]
fn order_is_by_path_then_position_then_rule_then_message() {
    let f = sample();
    let c = ctx();
    let r = Report::new(&f, &c);
    let got: Vec<(String, u32, u32, u16)> = r
        .iter()
        .map(|x| (String::from_utf8_lossy(r.path(x)).into_owned(), x.loc.line, x.loc.col, x.rule.code()))
        .collect();
    let want = [
        ("", 0, 0, 301),
        ("", 0, 0, 602),
        ("odd dir/a,b:c%d\"e.py", 1, 1, 401),
        ("pkg/__init__.py", 0, 0, 107),
        ("pkg/alpha.py", 3, 1, 303),
        ("pkg/alpha.py", 3, 1, 405),
        ("pkg/alpha.py", 3, 1, 405),
        ("pkg/alpha.py", 20, 5, 106),
        ("pkg/caf\u{e9}/\u{fffd}.py", 2, 0, 601),
        ("pkg/zeta.py", 12, 5, 101),
        ("pkg/zeta.py", 14, 9, 501),
    ];
    let want: Vec<(String, u32, u32, u16)> = want.iter().map(|&(p, l, c, r)| (p.to_owned(), l, c, r)).collect();
    assert_eq!(got, want);
    // The two PM405 findings tie on everything but the message: `os.path` < `yaml`.
    let mut msg = String::new();
    let msgs: Vec<String> = r
        .iter()
        .filter(|x| x.rule == UNUSED_IMPORT)
        .map(|x| {
            r.message(x, &mut msg);
            msg.clone()
        })
        .collect();
    assert_eq!(msgs, ["`os.path` is imported but never used", "`yaml` is imported but never used"]);
}

#[test]
fn order_does_not_depend_on_push_order() {
    let f = sample();
    let c = ctx();
    let opts = Options::new(OutputFormat::Json);
    let want = render(&f, &c, &opts, &summary());
    // Reversed and rotated pushes give byte-identical output.
    let items: Vec<_> = f.iter().collect();
    for rot in 0..items.len() {
        let mut g = Findings::new();
        for k in 0..items.len() {
            let (x, args) = items[(items.len() - 1 - k + rot) % items.len()];
            g.push_full(x.rule, x.loc, x.end_line, x.symbol, x.confidence, x.reasons, args, f.extra(x));
        }
        assert_eq!(render(&g, &c, &opts, &summary()), want, "rotation {rot}");
    }
}

#[test]
fn concise_golden() {
    let got = render(&sample(), &ctx(), &Options::new(OutputFormat::Concise), &summary());
    check_golden("sample.concise.txt", &got);
}

#[test]
fn github_golden() {
    let got = render(&sample(), &ctx(), &Options::new(OutputFormat::Github), &summary());
    check_golden("sample.github.txt", &got);
}

#[test]
fn json_golden() {
    let got = render(&sample(), &ctx(), &Options::new(OutputFormat::Json), &summary());
    assert_json(&got);
    check_golden("sample.json", &got);
}

#[test]
fn sarif_golden() {
    let got = render(&sample(), &ctx(), &Options::new(OutputFormat::Sarif), &summary());
    assert_json(&got);
    check_golden("sample.sarif", &got);
}

#[test]
fn baseline_golden() {
    let f = sample();
    let c = ctx();
    let r = Report::new(&f, &c);
    let mut out = Vec::new();
    assert_eq!(r.write_baseline(&mut out).expect("write"), Outcome::Written);
    let got = String::from_utf8(out).expect("UTF-8");
    assert_json(&got);
    check_golden("sample.baseline.json", &got);
}

#[test]
fn empty_report_in_every_format() {
    let f = Findings::new();
    let c = ctx();
    let sum = Summary::default();
    for format in [OutputFormat::Concise, OutputFormat::Github] {
        assert_eq!(render(&f, &c, &Options::new(format), &sum), "All checks passed!\n");
    }
    let json = render(&f, &c, &Options::new(OutputFormat::Json), &sum);
    assert_json(&json);
    assert!(json.contains("\"findings\":[],"), "{json}");
    let sarif = render(&f, &c, &Options::new(OutputFormat::Sarif), &sum);
    assert_json(&sarif);
    assert!(sarif.contains("\"rules\":[]") && sarif.contains("\"results\": []"), "{sarif}");
    let with_files = Summary { files_checked: 1, ..Summary::default() };
    assert_eq!(
        render(&f, &c, &Options::new(OutputFormat::Concise), &with_files),
        "All checks passed! (1 file checked)\n"
    );
}

#[test]
fn footer_can_be_disabled() {
    let mut f = Findings::new();
    f.push(UNUSED_FUNCTION, loc(0, 1, 1), SymbolId(0), 100, &[Arg::Symbol(SymbolId(0)), Arg::Phrase(0)]);
    let opts = Options { footer: false, ..Options::new(OutputFormat::Concise) };
    assert_eq!(
        render(&f, &ctx(), &opts, &summary()),
        "pkg/zeta.py:1:1: PM401 function `pkg.zeta:handle` is never used\n"
    );
}

#[test]
fn footer_counts() {
    let mut f = Findings::new();
    f.push(UNUSED_FUNCTION, loc(0, 1, 1), SymbolId(0), 100, &[Arg::Symbol(SymbolId(0)), Arg::Phrase(0)]);
    f.push(UNUSED_FUNCTION, loc(0, 9, 1), SymbolId(0), 50, &[Arg::Symbol(SymbolId(0)), Arg::Phrase(0)]);
    f.push(UNUSED_FUNCTION, loc(1, 9, 1), SymbolId(1), 100, &[Arg::Symbol(SymbolId(1)), Arg::Phrase(0)]);
    let c = ctx();
    let sum = Summary { files_checked: 0, filter: FilterStats { below_confidence: 1, ..FilterStats::default() } };
    let out = render(&f, &c, &Options::new(OutputFormat::Concise), &sum);
    // The one below-gate finding is shown, so none is hidden.
    assert!(out.ends_with("Found 3 findings in 2 files (1 below min-confidence 80).\n"), "{out}");
    let mut one = Findings::new();
    one.push(UNUSED_FUNCTION, loc(0, 1, 1), SymbolId(0), 100, &[Arg::Symbol(SymbolId(0)), Arg::Phrase(0)]);
    let sum = Summary { files_checked: 1, filter: FilterStats { below_confidence: 4, ..FilterStats::default() } };
    let out = render(&one, &c, &Options::new(OutputFormat::Concise), &sum);
    assert!(out.ends_with("Found 1 finding in 1 file (1 file checked).\nNot shown: 4 below min-confidence.\n"), "{out}");
}

#[test]
fn exit_status_follows_the_gate() {
    let mut f = Findings::new();
    let c = ctx();
    assert_eq!(Report::new(&f, &c).exit_status(80), ExitStatus::Clean);
    f.push(UNUSED_FUNCTION, loc(0, 1, 1), SymbolId(0), 79, &[Arg::Symbol(SymbolId(0)), Arg::Phrase(0)]);
    let r = Report::new(&f, &c);
    assert_eq!(r.exit_status(80), ExitStatus::Clean);
    assert_eq!(r.exit_status(79), ExitStatus::Findings);
    assert_eq!(r.exit_status(0), ExitStatus::Findings);
    assert_eq!(r.gated_count(80), 0);
    assert_eq!((ExitStatus::Clean.code(), ExitStatus::Findings.code(), ExitStatus::Error.code()), (0, 1, 2));
}

#[test]
fn retain_hides_findings() {
    let f = sample();
    let c = ctx();
    let mut r = Report::new(&f, &c);
    r.retain(|x| x.confidence >= 80);
    assert_eq!(r.len(), f.len() - 1);
    assert!(r.iter().all(|x| x.rule != UNUSED_FUNCTION));
}

// ---------------------------------------------------------------------------------------------
// Sink behaviour: EPIPE, errors, buffering
// ---------------------------------------------------------------------------------------------

/// Accepts `room` bytes, then fails every write with `kind`. Counts write calls.
struct Failing {
    room: usize,
    kind: io::ErrorKind,
    written: Vec<u8>,
    writes: usize,
    flushes: usize,
    /// Calls after the first failure.
    after_fail: usize,
    failed: bool,
}

impl Failing {
    fn new(room: usize, kind: io::ErrorKind) -> Failing {
        Failing { room, kind, written: Vec::new(), writes: 0, flushes: 0, after_fail: 0, failed: false }
    }
}

impl Write for Failing {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        self.after_fail += usize::from(self.failed);
        if self.room == 0 {
            self.failed = true;
            return Err(io::Error::from(self.kind));
        }
        let n = buf.len().min(self.room);
        self.room -= n;
        self.written.extend_from_slice(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        self.after_fail += usize::from(self.failed);
        if self.room == 0 { Err(io::Error::from(self.kind)) } else { Ok(()) }
    }
}

/// `n` distinct findings spread over a few files.
fn many(n: u32) -> Findings {
    let mut f = Findings::with_capacity(n as usize, n as usize);
    for i in 0..n {
        f.push(UNUSED_IMPORT, loc(i % 5, i + 1, 1), SymbolId(i % 6), 100, &[Arg::Name(i % 5)]);
    }
    f
}

#[test]
fn closed_pipe_is_not_an_error() {
    let f = many(5000);
    let c = ctx();
    let r = Report::new(&f, &c);
    for format in [OutputFormat::Concise, OutputFormat::Json, OutputFormat::Sarif, OutputFormat::Github] {
        for room in [0, 10, 70_000] {
            let mut sink = Failing::new(room, io::ErrorKind::BrokenPipe);
            let got = r.write(&Options::new(format), &summary(), &mut sink).expect("EPIPE is graceful");
            assert_eq!(got, Outcome::Closed, "{format:?} room {room}");
            // Nothing is attempted after the first failure.
            assert_eq!(sink.after_fail, 0, "{format:?}: calls after EPIPE");
            assert_eq!(sink.written.len(), room);
        }
    }
    let mut sink = Failing::new(0, io::ErrorKind::BrokenPipe);
    assert_eq!(r.write_baseline(&mut sink).expect("graceful"), Outcome::Closed);
    assert!(is_broken_pipe(&io::Error::from(io::ErrorKind::BrokenPipe)));
    assert!(!is_broken_pipe(&io::Error::from(io::ErrorKind::PermissionDenied)));
}

#[test]
fn other_write_errors_are_reported() {
    let f = many(10);
    let c = ctx();
    let r = Report::new(&f, &c);
    let mut sink = Failing::new(3, io::ErrorKind::StorageFull);
    let err = r.write(&Options::new(OutputFormat::Concise), &summary(), &mut sink).expect_err("disk full");
    assert_eq!(err.kind(), io::ErrorKind::StorageFull);
}

#[test]
fn output_is_written_in_large_chunks() {
    let f = many(20_000);
    let c = ctx();
    let r = Report::new(&f, &c);
    let mut sink = Failing::new(usize::MAX, io::ErrorKind::Other);
    assert_eq!(r.write(&Options::new(OutputFormat::Concise), &summary(), &mut sink).expect("write"), Outcome::Written);
    let len = sink.written.len();
    assert!(len > 1_000_000, "{len}");
    // One write per 64 KiB chunk (plus the tail), not one per line; flushed once at the end.
    assert!(sink.writes <= len / (64 * 1024) + 1, "{} writes for {len} bytes", sink.writes);
    assert_eq!(sink.flushes, 1);
    let lines = sink.written.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(lines, 20_000 + 2); // + footer (2 lines)
}

#[test]
fn integer_formatting() {
    let mut digits = [0u8; 10];
    for v in [0u32, 7, 10, 99, 100, 12345, u32::MAX] {
        assert_eq!(super::buf::fmt_u32(v, &mut digits), v.to_string().as_bytes());
    }
    let mut hex = Vec::new();
    super::buf::push_hex64(&mut hex, 0x0123_4567_89ab_cdef);
    assert_eq!(hex, b"0123456789abcdef");
}

// ---------------------------------------------------------------------------------------------
// Baselines
// ---------------------------------------------------------------------------------------------

fn baseline_text(f: &Findings, c: &TestCtx) -> String {
    let mut out = Vec::new();
    Report::new(f, c).write_baseline(&mut out).expect("write");
    String::from_utf8(out).expect("UTF-8")
}

fn parse(text: &str) -> Baseline {
    Baseline::parse(text.as_bytes()).unwrap_or_else(|e| panic!("{e}\n{text}"))
}

#[test]
fn baseline_round_trips_and_hides_everything_it_recorded() {
    let f = sample();
    let c = ctx();
    let text = baseline_text(&f, &c);
    let b = parse(&text);
    assert_eq!(b.len(), f.len());
    assert_eq!(b.total(), f.len() as u64);
    // Writing the parsed baseline gives the same file.
    assert_eq!(b.to_string(), text);
    let mut r = Report::new(&f, &c);
    assert_eq!(r.apply_baseline(&b), f.len() as u32);
    assert!(r.is_empty());
    assert_eq!(r.baselined(), f.len() as u32);
    assert_eq!(r.exit_status(0), ExitStatus::Clean);
    let mut out = Vec::new();
    r.write(&Options::new(OutputFormat::Concise), &Summary::default(), &mut out).expect("write");
    assert_eq!(String::from_utf8(out).expect("UTF-8"), "All checks passed!\nNot shown: 11 baselined.\n");
}

#[test]
fn baseline_reports_only_new_findings() {
    let c = ctx();
    let mut old = Findings::new();
    old.push(COMPLEX_FUNCTION, loc(0, 12, 5), SymbolId(0), 100, &[Arg::Symbol(SymbolId(0)), Arg::U32(14), Arg::U32(10)]);
    old.push(UNUSED_IMPORT, loc(1, 3, 1), SymbolId(1), 90, &[Arg::Name(2)]);
    let b = parse(&baseline_text(&old, &c));

    let mut new = Findings::new();
    // Moved (other line/column) and worse (other metric value): still baselined.
    new.push(COMPLEX_FUNCTION, loc(0, 40, 9), SymbolId(0), 100, &[Arg::Symbol(SymbolId(0)), Arg::U32(19), Arg::U32(10)]);
    // Same rule and symbol, other name argument: new.
    new.push(UNUSED_IMPORT, loc(1, 4, 1), SymbolId(1), 90, &[Arg::Name(0)]);
    // Same rule and argument, other symbol: new.
    new.push(UNUSED_IMPORT, loc(0, 3, 1), SymbolId(4), 90, &[Arg::Name(2)]);
    // Recorded: hidden.
    new.push(UNUSED_IMPORT, loc(1, 3, 1), SymbolId(1), 90, &[Arg::Name(2)]);
    // Same symbol, other rule: new.
    new.push(COGNITIVE_COMPLEXITY, loc(0, 40, 9), SymbolId(0), 100, &[Arg::Symbol(SymbolId(0)), Arg::U32(30), Arg::U32(15)]);
    let mut r = Report::new(&new, &c);
    assert_eq!(r.apply_baseline(&b), 2);
    let left: Vec<(u16, u32)> = r.iter().map(|x| (x.rule.code(), x.loc.line)).collect();
    assert_eq!(left, [(405, 4), (405, 3), (102, 40)]);
    assert_eq!(r.exit_status(80), ExitStatus::Findings);
}

#[test]
fn baseline_counts_duplicate_keys() {
    let c = ctx();
    let push = |f: &mut Findings, line: u32| {
        f.push(UNUSED_IMPORT, loc(1, line, 1), SymbolId(1), 90, &[Arg::Name(0)]);
    };
    let mut two = Findings::new();
    push(&mut two, 3);
    push(&mut two, 8);
    let text = baseline_text(&two, &c);
    assert!(text.contains("\"count\": 2"), "{text}");
    let b = parse(&text);
    assert_eq!((b.len(), b.total()), (1, 2));

    let mut three = Findings::new();
    for line in [9, 1, 5] {
        push(&mut three, line);
    }
    let mut r = Report::new(&three, &c);
    assert_eq!(r.apply_baseline(&b), 2);
    // The first two in output order are baselined; the last is reported.
    assert_eq!(r.iter().map(|x| x.loc.line).collect::<Vec<_>>(), [9]);

    let mut one = Findings::new();
    push(&mut one, 7);
    let mut r = Report::new(&one, &c);
    assert_eq!(r.apply_baseline(&b), 1);
    assert!(r.is_empty());
}

#[test]
fn baseline_keys_are_unambiguous() {
    // ("pkg.alpha", ["os.path"]) and ("pkg.alphaos.path", []) differ: parts are
    // length-prefixed.
    let b = parse(r#"{"version": 1, "entries": [{"code": "PM405", "id": "pkg.alphaos.path"}]}"#);
    let c = ctx();
    let mut f = Findings::new();
    f.push(UNUSED_IMPORT, loc(1, 3, 1), SymbolId(1), 90, &[Arg::Name(2)]);
    let mut r = Report::new(&f, &c);
    assert_eq!(r.apply_baseline(&b), 0);
    let b = parse(r#"{"version": 1, "entries": [{"code": "PM405", "id": "pkg.alpha", "detail": ["os.path"]}]}"#);
    assert_eq!(r.apply_baseline(&b), 1);
}

#[test]
fn baseline_parse_accepts_variations() {
    // BOM, field order, unknown fields of every type, escapes (incl. a surrogate pair), missing
    // `count` (1) and `detail`, and duplicate entries (counts add up).
    let text = concat!(
        "\u{feff}{\"comment\": {\"nested\": [1, 2.5e3, -0.5, 1E-2, true, false, null, \"s\"]},\n",
        "  \"entries\": [\n",
        r#"    {"count": 3, "detail": ["PyYAML", "yaml"], "id": "pkg/café/😀.py", "code": "PM601", "x": {}},"#,
        "\n",
        r#"    {"code": "PM999", "id": "\"\\\/\b\f\n\r\t"},"#,
        "\n",
        r#"    {"code": "PM999", "id": "\"\\/\b\f\n\r\t", "count": 4}"#,
        "\n  ],\n  \"version\": 1\n}"
    );
    let b = parse(text);
    assert_eq!((b.len(), b.total()), (2, 8));
    let shown = b.to_string();
    assert!(
        shown.contains(r#"{"code": "PM601", "id": "pkg/café/😀.py", "detail": ["PyYAML", "yaml"], "count": 3}"#),
        "{shown}"
    );
    assert!(shown.contains(r#"{"code": "PM999", "id": "\"\\/\u0008\u000c\n\r\t", "count": 5}"#), "{shown}");
    let empty = parse(r#"{"version":1,"entries":[]}"#);
    assert!(empty.is_empty());
    assert_eq!(empty.to_string(), "{\n  \"version\": 1,\n  \"tool\": \"pymapper\",\n  \"entries\": []\n}\n");
}

#[test]
fn baseline_parse_errors() {
    let cases: &[(&str, &str)] = &[
        ("", "invalid JSON at byte 0: expected `{`"),
        ("[]", "invalid JSON at byte 0: expected `{`"),
        (r#"{"version": 2, "entries": []}"#, "unsupported baseline version 2 (this pymapper reads version 1)"),
        (r#"{"entries": []}"#, "not a pymapper baseline: missing `version`"),
        (r#"{"version": 1}"#, "not a pymapper baseline: missing `entries`"),
        (r#"{"version": 1, "entries": [{"id": "x"}]}"#, "not a pymapper baseline: entry without `code`"),
        (r#"{"version": 1, "entries": [{"code": "PM101"}]}"#, "not a pymapper baseline: entry without `id`"),
        (r#"{"version": 1, "entries": [{"code": "E501", "id": "x"}]}"#, "not a pymapper baseline: entry `code` is not PMxxx"),
        (r#"{"version": 1, "entries": [1]}"#, "invalid JSON at byte 27: expected `{`"),
        (r#"{"version": "1", "entries": []}"#, "invalid JSON at byte 12: expected a non-negative integer"),
        (r#"{"version": 1, "entries": [{"code": "PM101", "id": "x", "count": -1}]}"#, "invalid JSON at byte 65: expected a non-negative integer"),
        (r#"{"version": 1, "entries": [{"code": "PM101", "id": "x", "count": 1.5}]}"#, "invalid JSON at byte 66: expected an integer"),
        (r#"{"version": 1, "entries": [{"code": "PM101", "id": "x", "count": 4294967296}]}"#, "invalid JSON at byte 74: number out of range"),
        (r#"{"version": 1, "entries": []} x"#, "invalid JSON at byte 30: trailing characters"),
        (r#"{"version": 1, "entries": [],}"#, "invalid JSON at byte 29: expected a string"),
        (r#"{"version": 1 "entries": []}"#, "invalid JSON at byte 14: expected `,` or `}`"),
        (r#"{"version": 1, "entries": [{"code": "PM101", "id": "\ud800"}]}"#, "invalid JSON at byte 58: unpaired surrogate"),
        (r#"{"version": 1, "entries": [{"code": "PM101", "id": "\udc00"}]}"#, "invalid JSON at byte 58: unpaired surrogate"),
        (r#"{"version": 1, "entries": [{"code": "PM101", "id": "\x"}]}"#, "invalid JSON at byte 54: invalid escape"),
        (r#"{"version": 1, "entries": [{"code": "PM101", "id": "\u12"}]}"#, "invalid JSON at byte 54: invalid \\u escape"),
        ("{\"version\": 1, \"entries\": [{\"code\": \"PM101\", \"id\": \"a\nb\"}]}", "invalid JSON at byte 53: control character in string"),
        (r#"{"version": 1, "entries": [{"code": "PM101", "id": "x"#, "invalid JSON at byte 53: unterminated string"),
        (r#"{"version": 1, "x": tru, "entries": []}"#, "invalid JSON at byte 20: invalid literal"),
        (r#"{"version": 1, "x": 01, "entries": []}"#, "invalid JSON at byte 22: invalid number"),
        (r#"{"version": 1, "x": 1., "entries": []}"#, "invalid JSON at byte 22: invalid number"),
        (r#"{"version": 1, "x": -, "entries": []}"#, "invalid JSON at byte 21: invalid number"),
    ];
    let mut wrong = Vec::new();
    for &(text, want) in cases {
        match Baseline::parse(text.as_bytes()) {
            Ok(_) => wrong.push(format!("accepted {text}")),
            Err(e) if e.to_string() != want => wrong.push(format!("{text}\n  got:  {e}\n  want: {want}")),
            Err(_) => {}
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    // Deep nesting in skipped values is bounded, not a stack overflow.
    let deep = format!("{{\"x\": {}{}, \"version\": 1, \"entries\": []}}", "[".repeat(100_000), "]".repeat(100_000));
    let err = Baseline::parse(deep.as_bytes()).expect_err("too deep");
    assert!(err.to_string().ends_with("nesting too deep"), "{err}");
    let ok = format!("{{\"x\": {}{}, \"version\": 1, \"entries\": []}}", "[".repeat(100), "]".repeat(100));
    assert!(Baseline::parse(ok.as_bytes()).is_ok());
}

#[test]
fn baseline_tolerates_non_utf8_ids() {
    // A non-UTF-8 path is stored lossily and still matches on the next run.
    let c = ctx();
    let mut f = Findings::new();
    f.push(UNDECLARED_DEPENDENCY, loc(3, 2, 0), SymbolId::NONE, 100, &[Arg::Name(0), Arg::Name(1)]);
    let b = parse(&baseline_text(&f, &c));
    let mut r = Report::new(&f, &c);
    assert_eq!(r.apply_baseline(&b), 1);
}

#[test]
fn sarif_fingerprints_follow_baseline_keys() {
    let c = ctx();
    let mut a = Findings::new();
    a.push(COMPLEX_FUNCTION, loc(0, 12, 5), SymbolId(0), 100, &[Arg::Symbol(SymbolId(0)), Arg::U32(14), Arg::U32(10)]);
    let mut b = Findings::new();
    b.push(COMPLEX_FUNCTION, loc(0, 30, 1), SymbolId(0), 100, &[Arg::Symbol(SymbolId(0)), Arg::U32(20), Arg::U32(10)]);
    let mut d = Findings::new();
    d.push(COMPLEX_FUNCTION, loc(0, 30, 1), SymbolId(2), 100, &[Arg::Symbol(SymbolId(2)), Arg::U32(20), Arg::U32(10)]);
    let fp = |f: &Findings| {
        let s = render(f, &c, &Options::new(OutputFormat::Sarif), &Summary::default());
        let i = s.find("pymapperKey/v1").expect("fingerprint");
        s[i..i + 36].to_owned()
    };
    assert_eq!(fp(&a), fp(&b));
    assert_ne!(fp(&a), fp(&d));
}

#[test]
fn uri_encoding() {
    let mut out = Vec::new();
    super::sarif::push_uri(&mut out, b"a b/c%d#e?f/caf\xc3\xa9/\xff/x-y_z.~py");
    assert_eq!(out, b"a%20b/c%25d%23e%3Ff/caf%C3%A9/%FF/x-y_z.~py");
}

// ---------------------------------------------------------------------------------------------
// IndexContext over a real index
// ---------------------------------------------------------------------------------------------

#[test]
fn index_context_renders_paths_ids_and_names() {
    use crate::config::{Contract, ContractKind};
    use crate::index::{Inputs, load};
    use crate::roots::Origin;
    use crate::walk::{WalkOptions, walk};

    let dir = std::env::temp_dir().join(format!("pymapper-output-ctx-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("pkg")).expect("mkdir");
    std::fs::write(dir.join("pkg/__init__.py"), "").expect("write");
    std::fs::write(dir.join("pkg/mod.py"), "import os\n\nclass C:\n    def run(self):\n        pass\n").expect("write");
    let w = walk(&[&dir], &WalkOptions::everything());
    let ix = load(Inputs::from_root_dir(&dir, &w.files, Origin::FirstParty, b"proj"));
    let run = ix.find(b"pkg.mod:C.run").expect("method node");
    let file = ix.file(run).expect("file");
    let os = ix.strings().get(b"os").expect("interned").to_raw();
    let contracts = [Contract {
        name: "layers".to_owned(),
        kind: ContractKind::Independence { modules: Vec::new() },
        allow_type_checking: false,
        transitive: false,
        ignore_imports: Vec::new(),
    }];

    let mut f = Findings::new();
    f.push(UNUSED_METHOD, Location::new(FileId(file), 4, 5), SymbolId(run), 90, &[Arg::Symbol(SymbolId(run)), Arg::Phrase(0)]);
    f.push(FORBIDDEN_IMPORT, Location::new(FileId(file), 1, 1), SymbolId(run), 100, &[
        Arg::Symbol(SymbolId(run)),
        Arg::Name(os),
        Arg::Contract(0),
    ]);
    // Unknown ids render as `?` instead of panicking.
    f.push(FORBIDDEN_IMPORT, Location::new(FileId(file), 2, 1), SymbolId(run), 100, &[
        Arg::Symbol(SymbolId(u32::MAX - 1)),
        Arg::Name(0),
        Arg::Contract(7),
    ]);
    let ctx = IndexContext::new(&ix, &dir, &contracts);
    let opts = Options { footer: false, ..Options::new(OutputFormat::Concise) };
    let render_ctx = |c: &IndexContext<'_>| {
        let r = Report::new(&f, c);
        let mut out = Vec::new();
        r.write(&opts, &Summary::default(), &mut out).expect("write");
        String::from_utf8(out).expect("UTF-8")
    };
    assert_eq!(
        render_ctx(&ctx),
        "pkg/mod.py:1:1: PM303 `pkg.mod:C.run` imports forbidden module `os` (contract `layers`)\n\
         pkg/mod.py:2:1: PM303 `?` imports forbidden module `?` (contract `?`)\n\
         pkg/mod.py:4:5: PM403 method `pkg.mod:C.run` is never used (confidence 90)\n"
    );
    // A root with a trailing slash strips the same way.
    let slash = std::path::PathBuf::from(format!("{}/", dir.display()));
    assert!(render_ctx(&IndexContext::new(&ix, &slash, &contracts)).starts_with("pkg/mod.py:1:1: "));
    // Files outside the root keep their full path.
    let other = dir.join("elsewhere");
    let full = render_ctx(&IndexContext::new(&ix, &other, &contracts));
    assert!(full.starts_with(&format!("{}/pkg/mod.py:1:1: ", dir.display())), "{full}");
    // A directory whose name is a prefix of the file's directory is not an ancestor.
    let prefix = std::path::PathBuf::from(format!("{}/pk", dir.display()));
    assert!(render_ctx(&IndexContext::new(&ix, &prefix, &contracts)).starts_with(&format!("{}/pkg/", dir.display())));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn json_escaping() {
    let mut out = Vec::new();
    json::push_str(&mut out, b"a\"b\\c\n\r\t\x01\x1f\x7f \xc3\xa9 \xff end");
    assert_eq!(String::from_utf8(out).expect("UTF-8"), r#""a\"b\\c\n\r\t\u0001\u001f\u007f é \ufffd end""#);
}

#[test]
fn baselines_with_superseded_ids_still_load() {
    // A baseline saved before overload implementations lost their `#N` suffix (and before
    // PM301 messages carried the cycle size): the old PM101 id no longer matches, so that
    // finding is new again; everything else stays baselined.
    let text = r#"{"version": 1, "tool": "pymapper", "entries": [
        {"code": "PM101", "id": "pkg.zeta:handle#4", "count": 1},
        {"code": "PM301", "id": "", "detail": ["pkg.alpha", "pkg.zeta", "pkg.alpha"], "count": 1},
        {"code": "PM405", "id": "pkg.alpha", "detail": ["yaml"], "count": 1}
    ]}"#;
    let b = parse(text);
    let f = sample();
    let c = ctx();
    let mut r = Report::new(&f, &c);
    assert_eq!(r.apply_baseline(&b), 2);
    let left: Vec<u16> = r.iter().map(|x| x.rule.code()).collect();
    assert!(left.contains(&101) && !left.contains(&301), "{left:?}");
    assert_eq!(left.iter().filter(|&&c| c == 405).count(), 1, "only `yaml` was baselined");
}

#[test]
fn json_end_line_and_unused_import_symbol() {
    let f = sample();
    let c = ctx();
    let got = render(&f, &c, &Options::new(OutputFormat::Json), &summary());
    assert_json(&got);
    // Derived from the definition when the analysis gave none; the analysis' value wins.
    assert!(got.contains("\"line\":12,\"col\":5,\"end_line\":30,\"symbol\":\"pkg.zeta:handle\""), "{got}");
    assert!(got.contains("\"line\":14,\"col\":9,\"end_line\":21,"), "{got}");
    assert!(got.contains("\"symbol\":\"pkg.alpha:yaml\""), "{got}");
    // Reals are numbers with at most two decimals.
    assert!(got.contains("\"value\":12.25,\"threshold\":20}"), "{got}");
}

#[test]
fn dead_code_reasons_are_rendered() {
    use crate::rules::dead_code::reason;
    let mut f = Findings::new();
    let args = [Arg::Symbol(SymbolId(0)), Arg::Phrase(3)];
    f.push_with_reasons(UNUSED_FUNCTION, loc(0, 4, 5), SymbolId(0), 50, &args, reason::NAME_ELSEWHERE | reason::ONLY_USED_BY_UNUSED_CODE);
    f.push_with_reasons(UNUSED_FUNCTION, loc(0, 9, 5), SymbolId(0), 100, &[Arg::Symbol(SymbolId(0)), Arg::Phrase(0)], reason::NO_REFERENCES);
    f.push(COMPLEX_FUNCTION, loc(0, 12, 5), SymbolId(0), 100, &[Arg::Symbol(SymbolId(0)), Arg::U32(14), Arg::U32(10)]);
    let sum = Summary::default();
    let text = render(&f, &ctx(), &Options { footer: false, ..Options::new(OutputFormat::Concise) }, &sum);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "pkg/zeta.py:4:5: PM401 function `pkg.zeta:handle` has no resolved callers (name appears elsewhere) (confidence 50: only-used-by-unused-code, name-elsewhere)");
    // Full confidence: no parenthesis, as before.
    assert_eq!(lines[1], "pkg/zeta.py:9:5: PM401 function `pkg.zeta:handle` is never used");
    let json = render(&f, &ctx(), &Options::new(OutputFormat::Json), &sum);
    assert_json(&json);
    assert!(json.contains(r#""reasons":["only-used-by-unused-code","name-elsewhere"]}"#), "{json}");
    assert!(json.contains(r#""reasons":["no-references"]}"#), "{json}");
    // Findings without reasons have no `reasons` key.
    assert_eq!(json.matches("\"reasons\"").count(), 2, "{json}");
}

// ---------------------------------------------------------------------------------------------
// Finding ids, display paths, plain messages
// ---------------------------------------------------------------------------------------------

#[test]
fn stable_hash_is_fnv1a() {
    // Published FNV-1a 64 test vectors: ids must never change.
    assert_eq!(ids::stable_hash(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(ids::stable_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(ids::stable_hash(b"foobar"), 0x8594_4171_f739_67e8);
}

#[test]
fn ids_are_deterministic_and_unique_per_rule() {
    let f = sample();
    let c = ctx();
    let r = Report::new(&f, &c);
    let a = r.ids();
    let b = Report::new(&f, &c).ids();
    assert_eq!(a.len(), r.len());
    let mut seen = std::collections::HashSet::new();
    for (pos, finding) in r.iter().enumerate() {
        let id = a.get(pos);
        assert_eq!(id, b.get(pos));
        assert_eq!(id.code, finding.rule.code());
        assert!(id.digits >= ids::MIN_ID_DIGITS);
        let text = id.to_string();
        assert!(text.starts_with(&format!("{}-", finding.rule)), "{text}");
        assert!(seen.insert(text.clone()), "duplicate id {text}");
        assert_eq!(a.find(&text), Ok(pos));
    }
}

#[test]
fn ids_ignore_lines_and_numbers() {
    let s = SymbolId;
    let mut one = Findings::new();
    one.push(COMPLEX_FUNCTION, loc(0, 12, 5), s(0), 100, &[Arg::Symbol(s(0)), Arg::U32(14), Arg::U32(10)]);
    let mut moved = Findings::new();
    moved.push(COMPLEX_FUNCTION, loc(0, 40, 1), s(0), 100, &[Arg::Symbol(s(0)), Arg::U32(19), Arg::U32(10)]);
    let c = ctx();
    let a = Report::new(&one, &c).ids().get(0);
    let b = Report::new(&moved, &c).ids().get(0);
    assert_eq!(a, b);
}

#[test]
fn equal_keys_get_distinct_ids_and_the_first_keeps_the_plain_one() {
    let s = SymbolId;
    let mut f = Findings::new();
    f.push(UNUSED_IMPORT, loc(1, 3, 1), s(1), 100, &[Arg::Name(0)]);
    let c = ctx();
    let single = Report::new(&f, &c).ids().get(0);
    f.push(UNUSED_IMPORT, loc(1, 4, 1), s(1), 100, &[Arg::Name(0)]);
    let r = Report::new(&f, &c);
    let both = r.ids();
    assert_eq!(both.get(0).hash, single.hash);
    assert_ne!(both.get(0).hash, both.get(1).hash);
}

#[test]
fn colliding_ids_are_extended() {
    // Fabricate the collision check directly: two hashes sharing 5 hex digits need 6.
    let a = FindingId { code: 401, hash: 0x3fa2_1000_0000_0000, digits: 6 };
    let mut out = Vec::new();
    a.push(&mut out);
    assert_eq!(out, b"PM401-3fa210");
    let ids = FindingIds::from_hashes(&[(401, 0x3fa2_1000_0000_0000), (401, 0x3fa2_1fff_0000_0000), (401, 0x1234_0000_0000_0000), (402, 0x3fa2_1000_0000_0000)]);
    assert_eq!(ids.get(0).to_string(), "PM401-3fa210");
    assert_eq!(ids.get(1).to_string(), "PM401-3fa21f");
    assert_eq!(ids.get(2).to_string(), "PM401-1234");
    assert_eq!(ids.get(3).to_string(), "PM402-3fa2");
    assert_eq!(ids.find("PM401-1234"), Ok(2));
    assert_eq!(ids.find("pm401-3FA21F"), Ok(1));
    assert_eq!(ids.find("PM401-3fa2"), Err(FindError::Ambiguous));
    assert_eq!(ids.find("PM401-9999"), Err(FindError::NotFound));
    for bad in ["PM401", "PM41-3fa2", "PM401-3fa", "PM401-xyzw", "X401-3fa2", "PM401-0123456789abcdef0"] {
        assert_eq!(ids.find(bad), Err(FindError::Malformed), "{bad}");
    }
}

#[test]
fn relative_paths() {
    let rel = |p: &str, base: &str| {
        let mut out = Vec::new();
        push_relative(p.as_bytes(), base.as_bytes(), &mut out);
        String::from_utf8(out).expect("utf-8")
    };
    assert_eq!(rel("/r/proj/pkg/a.py", "/r/proj"), "pkg/a.py");
    assert_eq!(rel("/r/proj/pkg/a.py", "/r"), "proj/pkg/a.py");
    assert_eq!(rel("/r/proj/pkg/a.py", "/r/proj/pkg/sub"), "../a.py");
    assert_eq!(rel("/r/proj/a.py", "/r/other/x"), "../../proj/a.py");
    assert_eq!(rel("/r/proj", "/r/proj"), ".");
    assert_eq!(rel("/r/projx/a.py", "/r/proj"), "../projx/a.py");
    assert_eq!(rel("/opt/a.py", "/r/proj"), "/opt/a.py");
    assert_eq!(rel("/r/a.py", "/"), "/r/a.py");
}

#[test]
fn plain_messages_and_short_symbols() {
    let f = sample();
    let c = ctx();
    let r = Report::new(&f, &c);
    let (mut msg, mut scratch, mut sym) = (String::new(), Vec::new(), Vec::new());
    let complex = r.iter().find(|x| x.rule == COMPLEX_FUNCTION).expect("PM101");
    r.message_plain(complex, &mut msg, &mut scratch);
    assert_eq!(msg, "handle is too complex (McCabe 14 > 10)");
    r.short_symbol(complex, &mut sym);
    assert_eq!(sym, b"handle");
    let method = r.iter().find(|x| x.rule == LOW_MAINTAINABILITY).expect("PM106");
    r.short_symbol(method, &mut sym);
    assert_eq!(sym, b"Big.run");
    let module = r.iter().find(|x| x.rule == LONG_MODULE).expect("PM107");
    r.short_symbol(module, &mut sym);
    assert_eq!(sym, b"pkg");
}
