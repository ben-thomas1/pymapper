//! `Requires-Dist` of installed distributions (PM604): which distributions a declared
//! dependency pulls in, transitively.
//!
//! Read lazily: only for the distributions reached from the declared dependencies of a unit,
//! and only once an undeclared import makes PM604 ask. One read of each `METADATA` header block
//! (the body, often a long README, is not read when the headers end within the first 16 KiB).

use std::fs::File;
use std::io::Read;

use crate::intern::{FxHashMap, Interner, Sym};
use crate::roots::{DistId, Distributions, normalize_name};

/// Bytes of `METADATA` read first.
const PREFIX: u64 = 16 * 1024;
/// Larger metadata files are not read past the prefix.
const MAX: u64 = 16 * 1024 * 1024;

/// One `Requires-Dist` entry: the required name, the extras it requests (`req[extra]`), and the
/// extras of the requiring distribution that enable it (`; extra == "x"`; empty: always).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    pub norm: Sym,
    pub extras: Vec<Sym>,
    pub when_extra: Vec<Sym>,
}

/// Calls `f(name, extras, marker)` for every `Requires-Dist` header of an RFC 822 header block
/// (`extras`: the text between `[` and `]`; `marker`: the text after `;`, both possibly empty).
pub fn for_each_requires_dist(headers: &[u8], mut f: impl FnMut(&[u8], &[u8], &[u8])) {
    for line in headers.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
        if !line[..colon].eq_ignore_ascii_case(b"requires-dist") {
            continue;
        }
        let val = line[colon + 1..].trim_ascii();
        let (req, marker) = match val.iter().position(|&b| b == b';') {
            Some(i) => (&val[..i], val[i + 1..].trim_ascii()),
            None => (val, &b""[..]),
        };
        let Ok(req) = std::str::from_utf8(req) else { continue };
        if let Some((name, extras)) = super::declared::parse_requirement(req) {
            f(name.as_bytes(), extras.as_bytes(), marker);
        }
    }
}

/// The values compared with `extra` in a PEP 508 marker (`extra == "socks"`, also
/// `'socks' == extra`), unnormalised.
pub fn marker_extras(marker: &[u8], mut f: impl FnMut(&[u8])) {
    let mut i = 0;
    while let Some(at) = find(&marker[i..], b"extra") {
        let s = i + at;
        i = s + 5;
        // `extra` as a whole word.
        let before = s.checked_sub(1).map(|j| marker[j]);
        let after = marker.get(i).copied();
        let word = |c: Option<u8>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_');
        if word(before) || word(after) {
            continue;
        }
        // Forward: `extra == "x"`.
        let rest = marker[i..].trim_ascii_start();
        if let Some(rest) = rest.strip_prefix(b"==")
            && let Some(v) = quoted(rest.trim_ascii_start())
        {
            f(v);
            continue;
        }
        // Backward: `"x" == extra`.
        let head = marker[..s].trim_ascii_end();
        if let Some(head) = head.strip_suffix(b"==") {
            let head = head.trim_ascii_end();
            if let Some(&q) = head.last()
                && (q == b'"' || q == b'\'')
                && let Some(open) = head[..head.len() - 1].iter().rposition(|&c| c == q)
            {
                f(&head[open + 1..head.len() - 1]);
            }
        }
    }
}

fn quoted(s: &[u8]) -> Option<&[u8]> {
    let q = *s.first()?;
    if q != b'"' && q != b'\'' {
        return None;
    }
    let end = s[1..].iter().position(|&c| c == q)?;
    Some(&s[1..1 + end])
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Parsed requirements of the distributions asked so far (each `METADATA` read at most once).
#[derive(Default)]
pub struct RequiresCache {
    map: FxHashMap<DistId, Vec<Requirement>>,
    reads: u32,
    buf: Vec<u8>,
    scratch: Vec<u8>,
}

impl RequiresCache {
    /// Metadata files read (or attempted) so far.
    pub fn reads(&self) -> u32 {
        self.reads
    }

    /// The requirements of `d`, reading its metadata on first use (unreadable: none).
    pub fn get(&mut self, dists: &Distributions, d: DistId, names: &mut Interner) -> &[Requirement] {
        if !self.map.contains_key(&d) {
            self.reads += 1;
            let reqs = self.read(dists, d, names);
            self.map.insert(d, reqs);
        }
        &self.map[&d]
    }

    fn read(&mut self, dists: &Distributions, d: DistId, names: &mut Interner) -> Vec<Requirement> {
        let info = dists.info_path(d);
        let path = if dists.is_egg_info(d) {
            if info.is_file() { info } else { info.join("PKG-INFO") }
        } else {
            info.join("METADATA")
        };
        if read_headers(&path, &mut self.buf).is_err() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let scratch = &mut self.scratch;
        for_each_requires_dist(&self.buf, |name, extras, marker| {
            let mut norm = |s: &[u8], names: &mut Interner| {
                normalize_name(s, scratch);
                names.intern(scratch)
            };
            let mut r = Requirement { norm: norm(name, names), extras: Vec::new(), when_extra: Vec::new() };
            for e in extras.split(|&b| b == b',').map(<[u8]>::trim_ascii).filter(|e| !e.is_empty()) {
                r.extras.push(norm(e, names));
            }
            marker_extras(marker, |e| r.when_extra.push(norm(e, names)));
            out.push(r);
        });
        out
    }
}

/// Reads the header block of a metadata file into `buf`: a prefix, or the whole file (up to
/// [`MAX`]) when the headers do not end within it.
fn read_headers(path: &std::path::Path, buf: &mut Vec<u8>) -> std::io::Result<()> {
    buf.clear();
    let mut f = File::open(path)?;
    (&mut f).take(PREFIX).read_to_end(buf)?;
    if (buf.len() as u64) == PREFIX && find(buf, b"\n\n").is_none() && find(buf, b"\r\n\r\n").is_none() {
        f.take(MAX - PREFIX).read_to_end(buf)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: &[u8]) -> String {
        String::from_utf8_lossy(b).into_owned()
    }

    #[test]
    fn requires_dist_headers() {
        let meta = b"Metadata-Version: 2.1\nName: requests\nRequires-Dist: charset_normalizer<4,>=2\n\
requires-dist: urllib3 (<3,>=1.21.1)\nRequires-Dist: PySocks!=1.5.7,>=1.5.6; extra == \"socks\"\n\
Requires-Dist: foo[bar, baz] ; python_version < \"3.9\"\n\nRequires-Dist: body-is-ignored\n";
        let mut got = Vec::new();
        for_each_requires_dist(meta, |n, e, m| got.push((s(n), s(e), s(m))));
        let want = [
            ("charset_normalizer", "", ""),
            ("urllib3", "", ""),
            ("PySocks", "", "extra == \"socks\""),
            ("foo", "bar, baz", "python_version < \"3.9\""),
        ];
        let got: Vec<(&str, &str, &str)> = got.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str())).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn marker_extra_values() {
        let mut got = Vec::new();
        marker_extras(b"extra == \"socks\" or extra=='Use-Chardet' or \"x\" == extra", |e| got.push(s(e)));
        assert_eq!(got, ["socks", "Use-Chardet", "x"]);
        got.clear();
        marker_extras(b"python_version < \"3.9\" and platform_extra == \"no\"", |e| got.push(s(e)));
        assert!(got.is_empty());
        marker_extras(b"", |e| got.push(s(e)));
        assert!(got.is_empty());
    }
}
