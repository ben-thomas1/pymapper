//! Fuzz target registry.
//!
//! A target is a plain `fn(&[u8])` that must never panic, hang, or abort for any input. Add a
//! target by appending an entry to [`TARGETS`]; the first entry is the default. The lexer and
//! extractor steps plug in `lex` and `extract` here (put them first).

use std::hint::black_box;
use std::time::Duration;

/// A named fuzz target.
pub struct FuzzTarget {
    pub name: &'static str,
    pub about: &'static str,
    /// Self-test targets have planted bugs; they exist to test the fuzzer itself.
    pub selftest: bool,
    pub run: fn(&[u8]),
}

pub const TARGETS: &[FuzzTarget] = &[
    FuzzTarget {
        name: "demo",
        about: "toy tokenizer with invariant checks (placeholder until lex/extract land)",
        selftest: false,
        run: demo,
    },
    FuzzTarget {
        name: "selftest-panic",
        about: "planted panics on a NUL byte and on '{{{{' (tests the fuzzer)",
        selftest: true,
        run: selftest_panic,
    },
    FuzzTarget {
        name: "selftest-hang",
        about: "planted 1.5 s stall on a UTF-8 BOM anywhere in the input (tests the fuzzer)",
        selftest: true,
        run: selftest_hang,
    },
    FuzzTarget {
        name: "selftest-abort",
        about: "planted unbounded recursion (stack overflow kills the process) on a backtick (tests the fuzzer)",
        selftest: true,
        run: selftest_abort,
    },
];

pub fn find(name: &str) -> Option<&'static FuzzTarget> {
    TARGETS.iter().find(|t| t.name == name)
}

/// Token classes of the toy tokenizer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Space,
    Newline,
    Word,
    Str,
    Comment,
    Punct,
}

/// Next token starting at `pos < src.len()`: `(class, len)` with `len >= 1`.
fn next_token(src: &[u8], pos: usize) -> (Class, usize) {
    let rest = &src[pos..];
    let b = rest[0];
    let run = |pred: fn(u8) -> bool| rest.iter().take_while(|&&c| pred(c)).count();
    match b {
        b' ' | b'\t' | b'\x0c' => (Class::Space, run(|c| c == b' ' || c == b'\t' || c == b'\x0c')),
        b'\n' | b'\r' => (Class::Newline, 1),
        b'#' => (Class::Comment, run(|c| c != b'\n' && c != b'\r')),
        b'\'' | b'"' => {
            // Single-line string with backslash escapes; unterminated strings end at the newline.
            let mut i = 1;
            while i < rest.len() {
                match rest[i] {
                    b'\\' => i += 2,
                    b'\n' | b'\r' => break,
                    c if c == b => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            (Class::Str, i.min(rest.len()))
        }
        c if c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80 => {
            (Class::Word, run(|c| c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80))
        }
        _ => (Class::Punct, 1),
    }
}

/// Demo target: tokenizes with a toy scanner and checks its invariants (tokens tile the input
/// exactly, bracket depth never underflows its saturating counter, line count matches).
fn demo(src: &[u8]) {
    let mut pos = 0usize;
    let mut tokens = 0u32;
    let mut lines = 0usize;
    let mut depth = 0u32;
    while pos < src.len() {
        let (class, len) = next_token(src, pos);
        assert!(len >= 1 && pos + len <= src.len(), "token out of bounds");
        match class {
            Class::Newline => lines += usize::from(src[pos] == b'\n'),
            Class::Punct => match src[pos] {
                b'(' | b'[' | b'{' => depth = depth.saturating_add(1),
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            },
            Class::Str => assert!(len >= 1 && (src[pos] == b'\'' || src[pos] == b'"')),
            Class::Space | Class::Word | Class::Comment => {}
        }
        tokens = tokens.wrapping_add(1);
        pos += len;
    }
    assert_eq!(pos, src.len(), "tokens must tile the input");
    // Strings never swallow a '\n' (a backslash may escape one), so every newline is seen once
    // unless escaped inside a string.
    assert!(lines <= src.iter().filter(|&&b| b == b'\n').count());
    black_box((tokens, depth));
}

fn selftest_panic(src: &[u8]) {
    if src.contains(&0) {
        panic!("selftest: NUL byte in input");
    }
    if src.windows(4).any(|w| w == b"{{{{") {
        panic!("selftest: four opening braces");
    }
    demo(src);
}

const BOM: &[u8] = "\u{FEFF}".as_bytes();

fn selftest_hang(src: &[u8]) {
    if src.windows(BOM.len()).any(|w| w == BOM) {
        // Bounded so abandoned threads in tests eventually finish.
        std::thread::sleep(Duration::from_millis(1500));
    }
    demo(src);
}

/// Recurses `depth` levels with a 256-byte frame each (opaque to the optimiser, so it cannot
/// become a loop).
fn recurse(depth: u64) -> u64 {
    let frame = black_box([depth as u8; 256]);
    if depth == 0 { u64::from(frame[0]) } else { recurse(black_box(depth - 1)).wrapping_add(u64::from(frame[255])) }
}

fn selftest_abort(src: &[u8]) {
    if src.contains(&b'`') {
        // No real stack is this deep: the guard page turns this into a fatal stack overflow,
        // which catch_unwind cannot catch.
        black_box(recurse(black_box(u64::MAX)));
    }
    demo(src);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_are_unique_and_default_is_not_a_selftest() {
        let mut names: Vec<&str> = TARGETS.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TARGETS.len());
        assert!(!TARGETS[0].selftest);
        assert!(find("demo").is_some());
        assert!(find("nope").is_none());
    }

    #[test]
    fn demo_tokenizes_awkward_inputs() {
        for src in [
            &b""[..],
            b"'",
            b"'\\",
            b"\"abc\\\"",
            b"x = 'a\\\nb'\n",
            b"((((]]]]",
            b"#",
            b"\xff\xfe\0",
            b"def f(x):\n\treturn f'{x!r}'\n",
        ] {
            demo(src);
        }
        assert_eq!(next_token(b"'ab' x", 0), (Class::Str, 4));
        assert_eq!(next_token(b"'a\\'b'", 0), (Class::Str, 6));
        assert_eq!(next_token(b"'ab\nc'", 0), (Class::Str, 3));
        assert_eq!(next_token(b"abc_1 x", 0), (Class::Word, 5));
        assert_eq!(next_token(b"\t \x0cx", 0), (Class::Space, 3));
        assert_eq!(next_token(b"# c\nx", 0), (Class::Comment, 3));
    }

    #[test]
    fn selftests_trigger_on_their_patterns_only() {
        selftest_panic(b"x = 1\n");
        selftest_hang(b"x = 1\n");
        let nul = std::panic::catch_unwind(|| selftest_panic(b"a\0b"));
        assert!(nul.is_err());
        let braces = std::panic::catch_unwind(|| selftest_panic(b"{{{{"));
        assert!(braces.is_err());
    }
}
