//! A small pull parser for JSON (RFC 8259), enough to read baseline files without a DOM:
//! callers walk objects and arrays, decode strings into a reused buffer and skip what they do
//! not know.

/// A syntax error at a byte offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SyntaxError {
    pub offset: usize,
    pub what: &'static str,
}

pub(crate) type Result<T> = std::result::Result<T, SyntaxError>;

/// Nesting limit for skipped values (bounded work and no unbounded recursion on hostile input).
const MAX_DEPTH: usize = 128;

pub(crate) struct Reader<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(s: &'a [u8]) -> Reader<'a> {
        let s = s.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(s);
        Reader { s, pos: 0 }
    }

    pub fn err<T>(&self, what: &'static str) -> Result<T> {
        Err(SyntaxError { offset: self.pos, what })
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.s.get(self.pos) {
            self.pos += 1;
        }
    }

    /// Next non-whitespace byte, not consumed.
    pub fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.pos).copied()
    }

    fn eat(&mut self, b: u8, what: &'static str) -> Result<()> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            self.err(what)
        }
    }

    /// Only whitespace remains.
    pub fn end(&mut self) -> Result<()> {
        match self.peek() {
            None => Ok(()),
            Some(_) => self.err("trailing characters"),
        }
    }

    /// Consumes `{`.
    pub fn begin_object(&mut self) -> Result<()> {
        self.eat(b'{', "expected `{`")
    }

    /// Reads the next key of an object into `key` (true), or consumes `}` (false). `first` is
    /// true for the first call after [`begin_object`](Self::begin_object).
    pub fn next_key(&mut self, first: bool, key: &mut Vec<u8>) -> Result<bool> {
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(false);
        }
        if !first {
            self.eat(b',', "expected `,` or `}`")?;
        }
        self.string(key)?;
        self.eat(b':', "expected `:`")?;
        Ok(true)
    }

    /// Consumes `[`.
    pub fn begin_array(&mut self) -> Result<()> {
        self.eat(b'[', "expected `[`")
    }

    /// Positions at the next array element (true), or consumes `]` (false).
    pub fn next_item(&mut self, first: bool) -> Result<bool> {
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(false);
        }
        if !first {
            self.eat(b',', "expected `,` or `]`")?;
        }
        Ok(true)
    }

    /// Decodes a string into `out` (cleared first).
    pub fn string(&mut self, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        self.eat(b'"', "expected a string")?;
        loop {
            let start = self.pos;
            while let Some(&b) = self.s.get(self.pos) {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            out.extend_from_slice(&self.s[start..self.pos]);
            match self.s.get(self.pos) {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(());
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(out)?;
                }
                Some(_) => return self.err("control character in string"),
                None => return self.err("unterminated string"),
            }
        }
    }

    fn escape(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let Some(&c) = self.s.get(self.pos) else { return self.err("unterminated string") };
        self.pos += 1;
        let simple = match c {
            b'"' => b'"',
            b'\\' => b'\\',
            b'/' => b'/',
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'u' => {
                let hi = self.hex4()?;
                let cp = if (0xd800..0xdc00).contains(&hi) {
                    // A high surrogate must be followed by `\u` + low surrogate.
                    if self.s.get(self.pos..self.pos + 2) != Some(b"\\u") {
                        return self.err("unpaired surrogate");
                    }
                    self.pos += 2;
                    let lo = self.hex4()?;
                    if !(0xdc00..0xe000).contains(&lo) {
                        return self.err("unpaired surrogate");
                    }
                    0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                } else if (0xdc00..0xe000).contains(&hi) {
                    return self.err("unpaired surrogate");
                } else {
                    hi
                };
                let ch = char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER);
                let mut tmp = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                return Ok(());
            }
            _ => return self.err("invalid escape"),
        };
        out.push(simple);
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32> {
        let Some(d) = self.s.get(self.pos..self.pos + 4) else { return self.err("truncated \\u escape") };
        let mut v = 0u32;
        for &c in d {
            let x = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return self.err("invalid \\u escape"),
            };
            v = v * 16 + u32::from(x);
        }
        self.pos += 4;
        Ok(v)
    }

    /// Reads a non-negative integer that fits in a `u32`.
    pub fn u32(&mut self) -> Result<u32> {
        self.ws();
        let start = self.pos;
        let mut v: u64 = 0;
        while let Some(&c) = self.s.get(self.pos) {
            if !c.is_ascii_digit() {
                break;
            }
            v = v * 10 + u64::from(c - b'0');
            if v > u64::from(u32::MAX) {
                return self.err("number out of range");
            }
            self.pos += 1;
        }
        let n = self.pos - start;
        if n == 0 || (n > 1 && self.s[start] == b'0') {
            self.pos = start;
            return self.err("expected a non-negative integer");
        }
        if let Some(b'.' | b'e' | b'E') = self.s.get(self.pos) {
            return self.err("expected an integer");
        }
        Ok(v as u32)
    }

    /// Skips one value of any type (validating it).
    pub fn skip(&mut self) -> Result<()> {
        let mut scratch = Vec::new();
        self.skip_at(0, &mut scratch)
    }

    fn skip_at(&mut self, depth: usize, scratch: &mut Vec<u8>) -> Result<()> {
        if depth > MAX_DEPTH {
            return self.err("nesting too deep");
        }
        match self.peek() {
            Some(b'{') => {
                self.pos += 1;
                let mut first = true;
                while self.next_key(first, scratch)? {
                    first = false;
                    self.skip_at(depth + 1, scratch)?;
                }
                Ok(())
            }
            Some(b'[') => {
                self.pos += 1;
                let mut first = true;
                while self.next_item(first)? {
                    first = false;
                    self.skip_at(depth + 1, scratch)?;
                }
                Ok(())
            }
            Some(b'"') => self.string(scratch),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => self.err("expected a value"),
        }
    }

    fn literal(&mut self, lit: &'static [u8]) -> Result<()> {
        if self.s[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(())
        } else {
            self.err("invalid literal")
        }
    }

    fn number(&mut self) -> Result<()> {
        let digits = |r: &mut Reader<'_>| {
            let start = r.pos;
            while r.s.get(r.pos).is_some_and(u8::is_ascii_digit) {
                r.pos += 1;
            }
            r.pos - start
        };
        if self.s.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        let int_start = self.pos;
        let n = digits(self);
        if n == 0 || (n > 1 && self.s[int_start] == b'0') {
            return self.err("invalid number");
        }
        if self.s.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            if digits(self) == 0 {
                return self.err("invalid number");
            }
        }
        if let Some(b'e' | b'E') = self.s.get(self.pos) {
            self.pos += 1;
            if let Some(b'+' | b'-') = self.s.get(self.pos) {
                self.pos += 1;
            }
            if digits(self) == 0 {
                return self.err("invalid number");
            }
        }
        Ok(())
    }
}
