//! A minimal pull reader for JSON over `&[u8]`: just what the coverage.py report needs
//! (objects, string keys, arrays of integers, skipping everything else). No allocation except
//! into the caller's buffers; no recursion (skipped values use a depth counter).

/// A malformed document: what was expected and the byte offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonError {
    pub offset: usize,
    pub what: &'static str,
}

pub type Result<T> = std::result::Result<T, JsonError>;

/// Values deeper than this are rejected (the report nests at most 4 levels).
const MAX_DEPTH: u32 = 512;

pub struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Reader<'a> {
        // A UTF-8 byte order mark is tolerated.
        let pos = if b.starts_with(b"\xEF\xBB\xBF") { 3 } else { 0 };
        Reader { b, pos }
    }

    fn err<T>(&self, what: &'static str) -> Result<T> {
        Err(JsonError { offset: self.pos, what })
    }

    #[inline]
    fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.pos) {
            if !matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                break;
            }
            self.pos += 1;
        }
    }

    /// The next non-whitespace byte, not consumed.
    #[inline]
    pub fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.b.get(self.pos).copied()
    }

    fn expect(&mut self, c: u8, what: &'static str) -> Result<()> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            self.err(what)
        }
    }

    /// Fails unless only whitespace is left.
    pub fn end(&mut self) -> Result<()> {
        if self.peek().is_some() { self.err("end of document") } else { Ok(()) }
    }

    /// Consumes the `{` of an object.
    pub fn begin_object(&mut self) -> Result<()> {
        self.expect(b'{', "`{`")
    }

    /// The next key of the current object, decoded into `key` (`false` and the `}` consumed
    /// when the object ends). `first`: no member was read yet (no `,` expected).
    pub fn next_key(&mut self, first: &mut bool, key: &mut Vec<u8>) -> Result<bool> {
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(false);
        }
        if !*first {
            self.expect(b',', "`,` or `}`")?;
        }
        *first = false;
        self.string(key)?;
        self.expect(b':', "`:`")?;
        Ok(true)
    }

    /// A string, decoded into `out` (cleared first). Escapes are decoded (`\uXXXX` with
    /// surrogate pairs to UTF-8; a lone surrogate becomes U+FFFD); raw bytes are copied as is.
    pub fn string(&mut self, out: &mut Vec<u8>) -> Result<()> {
        self.expect(b'"', "a string")?;
        out.clear();
        loop {
            let rest = &self.b[self.pos..];
            let Some(i) = rest.iter().position(|&c| c == b'"' || c == b'\\') else {
                self.pos = self.b.len();
                return self.err("end of string");
            };
            out.extend_from_slice(&rest[..i]);
            self.pos += i + 1;
            if rest[i] == b'"' {
                return Ok(());
            }
            let Some(&e) = self.b.get(self.pos) else { return self.err("escape") };
            self.pos += 1;
            let c = match e {
                b'"' => b'"',
                b'\\' => b'\\',
                b'/' => b'/',
                b'b' => 8,
                b'f' => 12,
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'u' => {
                    let mut cp = self.hex4()?;
                    if (0xD800..0xDC00).contains(&cp) && self.b[self.pos..].starts_with(b"\\u") {
                        let save = self.pos;
                        self.pos += 2;
                        let lo = self.hex4()?;
                        if (0xDC00..0xE000).contains(&lo) {
                            cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                        } else {
                            self.pos = save;
                        }
                    }
                    let ch = char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER);
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    continue;
                }
                _ => return self.err("a valid escape"),
            };
            out.push(c);
        }
    }

    fn hex4(&mut self) -> Result<u32> {
        let Some(h) = self.b.get(self.pos..self.pos + 4) else { return self.err("4 hex digits") };
        let mut v = 0u32;
        for &c in h {
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return self.err("4 hex digits"),
            };
            v = v << 4 | u32::from(d);
        }
        self.pos += 4;
        Ok(v)
    }

    /// An array of non-negative integers appended to `out`. Values above `u32::MAX` saturate;
    /// any other element is an error.
    pub fn u32_array(&mut self, out: &mut Vec<u32>) -> Result<()> {
        self.expect(b'[', "`[`")?;
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(());
        }
        loop {
            self.ws();
            let start = self.pos;
            let mut v: u64 = 0;
            while let Some(&c) = self.b.get(self.pos) {
                if !c.is_ascii_digit() {
                    break;
                }
                v = (v * 10 + u64::from(c - b'0')).min(u64::from(u32::MAX));
                self.pos += 1;
            }
            if self.pos == start {
                return self.err("a line number");
            }
            out.push(v as u32);
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(());
                }
                _ => return self.err("`,` or `]`"),
            }
        }
    }

    /// Skips one value of any kind. Strings are scanned for their end only and scalars up to
    /// the next delimiter: a skipped value is checked for balanced brackets, not validated.
    pub fn skip_value(&mut self) -> Result<()> {
        let mut depth = 0u32;
        loop {
            let Some(c) = self.peek() else { return self.err("a value") };
            match c {
                b'"' => {
                    self.pos += 1;
                    loop {
                        let rest = &self.b[self.pos..];
                        let Some(i) = rest.iter().position(|&c| c == b'"' || c == b'\\') else {
                            self.pos = self.b.len();
                            return self.err("end of string");
                        };
                        self.pos += i + 1;
                        if rest[i] == b'"' {
                            break;
                        }
                        // Skip the escaped byte (`\"` must not end the string).
                        self.pos = (self.pos + 1).min(self.b.len());
                    }
                }
                b'{' | b'[' => {
                    depth += 1;
                    if depth > MAX_DEPTH {
                        return self.err("less nesting");
                    }
                    self.pos += 1;
                    continue;
                }
                b'}' | b']' => {
                    if depth == 0 {
                        return self.err("a value");
                    }
                    depth -= 1;
                    self.pos += 1;
                }
                b',' | b':' if depth > 0 => {
                    self.pos += 1;
                    continue;
                }
                _ => {
                    let start = self.pos;
                    while let Some(&c) = self.b.get(self.pos) {
                        if matches!(c, b',' | b'}' | b']' | b':' | b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'{' | b'[') {
                            break;
                        }
                        self.pos += 1;
                    }
                    if self.pos == start {
                        return self.err("a value");
                    }
                }
            }
            if depth == 0 {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_objects_keys_and_arrays() {
        let mut r = Reader::new(b"\xEF\xBB\xBF { \"a\" : [1, 2 ,30], \"b\\n\\u00e9\\ud83d\\ude00\": [] }  ");
        let (mut key, mut first, mut out) = (Vec::new(), true, Vec::new());
        r.begin_object().unwrap();
        assert!(r.next_key(&mut first, &mut key).unwrap());
        assert_eq!(key, b"a");
        r.u32_array(&mut out).unwrap();
        assert_eq!(out, [1, 2, 30]);
        assert!(r.next_key(&mut first, &mut key).unwrap());
        assert_eq!(key, "b\né😀".as_bytes());
        r.u32_array(&mut out).unwrap();
        assert_eq!(out, [1, 2, 30]);
        assert!(!r.next_key(&mut first, &mut key).unwrap());
        r.end().unwrap();
    }

    #[test]
    fn skips_any_value() {
        for v in [
            &b"{\"x\": [1, {\"y\": \"a]}\\\"\"}, null], \"z\": true}"[..],
            b"\"str\\\\\"",
            b"-1.5e3",
            b"[[[]]]",
            b"{}",
        ] {
            let mut doc = v.to_vec();
            doc.extend_from_slice(b" ,");
            let mut r = Reader::new(&doc);
            r.skip_value().unwrap_or_else(|e| panic!("{e:?} in {}", String::from_utf8_lossy(v)));
            assert_eq!(r.peek(), Some(b','));
        }
    }

    #[test]
    fn errors_never_panic() {
        let bad: &[&[u8]] = &[b"", b"{", b"{\"a", b"{\"a\":", b"[1,", b"[x]", b"\"\\u12", b"\"\\", b"]", b"{\"a\" 1}", b"[1 2]"];
        for &b in bad {
            let mut r = Reader::new(b);
            let mut key = Vec::new();
            let mut first = true;
            let _ = r.skip_value();
            let mut r2 = Reader::new(b);
            let _ = r2.begin_object().and_then(|()| r2.next_key(&mut first, &mut key)).and_then(|_| r2.skip_value());
            let mut r3 = Reader::new(b);
            let _ = r3.u32_array(&mut Vec::new());
            let mut r4 = Reader::new(b);
            let _ = r4.string(&mut key);
        }
        // Too deep.
        let deep = vec![b'['; 10_000];
        assert!(Reader::new(&deep).skip_value().is_err());
        // Lone surrogate.
        let mut out = Vec::new();
        Reader::new(b"\"\\ud800x\"").string(&mut out).unwrap();
        assert_eq!(out, "\u{FFFD}x".as_bytes());
        // Saturating numbers.
        let mut nums = Vec::new();
        Reader::new(b"[99999999999]").u32_array(&mut nums).unwrap();
        assert_eq!(nums, [u32::MAX]);
    }
}
