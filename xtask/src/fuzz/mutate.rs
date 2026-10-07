//! Seed store and the mutation engine.
//!
//! An input is a pure function of `(seeds, seed, iter, max_len)`: pick a seed file, then apply a
//! short stack of mutations (byte flips, interesting tokens, truncation, splicing with another
//! seed file, indentation scrambles, ...). Generation writes into a caller-owned buffer, so a
//! worker allocates nothing per input once its buffer has grown.

use super::rng::Rng;

/// Immutable seed inputs in one byte buffer plus end offsets (no per-file allocation).
#[derive(Default)]
pub struct Seeds {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl Seeds {
    /// Adds a seed, truncated to `max_len` bytes. Fails only past 4 GiB of seed data.
    pub fn push(&mut self, data: &[u8], max_len: usize) -> anyhow::Result<()> {
        let data = &data[..data.len().min(max_len)];
        self.bytes.extend_from_slice(data);
        let end = u32::try_from(self.bytes.len()).map_err(|_| anyhow::anyhow!("seed corpus exceeds 4 GiB"))?;
        self.ends.push(end);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn total_bytes(&self) -> usize {
        self.bytes.len()
    }

    pub fn get(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.bytes[start..self.ends[i] as usize]
    }
}

/// Tokens that tend to break Python lexers and parsers: quotes and string prefixes, brackets,
/// backslashes and continuations, newlines and other whitespace, f/t-string braces and format
/// specs, non-ASCII and invalid UTF-8, NUL, and grammar edge cases.
pub const TOKENS: &[&[u8]] = &[
    // quotes and string prefixes
    b"'", b"\"", b"'''", b"\"\"\"", b"f'", b"f\"", b"rb'", b"Rb\"", b"u'", b"br\"\"\"", b"t'", b"T\"\"\"",
    b"fr'", b"rf\"", b"tr'''", b"f'''",
    // brackets
    b"(", b")", b"[", b"]", b"{", b"}", b"((((", b"))))", b"[[[[", b"{{{{",
    // backslash, continuation, newlines and other whitespace
    b"\\", b"\\\n", b"\\\r\n", b"\\\\", b"\n", b"\r", b"\r\n", b"\n\n", b"\t", b"\x0c", b"\x0b", b" ",
    b"    ", b"\t ",
    // f-string / t-string braces and format specs
    b"{{", b"}}", b"{x}", b"{x!r}", b"{x=}", b"{x:>{w}}", b"{x:{y:{z}}}", b"{'a'}", b"{f'{x}'}", b"!r", b":",
    b"f'{", b"}'", b"f\"{x:{", b"t'{x}'",
    // unicode, BOM, invalid UTF-8, NUL
    "\u{e9}".as_bytes(), "\u{3bb}".as_bytes(), "\u{FEFF}".as_bytes(), "\u{2028}".as_bytes(),
    "\u{1F600}".as_bytes(), "\u{0301}".as_bytes(), "\u{FF21}".as_bytes(), b"\xff", b"\xc3", b"\xe2\x80",
    b"\xf0\x9f", b"\xed\xa0\x80", b"\0", b"\0\0\0\0",
    // comments and suppressions
    b"#", b"# comment\n", b"# pymapper: ignore[PM001]", b"# pymapper: ignore[", b"# type: ignore",
    // keywords, soft keywords and operators
    b"def ", b"class ", b"async ", b"await ", b"lambda", b"lambda: ", b"yield ", b"return ", b"match ",
    b"case ", b"type ", b"_", b"if ", b"elif ", b"else:", b"try:", b"except* ", b"finally:", b"with ",
    b"for ", b"while ", b"import ", b"from . import ", b"global ", b"nonlocal ", b"del ", b"pass",
    b"@", b"->", b":=", b"**", b"//=", b">>=", b"...", b".", b",", b";", b"=", b"==", b"!=", b"<>", b"~",
    b"$", b"?", b"`",
    // numbers
    b"0", b"0x", b"0xFFFF_FFFF", b"0o17", b"0b1", b"1_000", b"1__0", b"1e", b"1e+", b"1.5e-3j", b".5j",
    b"0777", b"1.", b"9" ,
];

/// Single bytes that are interesting on their own.
const BYTES: &[u8] = b"\0\t\n\r\x0c\\'\"#{}()[]:;,.=@ \x7f\x80\xc0\xff";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    FlipBit,
    SetByte,
    InsertToken,
    ReplaceWithToken,
    Truncate,
    DeleteRange,
    DuplicateRange,
    Splice,
    Crossover,
    Indent,
    RandomBytes,
}

/// Mutation operators weighted by how often they are picked.
const OPS: &[(Op, u8)] = &[
    (Op::FlipBit, 2),
    (Op::SetByte, 2),
    (Op::InsertToken, 7),
    (Op::ReplaceWithToken, 2),
    (Op::Truncate, 1),
    (Op::DeleteRange, 2),
    (Op::DuplicateRange, 2),
    (Op::Splice, 2),
    (Op::Crossover, 1),
    (Op::Indent, 4),
    (Op::RandomBytes, 1),
];

const OP_WEIGHT_TOTAL: usize = {
    let mut t = 0;
    let mut i = 0;
    while i < OPS.len() {
        t += OPS[i].1 as usize;
        i += 1;
    }
    t
};

fn pick_op(rng: &mut Rng) -> Op {
    let mut r = rng.below(OP_WEIGHT_TOTAL);
    for &(op, w) in OPS {
        if r < w as usize {
            return op;
        }
        r -= w as usize;
    }
    unreachable!("weights sum to OP_WEIGHT_TOTAL")
}

/// Generates inputs for one run. Cheap to share between threads (read-only).
pub struct Mutator<'a> {
    seeds: &'a Seeds,
    seed: u64,
    max_len: usize,
}

/// Per-thread scratch space reused across inputs.
#[derive(Default)]
pub struct Scratch {
    tmp: Vec<u8>,
}

impl<'a> Mutator<'a> {
    /// `seeds` must be non-empty; `max_len` caps every generated input.
    pub fn new(seeds: &'a Seeds, seed: u64, max_len: usize) -> Mutator<'a> {
        assert!(seeds.len() > 0, "mutator needs at least one seed");
        Mutator { seeds, seed, max_len: max_len.max(1) }
    }

    /// Writes input number `iter` into `out` (replacing its contents).
    pub fn generate(&self, iter: u64, out: &mut Vec<u8>, scratch: &mut Scratch) {
        let mut rng = Rng::for_iteration(self.seed, iter);
        out.clear();
        out.extend_from_slice(self.seeds.get(rng.below(self.seeds.len())));
        let n = rng.small_len(3);
        for _ in 0..n {
            self.apply(pick_op(&mut rng), &mut rng, out, scratch);
            out.truncate(self.max_len);
        }
    }

    fn apply(&self, op: Op, rng: &mut Rng, out: &mut Vec<u8>, scratch: &mut Scratch) {
        let len = out.len();
        match op {
            Op::FlipBit => {
                if len > 0 {
                    let pos = rng.below(len);
                    out[pos] ^= 1 << rng.below(8);
                }
            }
            Op::SetByte => {
                if len > 0 {
                    let pos = rng.below(len);
                    out[pos] = if rng.one_in(4) { rng.next_u64() as u8 } else { *rng.choose(BYTES) };
                }
            }
            Op::InsertToken => {
                let tok = *rng.choose(TOKENS);
                // Occasionally repeat the token many times (deep nesting, long runs).
                let reps = if rng.one_in(8) { rng.small_len(9) } else { 1 };
                let pos = rng.below(len + 1);
                let tmp = &mut scratch.tmp;
                tmp.clear();
                for _ in 0..reps {
                    tmp.extend_from_slice(tok);
                }
                out.splice(pos..pos, tmp.iter().copied());
            }
            Op::ReplaceWithToken => {
                let tok = *rng.choose(TOKENS);
                let start = rng.below(len + 1);
                let end = (start + rng.small_len(3)).min(len);
                out.splice(start..end, tok.iter().copied());
            }
            Op::Truncate => out.truncate(rng.below(len + 1)),
            Op::DeleteRange => {
                if len > 0 {
                    let start = rng.below(len);
                    let end = (start + rng.small_len(8)).min(len);
                    out.drain(start..end);
                }
            }
            Op::DuplicateRange => {
                if len > 0 {
                    let start = rng.below(len);
                    let end = (start + rng.small_len(8)).min(len);
                    let reps = if rng.one_in(4) { rng.small_len(5) } else { 1 };
                    let at = rng.below(len + 1);
                    for _ in 0..reps {
                        if out.len() + (end - start) > self.max_len {
                            break;
                        }
                        insert_from_within(out, start..end, at);
                    }
                }
            }
            Op::Splice => {
                let other = self.seeds.get(rng.below(self.seeds.len()));
                if !other.is_empty() {
                    let start = rng.below(other.len());
                    let end = (start + rng.small_len(10)).min(other.len());
                    let pos = rng.below(len + 1);
                    out.splice(pos..pos, other[start..end].iter().copied());
                }
            }
            Op::Crossover => {
                let other = self.seeds.get(rng.below(self.seeds.len()));
                out.truncate(rng.below(len + 1));
                let from = rng.below(other.len() + 1);
                let take = (other.len() - from).min(self.max_len.saturating_sub(out.len()));
                out.extend_from_slice(&other[from..from + take]);
            }
            Op::Indent => scramble_indent(rng, out, &mut scratch.tmp),
            Op::RandomBytes => {
                let n = rng.small_len(4);
                let pos = rng.below(len + 1);
                let tmp = &mut scratch.tmp;
                tmp.clear();
                for _ in 0..n {
                    tmp.push(rng.next_u64() as u8);
                }
                out.splice(pos..pos, tmp.iter().copied());
            }
        }
    }
}

/// Inserts a copy of `out[range]` at `at` (positions refer to `out` before the insertion).
fn insert_from_within(out: &mut Vec<u8>, range: std::ops::Range<usize>, at: usize) {
    let n = range.len();
    out.extend_from_within(range);
    out[at..].rotate_right(n);
}

fn is_indent_byte(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\x0c'
}

/// Rewrites the leading whitespace of a run of 1..=8 consecutive lines starting at a random line,
/// all in the same way (so whole blocks get shifted, dedented, or switched to tabs).
fn scramble_indent(rng: &mut Rng, out: &mut Vec<u8>, tmp: &mut Vec<u8>) {
    let pos = rng.below(out.len() + 1);
    let mut line = match out[..pos].iter().rposition(|&b| b == b'\n') {
        Some(nl) => nl + 1,
        None => 0,
    };
    let lines = rng.small_len(3);
    let variant = rng.below(7);
    let amount = rng.small_len(3);
    for _ in 0..lines {
        let ws_end = line + out[line..].iter().take_while(|&&b| is_indent_byte(b)).count();
        let old = &out[line..ws_end];
        tmp.clear();
        match variant {
            // dedent fully
            0 => {}
            // indent further with spaces
            1 => {
                tmp.extend_from_slice(old);
                tmp.resize(old.len() + amount, b' ');
            }
            // replace with tabs
            2 => tmp.resize(amount, b'\t'),
            // prepend a tab to the existing indentation (inconsistent tabs/spaces)
            3 => {
                tmp.push(b'\t');
                tmp.extend_from_slice(old);
            }
            // drop the last indentation byte (partial dedent)
            4 => tmp.extend_from_slice(&old[..old.len().saturating_sub(1)]),
            // swap spaces and tabs
            5 => tmp.extend(old.iter().map(|&b| if b == b' ' { b'\t' } else { b' ' })),
            // form feed somewhere in the indentation
            _ => {
                let at = rng.below(old.len() + 1);
                tmp.extend_from_slice(old);
                tmp.insert(at, b'\x0c');
            }
        }
        let new_end = line + tmp.len();
        out.splice(line..ws_end, tmp.iter().copied());
        match out[new_end..].iter().position(|&b| b == b'\n') {
            Some(off) => line = new_end + off + 1,
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeds(items: &[&[u8]]) -> Seeds {
        let mut s = Seeds::default();
        for it in items {
            s.push(it, 1 << 20).unwrap();
        }
        s
    }

    const PY: &[u8] = b"def f(x):\n    if x:\n        return f'{x!r}'\n    return 0\n\nclass C:\n    pass\n";

    #[test]
    fn seed_store_round_trips_and_truncates() {
        let mut s = Seeds::default();
        s.push(b"abc", 2).unwrap();
        s.push(b"", 2).unwrap();
        s.push(b"xyz", 10).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s.get(0), b"ab");
        assert_eq!(s.get(1), b"");
        assert_eq!(s.get(2), b"xyz");
        assert_eq!(s.total_bytes(), 5);
    }

    #[test]
    fn generation_is_deterministic_per_iteration() {
        let s = seeds(&[PY, b"x = 1\n", b""]);
        let m = Mutator::new(&s, 1234, 4096);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let mut scratch = Scratch::default();
        let mut distinct = std::collections::HashSet::new();
        for iter in 0..2000 {
            m.generate(iter, &mut a, &mut scratch);
            // A dirty buffer and different scratch history must not change the result.
            b.extend_from_slice(b"garbage");
            m.generate(iter, &mut b, &mut Scratch::default());
            assert_eq!(a, b, "iteration {iter}");
            distinct.insert(a.clone());
        }
        assert!(distinct.len() > 1500, "too few distinct inputs: {}", distinct.len());
        let other = Mutator::new(&s, 1235, 4096);
        let mut c = Vec::new();
        let same = (0..200)
            .filter(|&i| {
                m.generate(i, &mut a, &mut scratch);
                other.generate(i, &mut c, &mut scratch);
                a == c
            })
            .count();
        assert!(same < 40, "different seeds should give different inputs ({same}/200 equal)");
    }

    #[test]
    fn respects_max_len_and_handles_empty_seeds() {
        let s = seeds(&[b"", PY]);
        let mut scratch = Scratch::default();
        let mut out = Vec::new();
        for max_len in [1, 7, 64] {
            let m = Mutator::new(&s, 5, max_len);
            for iter in 0..3000 {
                m.generate(iter, &mut out, &mut scratch);
                assert!(out.len() <= max_len, "len {} > {max_len}", out.len());
            }
        }
        // Only an empty seed: every operator must cope with empty input.
        let empty = seeds(&[b""]);
        let m = Mutator::new(&empty, 6, 256);
        let mut nonempty = 0;
        for iter in 0..3000 {
            m.generate(iter, &mut out, &mut scratch);
            nonempty += usize::from(!out.is_empty());
        }
        assert!(nonempty > 1000);
    }

    #[test]
    fn every_operator_runs_on_awkward_inputs() {
        let s = seeds(&[PY, b"\n\n\n", b"\t \x0c", b"x"]);
        let m = Mutator::new(&s, 77, 512);
        let mut scratch = Scratch::default();
        for (op, _) in OPS {
            for start in [&b""[..], b"\n", b"  ", b"a\n  b\n\tc", PY] {
                for i in 0..200u64 {
                    let mut rng = Rng::new(i);
                    let mut out = start.to_vec();
                    m.apply(*op, &mut rng, &mut out, &mut scratch);
                }
            }
        }
    }

    #[test]
    fn mutations_reach_interesting_tokens_and_splices() {
        let s = seeds(&[b"aaaa\naaaa\n", b"zzzz\nzzzz\n"]);
        let m = Mutator::new(&s, 3, 4096);
        let mut scratch = Scratch::default();
        let mut out = Vec::new();
        let (mut nul, mut fbrace, mut spliced, mut tab_indent, mut bom, mut truncated) = (0, 0, 0, 0, 0, 0);
        for iter in 0..20_000 {
            m.generate(iter, &mut out, &mut scratch);
            let has = |needle: &[u8]| out.windows(needle.len()).any(|w| w == needle);
            nul += usize::from(out.contains(&0));
            fbrace += usize::from(has(b"{x"));
            spliced += usize::from(out.contains(&b'a') && out.contains(&b'z'));
            tab_indent += usize::from(has(b"\n\ta") || has(b"\n\tz") || out.starts_with(b"\t"));
            bom += usize::from(has("\u{FEFF}".as_bytes()));
            truncated += usize::from(out.len() < 10);
        }
        for (name, n) in [
            ("nul", nul),
            ("f-brace", fbrace),
            ("splice", spliced),
            ("tab indent", tab_indent),
            ("bom", bom),
            ("truncated", truncated),
        ] {
            assert!(n > 20, "{name}: only {n} inputs");
        }
    }

    #[test]
    fn indent_scramble_rewrites_leading_whitespace_only() {
        let src = b"a:\n    b\n    c\nd\n";
        let mut tmp = Vec::new();
        let mut changed = 0;
        for i in 0..500 {
            let mut rng = Rng::new(i);
            let mut out = src.to_vec();
            scramble_indent(&mut rng, &mut out, &mut tmp);
            // Removing all indentation bytes gives back the same text.
            let strip = |v: &[u8]| -> Vec<u8> { v.iter().copied().filter(|&b| !is_indent_byte(b)).collect() };
            assert_eq!(strip(&out), strip(src), "seed {i}: {:?}", String::from_utf8_lossy(&out));
            changed += usize::from(out != src);
        }
        assert!(changed > 250);
    }

    #[test]
    fn insert_from_within_places_copy() {
        let mut v = b"abcdef".to_vec();
        insert_from_within(&mut v, 1..3, 0);
        assert_eq!(v, b"bcabcdef");
        let mut v = b"abcdef".to_vec();
        insert_from_within(&mut v, 0..2, 6);
        assert_eq!(v, b"abcdefab");
        let mut v = b"abcdef".to_vec();
        insert_from_within(&mut v, 4..6, 2);
        assert_eq!(v, b"abefcdef");
    }
}
