use std::path::{Path, PathBuf};

use super::norm::Normaliser;
use super::winnow::{for_each_token, winnow};
use super::*;
use crate::lex::TokenKind;

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Normalised codes of `src` (lexer stream).
fn codes(src: &str) -> Vec<u8> {
    let src = src.as_bytes();
    let mut norm = Normaliser::default();
    let mut out = Vec::new();
    for_each_token(src, |t| {
        if let Some(c) = norm.push(src, t) {
            out.push(c);
        }
    });
    out
}

fn kinds(ks: &[TokenKind]) -> Vec<u8> {
    ks.iter().map(|&k| super::norm::code(k)).collect()
}

/// Fingerprints `srcs` (file i = srcs[i]) with the lexer and matches them from memory.
fn run(srcs: &[&str], params: DupesParams) -> DupesReport {
    let mut fps = Fingerprints::new();
    let mut sc = FingerprintScratch::new();
    for (i, s) in srcs.iter().enumerate() {
        fingerprint_source(s.as_bytes(), i as u32, &params, &mut sc, &mut fps);
    }
    find_clones(fps, &params, |f, buf| {
        buf.extend_from_slice(srcs[f as usize].as_bytes());
        true
    })
}

/// Fingerprints of `src` through the parser (the loader's path).
fn parser_fps(src: &[u8], file: u32, params: &DupesParams) -> Fingerprints {
    let mut sc = FingerprintScratch::new();
    let mut out = Fingerprints::new();
    let v = crate::extract::parse(src, FingerprintVisitor::new(src, &mut sc));
    v.finish(file, params, &mut out);
    out
}

fn lexer_fps(src: &[u8], file: u32, params: &DupesParams) -> Fingerprints {
    let mut sc = FingerprintScratch::new();
    let mut out = Fingerprints::new();
    fingerprint_source(src, file, params, &mut sc, &mut out);
    out
}

/// A function of 12 lines and 76 tokens; `n` renames its identifiers and literals.
fn func(name: &str, n: &str) -> String {
    format!(
        "def {name}(items{n}, limit{n}=10):
    total{n} = 0
    seen{n} = set()
    for item{n} in items{n}:
        if item{n}.key in seen{n}:
            continue
        seen{n}.add(item{n}.key)
        total{n} += item{n}.value * {n}2
        if total{n} > limit{n}:
            raise ValueError(\"too much {n}\", total{n})
    result{n} = [x for x in seen{n} if x]
    return total{n}, sorted(result{n})
"
    )
}

/// A class of 10 lines and 88 tokens, unlike [`func`].
const REPO: &str = "class Repo:
    def save(self, obj, *, force=False):
        with self.lock:
            if obj.id is None or force:
                obj.id = self.next_id()
                self.items[obj.id] = obj
            self.log.append((\"save\", obj.id, len(self.items)))
            for hook in self.hooks:
                hook(obj, self)
        return obj.id
";

/// Deterministic xorshift for property tests.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

// ---------------------------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------------------------

#[test]
fn params_derive_k_and_w_from_min_tokens() {
    let d = DupesParams::default();
    assert_eq!((d.min_tokens, d.min_lines), (50, 6));
    assert_eq!((d.k(), d.w()), (20, 31));
    assert_eq!(d.k() + d.w() - 1, d.min_tokens);
    for t in 2..200 {
        let p = DupesParams::new(t, 1);
        assert!(p.k() >= 1 && p.k() <= 20 && p.w() >= 1, "{t}");
        assert_eq!(p.k() + p.w() - 1, t, "{t}");
    }
    assert_eq!(DupesParams::new(0, 0).min_tokens, 2);

    let mut cfg = Config::new(Path::new("/p"));
    assert_eq!(DupesParams::from_config(&cfg), None);
    cfg.analysis.dupes = true;
    cfg.thresholds.dupes_min_tokens = 70;
    cfg.thresholds.dupes_min_lines = 3;
    assert_eq!(DupesParams::from_config(&cfg), Some(DupesParams::new(70, 3)));
}

// ---------------------------------------------------------------------------------------------
// Normalisation
// ---------------------------------------------------------------------------------------------

#[test]
fn names_and_literals_collapse() {
    use TokenKind as K;
    assert_eq!(codes("foo = bar(1, 'x')"), kinds(&[K::Name, K::Equal, K::Name, K::LPar, K::Int, K::Comma, K::String, K::RPar]));
    assert_eq!(codes("a = 1\n"), codes("bbb = 2.5j\n"));
    assert_eq!(codes("x = 'a'\n"), codes("y = b\"\"\"zz\"\"\"\n"));
    assert_eq!(codes("x = f'{a}b'\n"), codes("y = t\"{c}d\"\n"));
    assert_ne!(codes("x = a + b\n"), codes("x = a - b\n"));
    assert_ne!(codes("if a: pass\n"), codes("while a: pass\n"));
    // Soft keywords are names.
    assert_eq!(codes("match = 1\n"), codes("x = 1\n"));
}

#[test]
fn import_statements_are_dropped() {
    let body = codes("x = 1\n");
    for src in [
        "import os\nx = 1\n",
        "import os.path as p, sys\nx = 1\n",
        "from . import a\nx = 1\n",
        "from ..m import (\n    a,\n    b,\n)\nx = 1\n",
        "from m import a, \\\n    b\nx = 1\n",
        "import os; x = 1\n",
        "x = 1\nimport os  # comment\n",
        "from __future__ import annotations\n\n\n# c\nimport a\nx = 1\n",
    ] {
        assert_eq!(codes(src), body, "{src:?}");
    }
    // Nested in blocks, after a colon on the same line.
    assert_eq!(codes("if t:\n    import a\n    x = 1\n"), codes("if t:\n    x = 1\n"));
    assert_eq!(codes("if t: import a\nx = 1\n"), codes("if t:\nx = 1\n"));
    assert_eq!(codes("try:\n    import a\nexcept ImportError:\n    a = None\n"), codes("try:\nexcept ImportError:\n    a = None\n"));
}

#[test]
fn from_inside_expressions_is_kept() {
    use TokenKind as K;
    assert_eq!(codes("def g():\n    yield from x\n")[5..], kinds(&[K::Yield, K::From, K::Name]));
    assert_eq!(codes("raise E from e\n"), kinds(&[K::Raise, K::Name, K::From, K::Name]));
    assert_eq!(codes("raise E \\\n    from e\n"), kinds(&[K::Raise, K::Name, K::From, K::Name]));
    // The statement after an import is kept, even on a continued logical line inside brackets.
    assert_eq!(codes("import a\nf(\n  from_)\n"), kinds(&[K::Name, K::LPar, K::Name, K::RPar]));
}

#[test]
fn exact_hash_distinguishes_spelling() {
    let src = b"abc abd abc 1 1";
    let toks: Vec<_> = {
        let mut v = Vec::new();
        for_each_token(src, |t| v.push(t));
        v
    };
    let e: Vec<u32> = toks.iter().map(|&t| super::norm::exact(src, t)).collect();
    assert_eq!(e[0], e[2]);
    assert_ne!(e[0], e[1]);
    assert_eq!(e[3], e[4]);
}

// ---------------------------------------------------------------------------------------------
// Winnowing
// ---------------------------------------------------------------------------------------------

fn winnow_codes(codes: &[u8], params: &DupesParams, file: u32) -> Fingerprints {
    let mut out = Fingerprints::new();
    winnow(codes, params, &mut Vec::new(), &mut Vec::new(), file, &mut out);
    out
}

/// Reference winnowing: hash every k-gram from scratch, take the rightmost minimum of every
/// window, record it when it changes.
fn winnow_naive(codes: &[u8], params: &DupesParams) -> Vec<(u32, u32)> {
    let (k, w) = (params.k() as usize, params.w() as usize);
    if codes.len() < k + w - 1 {
        return Vec::new();
    }
    let hs: Vec<u64> = (0..=codes.len() - k)
        .map(|i| {
            let h = codes[i..i + k].iter().fold(0u64, |h, &c| h.wrapping_mul(super::winnow::BASE).wrapping_add(u64::from(c) + 1));
            super::winnow::mix(h)
        })
        .collect();
    let mut fps = Vec::new();
    let mut prev = usize::MAX;
    for end in w - 1..hs.len() {
        let mut m = end;
        for j in (end + 1 - w..=end).rev() {
            if hs[j] < hs[m] {
                m = j;
            }
        }
        if m != prev {
            prev = m;
            fps.push((hs[m] as u32, m as u32));
        }
    }
    fps
}

#[test]
fn winnowing_matches_the_reference() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for params in [DupesParams::new(50, 1), DupesParams::new(10, 1), DupesParams::new(2, 1), DupesParams::new(41, 1)] {
        for len in [0usize, 1, 5, 30, 49, 50, 51, 200, 3000] {
            // Small alphabets give many equal hashes (ties) and repeats.
            for alpha in [2u64, 7, 90] {
                let codes: Vec<u8> = (0..len).map(|_| (rng.next() % alpha) as u8).collect();
                let got: Vec<(u32, u32)> = winnow_codes(&codes, &params, 3).iter().map(|f| (f.hash, f.pos)).collect();
                assert_eq!(got, winnow_naive(&codes, &params), "{params:?} len {len} alpha {alpha}");
            }
        }
    }
}

#[test]
fn shared_runs_of_min_tokens_share_a_fingerprint() {
    let params = DupesParams::default();
    let t = params.min_tokens as usize;
    let mut rng = Rng(42);
    for trial in 0..200 {
        let shared: Vec<u8> = (0..t + (trial % 7)).map(|_| (rng.next() % 60) as u8).collect();
        let mut a: Vec<u8> = (0..rng.next() % 300).map(|_| (rng.next() % 60) as u8).collect();
        let mut b: Vec<u8> = (0..rng.next() % 300).map(|_| (rng.next() % 60) as u8).collect();
        let (pa, pb) = (a.len(), b.len());
        a.extend_from_slice(&shared);
        b.extend_from_slice(&shared);
        a.extend((0..rng.next() % 100).map(|_| (rng.next() % 60) as u8));
        let fa = winnow_codes(&a, &params, 0);
        let fb = winnow_codes(&b, &params, 1);
        let common = fa.iter().any(|x| {
            let off = x.pos as usize;
            off >= pa
                && off + params.k() as usize <= pa + shared.len()
                && fb.iter().any(|y| y.hash == x.hash && y.pos as usize + pa == off + pb)
        });
        assert!(common, "trial {trial}");
    }
}

#[test]
fn density_is_about_two_over_w_plus_one() {
    let params = DupesParams::default();
    let mut rng = Rng(7);
    let codes: Vec<u8> = (0..200_000).map(|_| (rng.next() % 90) as u8).collect();
    let n = winnow_codes(&codes, &params, 0).len() as f64;
    let expected = codes.len() as f64 * 2.0 / f64::from(params.w() + 1);
    assert!((n / expected - 1.0).abs() < 0.1, "{n} vs {expected}");
}

/// Every `.py` file under `root`, in walk order.
fn walk_py(root: &Path) -> Vec<PathBuf> {
    let w = crate::walk::walk(&[root], &crate::walk::WalkOptions::everything());
    (0..w.files.len()).map(|i| w.files.get(i).to_path_buf()).collect()
}

#[test]
fn parser_and_lexer_streams_agree_on_the_corpus() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    let params = DupesParams::new(10, 1);
    let (mut files, mut nonempty) = (0, 0);
    for p in walk_py(&root) {
        let src = std::fs::read(&p).expect("read");
        let a = parser_fps(&src, 0, &params);
        let b = lexer_fps(&src, 0, &params);
        assert_eq!(a.as_slice(), b.as_slice(), "{}", p.display());
        files += 1;
        nonempty += usize::from(!a.is_empty());
    }
    assert!(files > 10 && nonempty > 10, "{files} {nonempty}");
}

// ---------------------------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------------------------

/// Lines of `src` up to (excluding) the first occurrence of `needle`, plus one: the 1-based
/// line where `needle` starts.
fn line_of(src: &str, needle: &str) -> u32 {
    let at = src.find(needle).expect("needle");
    src[..at].matches('\n').count() as u32 + 1
}

#[test]
fn renamed_copy_in_another_file() {
    let a = format!("x = 1\n\n{}\ny = 2\n", func("alpha", "a"));
    let b = format!("import os\n\nclass K:\n    pass\n\n\n{}", func("beta", "b"));
    let r = run(&[&a, &b], DupesParams::default());
    assert_eq!(r.pairs.len(), 1, "{r:?}");
    let p = r.pairs[0];
    assert_eq!((p.a.file, p.b.file), (0, 1));
    assert_eq!(p.a.start_line, line_of(&a, "def alpha"));
    assert_eq!(p.b.start_line, line_of(&b, "def beta"));
    assert_eq!(p.a.col, 1);
    assert_eq!(p.a.lines(), 12);
    assert_eq!(p.b.lines(), 12);
    assert_eq!(p.a.tokens(), p.b.tokens());
    // Renamed identifiers and literals: similar but not identical.
    assert!(p.similarity > 30 && p.similarity < 100, "{}", p.similarity);
    // One group of two members pointing at each other.
    assert_eq!(r.members.len(), 2);
    assert_eq!(r.groups.len(), 1);
    assert_eq!(r.group(0), &[0, 1]);
    assert_eq!(r.best[0].map(|b| b.other), Some(p.b));
    assert_eq!(r.best[1].map(|b| b.other), Some(p.a));
    // Each file read once to re-lex it and once for line numbers.
    assert_eq!(r.stats.files_read, 4);
    assert_eq!(r.stats.bytes_read, 2 * (a.len() + b.len()) as u64);
    assert!(r.stats.fingerprints > 0 && r.stats.seeds > 0 && r.stats.extended > 0);
}

#[test]
fn exact_copy_is_fully_similar() {
    let f = func("same", "s");
    let a = format!("{f}\nz = 3\n");
    let b = format!("w = 0\n{f}");
    let r = run(&[&a, &b], DupesParams::default());
    assert_eq!(r.pairs.len(), 1);
    assert_eq!(r.pairs[0].similarity, 100);
    assert_eq!(r.pairs[0].b.start_line, 2);
}

#[test]
fn small_or_unrelated_code_is_not_reported() {
    let a = func("alpha", "a");
    let b = "def other(a, b):\n    return {k: v for k, v in zip(a, b) if k}\n\nclass C(Base):\n    x: int = 3\n";
    assert!(run(&[&a, b], DupesParams::default()).pairs.is_empty());
    // The same function under a stricter token threshold.
    let b = func("beta", "b");
    assert_eq!(codes(&a).len(), 76);
    assert_eq!(run(&[&a, &b], DupesParams::new(76, 6)).pairs.len(), 1);
    assert!(run(&[&a, &b], DupesParams::new(77, 6)).pairs.is_empty());
    // ... and line threshold (the copies span 12 lines).
    assert_eq!(run(&[&a, &b], DupesParams::new(50, 12)).pairs.len(), 1);
    assert!(run(&[&a, &b], DupesParams::new(50, 13)).pairs.is_empty());
    // Joined onto few lines: too few lines.
    let one_line: String = a.lines().map(|l| l.trim()).collect::<Vec<_>>().join("; ");
    let one_line = format!("[{}]\n", one_line.replace(':', "").replace("def", "d"));
    assert!(run(&[&one_line, &one_line.replace('a', "b")], DupesParams::default()).pairs.is_empty());
}

#[test]
fn import_blocks_are_not_clones() {
    let imports: String = (0..40).map(|i| format!("from pkg.mod{i} import name{i}, other{i}\n")).collect();
    let a = format!("{imports}x = 1\n");
    let b = format!("{imports}y = 2\n");
    assert!(run(&[&a, &b], DupesParams::default()).pairs.is_empty());
}

#[test]
fn clone_within_one_file() {
    let src = format!("{}\n\nq = 0\n\n{}", func("first", "x"), func("second", "y"));
    let r = run(&[&src], DupesParams::default());
    assert_eq!(r.pairs.len(), 1, "{r:?}");
    let p = r.pairs[0];
    assert_eq!((p.a.file, p.b.file), (0, 0));
    assert_eq!(p.a.start_line, 1);
    assert_eq!(p.b.start_line, line_of(&src, "def second"));
    assert!(p.a.end_tok <= p.b.start_tok);
    assert_eq!(r.stats.files_read, 2);
}

#[test]
fn three_copies_form_one_group() {
    let srcs: Vec<String> = ["a", "b", "c"].iter().map(|n| format!("v = 0\n{}", func(n, n))).collect();
    let refs: Vec<&str> = srcs.iter().map(String::as_str).collect();
    let r = run(&refs, DupesParams::default());
    assert_eq!(r.pairs.len(), 3, "{r:?}");
    assert_eq!(r.members.len(), 3);
    assert_eq!(r.groups.len(), 1);
    assert_eq!(r.group(0), &[0, 1, 2]);
    assert!(r.best.iter().all(Option::is_some));
    assert_eq!(r.group, vec![0, 0, 0]);
}

#[test]
fn many_copies_stay_linear_and_grouped() {
    // More copies than ALL_PAIRS_MAX: buckets seed a star, everything still lands in one group.
    let srcs: Vec<String> = (0..20).map(|i| format!("v{i} = {i}\n{}", func(&format!("f{i}"), &format!("n{i}")))).collect();
    let refs: Vec<&str> = srcs.iter().map(String::as_str).collect();
    let r = run(&refs, DupesParams::default());
    assert_eq!(r.members.len(), 20);
    assert_eq!(r.groups.len(), 1);
    assert!(r.best.iter().all(Option::is_some));
    assert!(r.pairs.len() >= 19 && r.pairs.len() <= 20 * 19 / 2, "{}", r.pairs.len());
}

#[test]
fn two_separate_groups() {
    let g1 = func("one", "o");
    let g2 = REPO;
    let a = format!("{g1}\n{g2}");
    let b = format!("{g2}\n\n{g1}");
    let r = run(&[&a, &b], DupesParams::new(40, 6));
    assert_eq!(r.pairs.len(), 2, "{r:?}");
    assert_eq!(r.groups.len(), 2);
    assert_eq!(r.members.len(), 4);
}

#[test]
fn small_edits_are_bridged() {
    // Two tokens inserted in the middle of a 164-token run: both halves are seeded and the
    // gap is bridged.
    assert_eq!(codes(REPO).len(), 88);
    let a = format!("{}{REPO}", func("alpha", "a"));
    let b = a.replace("sorted(resulta)", "sorted(resulta), 1");
    assert_eq!(codes(&b).len(), codes(&a).len() + 2);
    let r = run(&[&a, &b], DupesParams::default());
    assert_eq!(r.pairs.len(), 1, "{r:?}");
    let p = r.pairs[0];
    assert_eq!((p.a.start_line, p.a.lines()), (1, 22));
    assert_eq!((p.b.start_line, p.b.lines()), (1, 22));
    assert_eq!((p.a.tokens(), p.b.tokens()), (164, 166));
    assert_eq!(p.similarity, (164 * 100 / 166) as u8);
    // A larger change is not bridged: two separate clones, or none when the halves are short.
    let b = a.replace("sorted(resulta)", "sorted(resulta), 1, 2, 3, 4, 5, 6");
    let r = run(&[&a, &b], DupesParams::default());
    assert_eq!(r.pairs.len(), 2, "{r:?}");
}

#[test]
fn repetitive_runs_need_an_exact_copy() {
    let assigns = |pfx: &str| -> String {
        let body: String = (0..30).map(|i| format!("        self.{pfx}{i} = {pfx}{i}\n")).collect();
        format!("class C:\n    def __init__(self):\n{body}")
    };
    let (a, b) = (assigns("a"), assigns("b"));
    assert!(run(&[&a, &b], DupesParams::default()).pairs.is_empty());
    let r = run(&[&a, &a], DupesParams::default());
    assert_eq!(r.pairs.len(), 1);
    assert_eq!(r.pairs[0].similarity, 100);
}

#[test]
fn data_tables_need_an_exact_copy() {
    // Rows of varying shape (not periodic), no names: data, not code.
    let table = |seed: u64| -> String {
        let mut rng = Rng(seed);
        let rows: String = (0..40)
            .map(|i| match rng.next() % 3 {
                0 => format!("    ({i}, 'M', 'x{i}'),\n"),
                1 => format!("    ({i}, 'V'),\n"),
                _ => format!("    ({i}, 'X', {i}, 1.5),\n"),
            })
            .collect();
        format!("TABLE = [\n{rows}]\n")
    };
    let (a, b) = (table(1), table(1).replace('x', "y").replace("1.5", "2.5"));
    assert_eq!(codes(&a), codes(&b));
    assert!(run(&[&a, &b], DupesParams::default()).pairs.is_empty());
    let r = run(&[&a, &a], DupesParams::default());
    assert_eq!(r.pairs.len(), 1);
    assert_eq!(r.pairs[0].similarity, 100);
}

#[test]
fn unreadable_and_changed_files_are_skipped() {
    let a = func("alpha", "a");
    let b = func("beta", "b");
    let params = DupesParams::default();
    let mut fps = Fingerprints::new();
    let mut sc = FingerprintScratch::new();
    fingerprint_source(a.as_bytes(), 0, &params, &mut sc, &mut fps);
    fingerprint_source(b.as_bytes(), 1, &params, &mut sc, &mut fps);
    let r = find_clones(fps.clone(), &params, |f, buf| {
        if f == 0 {
            buf.extend_from_slice(a.as_bytes());
        }
        f == 0
    });
    assert!(r.pairs.is_empty());
    // File 1 changed on disk since extraction: seeds no longer verify.
    let r = find_clones(fps, &params, |f, buf| {
        buf.extend_from_slice(if f == 0 { a.as_bytes() } else { b"x = 1\n" });
        true
    });
    assert!(r.pairs.is_empty());
    assert!(run(&[], params).pairs.is_empty());
    assert!(run(&[""], params).pairs.is_empty());
}

#[test]
fn findings_point_at_the_other_copy() {
    let a = format!("x = 1\n\n{}", func("alpha", "a"));
    let b = func("beta", "b");
    let r = run(&[&a, &b], DupesParams::default());
    let mut out = Findings::new();
    findings(&r, &mut out);
    assert_eq!(out.len(), 2);
    let all: Vec<(Location, u8, Vec<Arg>)> = out.iter().map(|(f, args)| (f.loc, f.confidence, args.to_vec())).collect();
    let sim = r.pairs[0].similarity;
    assert_eq!(all[0], (Location::new(FileId(0), 3, 1), sim, vec![Arg::U32(12), Arg::File(FileId(1)), Arg::U32(1)]));
    assert_eq!(all[1], (Location::new(FileId(1), 1, 1), sim, vec![Arg::U32(12), Arg::File(FileId(0)), Arg::U32(3)]));
    for (f, args) in out.iter() {
        assert_eq!(f.rule, crate::rules::codes::DUPLICATE_CODE);
        assert!(f.rule.def().accepts(args));
    }
}

#[test]
fn deterministic_across_thread_counts() {
    let srcs: Vec<String> = (0..30)
        .map(|i| match i % 3 {
            0 => format!("a{i} = 1\n{}", func(&format!("f{i}"), "p")),
            1 => format!("{}\nb = [{i}]\n", func("g", &format!("q{i}"))),
            _ => format!("def h{i}():\n    return {i}\n"),
        })
        .collect();
    let refs: Vec<&str> = srcs.iter().map(String::as_str).collect();
    let dump = |threads: usize| {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().expect("pool");
        let r = pool.install(|| run(&refs, DupesParams::default()));
        format!("{:?}|{:?}|{:?}|{:?}", r.pairs, r.members, r.best, r.group)
    };
    let one = dump(1);
    assert!(one.len() > 100);
    assert_eq!(one, dump(4));
    assert_eq!(one, dump(9));
}

// ---------------------------------------------------------------------------------------------
// Loader integration
// ---------------------------------------------------------------------------------------------

/// Writes `files` (relative path, contents) into a fresh temp dir and builds inputs: files
/// under `dep/` in a third-party group, the rest first-party.
fn temp_inputs(tag: &str, files: &[(&str, String)]) -> (PathBuf, crate::index::Inputs) {
    use crate::roots::{FileStatus, Origin};
    let dir = std::env::temp_dir().join(format!("pymapper-dupes-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut inputs = crate::index::Inputs::new();
    let fp = inputs.add_group(b"proj", Origin::FirstParty, None);
    let tp = inputs.add_group(b"dep", Origin::ThirdParty, None);
    for (rel, text) in files {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, text).expect("write");
        let module = rel.trim_end_matches(".py").replace('/', ".");
        let g = if rel.starts_with("dep/") { tp } else { fp };
        inputs.push(p.to_str().expect("utf-8").as_bytes(), g, module.as_bytes(), FileStatus::Active).expect("push");
    }
    (dir, inputs)
}

#[test]
fn loader_fingerprints_first_party_files_only_when_enabled() {
    use crate::index::{LoadOptions, load_with};
    let files = [
        ("pkg/a.py", format!("import os\n\n{}", func("alpha", "a"))),
        ("pkg/b.py", format!("x = 1\n{}", func("beta", "b"))),
        ("dep/c.py", func("gamma", "c")),
        ("pkg/empty.py", String::new()),
    ];
    let params = DupesParams::default();

    let (dir, inputs) = temp_inputs("off", &files);
    let (ix_off, fps) = load_with(inputs, &LoadOptions::default());
    assert!(fps.is_none());

    let (dir2, inputs) = temp_inputs("on", &files);
    let (ix, fps) = load_with(inputs, &LoadOptions { dupes: Some(params) });
    let fps = fps.expect("fingerprints");
    // The index is the same with or without fingerprinting.
    assert_eq!(ix.file_count(), ix_off.file_count());
    assert_eq!(ix.facts().defs.len(), ix_off.facts().defs.len());
    assert_eq!(ix.stats().lines, ix_off.stats().lines);
    // Exactly the lexer fingerprints of the first-party files, in FileId order.
    let mut expected = Fingerprints::new();
    let mut sc = FingerprintScratch::new();
    for (f, (rel, text)) in files.iter().enumerate() {
        if !rel.starts_with("dep/") {
            fingerprint_source(text.as_bytes(), f as u32, &params, &mut sc, &mut expected);
        }
    }
    assert!(!expected.is_empty());
    assert_eq!(fps.as_slice(), expected.as_slice());
    assert!(fps.iter().all(|f| f.file != 2));

    // End to end: the first-party copies are found by re-reading the files.
    let mut out = Findings::new();
    let report = analyse(&ix, fps, &params, &mut out);
    assert_eq!(report.pairs.len(), 1, "{report:?}");
    assert_eq!((report.pairs[0].a.file, report.pairs[0].b.file), (0, 1));
    assert_eq!(report.pairs[0].a.start_line, 3);
    assert_eq!(report.pairs[0].b.start_line, 2);
    assert_eq!(out.len(), 2);
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(dir2);
}

/// Same on the stdlib (`PYMAPPER_STDLIB`, else the path of docs/design.md); files with parser
/// diagnostics may differ (error recovery re-scans), every other file must agree.
/// `cargo test --release -p pymapper-core dupes_stdlib -- --ignored`
#[test]
#[ignore]
fn dupes_stdlib_parser_and_lexer_streams_agree() {
    let root = std::env::var_os("PYMAPPER_STDLIB").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from("/Users/bt386671/.local/share/uv/python/cpython-3.13.5-macos-aarch64-none/lib/python3.13")
    });
    if !root.is_dir() {
        eprintln!("skipping: {} not found", root.display());
        return;
    }
    let params = DupesParams::default();
    let mut differ = Vec::new();
    let mut files = 0;
    for p in walk_py(&root) {
        let src = std::fs::read(&p).expect("read");
        files += 1;
        if parser_fps(&src, 0, &params).as_slice() != lexer_fps(&src, 0, &params).as_slice() {
            differ.push(p);
        }
    }
    eprintln!("{files} files, {} differ: {differ:?}", differ.len());
    assert!(differ.len() * 1000 <= files, "{differ:?}");
}
