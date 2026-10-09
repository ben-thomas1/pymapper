//! Qualified ids: rendering into a caller buffer and lookup.

use super::{Index, NodeId, NodeKind};

/// Appends `#{dup + 1}` for duplicates.
fn push_dup(out: &mut Vec<u8>, dup: u16) {
    if dup == 0 {
        return;
    }
    let mut n = u32::from(dup) + 1;
    let mut digits = [0u8; 5];
    let mut i = digits.len();
    while n > 0 {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    out.push(b'#');
    out.extend_from_slice(&digits[i..]);
}

/// Splits a `name#N` segment into the name and the duplicate ordinal (`N - 1`). `None` for a
/// malformed suffix (`#`, `#0`, `#1`, non-digits, overflow).
pub(crate) fn split_dup(seg: &[u8]) -> Option<(&[u8], u16)> {
    let Some(h) = seg.iter().rposition(|&c| c == b'#') else { return Some((seg, 0)) };
    let digits = &seg[h + 1..];
    if digits.is_empty() || digits[0] == b'0' || !digits.iter().all(u8::is_ascii_digit) || digits.len() > 5 {
        return None;
    }
    let n: u32 = digits.iter().fold(0, |a, &c| a * 10 + u32::from(c - b'0'));
    if n < 2 || n > u32::from(u16::MAX) + 1 {
        return None;
    }
    Some((&seg[..h], (n - 1) as u16))
}

impl Index {
    /// Appends the qualified id of `v` to `out` (see the module docs): `@dist`, `pkg.mod`, or
    /// `pkg.mod:Class.method`, with `#N` suffixes on duplicates. No allocation beyond `out`.
    pub fn qid(&self, v: NodeId, out: &mut Vec<u8>) {
        let kind = self.kind(v);
        if kind == NodeKind::Distribution {
            out.push(b'@');
            out.extend_from_slice(self.name(v));
            push_dup(out, self.dup(v));
            return;
        }
        // The package/module part: the node itself, or the module of a def.
        let m = match self.file(v) {
            Some(f) if kind.is_def() => self.file_node(f),
            _ => v,
        };
        let dm = self.depth(m);
        for k in 1..=dm {
            if k > 1 {
                out.push(b'.');
            }
            out.extend_from_slice(self.name(self.ancestor_at(m, k)));
        }
        push_dup(out, self.dup(m));
        if m == v {
            return;
        }
        out.push(b':');
        for k in dm + 1..=self.depth(v) {
            if k > dm + 1 {
                out.push(b'.');
            }
            let a = self.ancestor_at(v, k);
            out.extend_from_slice(self.name(a));
            push_dup(out, self.dup(a));
        }
    }

    /// The qualified id of `v` as a new buffer (convenience; prefer [`Index::qid`] in loops).
    pub fn qid_string(&self, v: NodeId) -> String {
        let mut out = Vec::new();
        self.qid(v, &mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The package/module node of a dotted name (`dup` 0 = the primary).
    pub fn module_node(&self, dotted: &[u8], dup: u16) -> Option<NodeId> {
        let s = self.strings.get(dotted)?;
        self.modules.get(&(s, dup)).copied()
    }

    /// The node with qualified id `qid`, if any. One hash lookup for the module part, then a
    /// scan of the children at each def segment.
    pub fn find(&self, qid: &[u8]) -> Option<NodeId> {
        if let Some(name) = qid.strip_prefix(b"@") {
            let (name, dup) = split_dup(name)?;
            return self
                .group_node
                .iter()
                .copied()
                .find(|&g| self.name(g) == name && self.dup(g) == dup);
        }
        let (module, defs) = match qid.iter().position(|&c| c == b':') {
            Some(i) => (&qid[..i], Some(&qid[i + 1..])),
            None => (qid, None),
        };
        let (dotted, dup) = split_dup(module)?;
        let mut cur = self.module_node(dotted, dup)?;
        let Some(defs) = defs else { return Some(cur) };
        for seg in defs.split(|&c| c == b'.') {
            let (name, dup) = split_dup(seg)?;
            let sym = self.strings.get(name)?;
            cur = self
                .children(cur)
                .find(|&c| self.kind(c).is_def() && self.name_sym(c) == sym && self.dup(c) == dup)?;
        }
        Some(cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dup_suffix_round_trips() {
        for dup in [0u16, 1, 9, 10, 98, 99, 1000, u16::MAX] {
            let mut out = b"f".to_vec();
            push_dup(&mut out, dup);
            assert_eq!(split_dup(&out), Some((&b"f"[..], dup)), "{}", String::from_utf8_lossy(&out));
        }
    }

    #[test]
    fn malformed_suffixes_are_rejected() {
        for bad in [&b"f#"[..], b"f#0", b"f#1", b"f#01", b"f#x", b"f#65537", b"f#999999"] {
            assert_eq!(split_dup(bad), None, "{}", String::from_utf8_lossy(bad));
        }
        assert_eq!(split_dup(b"f"), Some((&b"f"[..], 0)));
    }
}
