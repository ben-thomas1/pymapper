//! ANSI styling for the human format (design: ISSUES O1 rule 10): plain SGR escapes, no
//! terminal library. Whether to style at all is decided once ([`super::Options::color`]);
//! every helper here is a no-op when styling is off, so writers call them unconditionally.

/// Ends every style.
const RESET: &[u8] = b"\x1b[0m";
/// File paths.
pub(super) const BOLD: &[u8] = b"\x1b[1m";
/// Line:col, ids, codes and the low-confidence suffix.
pub(super) const DIM: &[u8] = b"\x1b[2m";
/// Architecture (import cycles, contracts).
pub(super) const RED: &[u8] = b"\x1b[1;31m";
/// Dependencies.
pub(super) const CYAN: &[u8] = b"\x1b[1;36m";
/// Complexity.
pub(super) const MAGENTA: &[u8] = b"\x1b[1;35m";
/// Coupling.
pub(super) const MAGENTA_DIM: &[u8] = b"\x1b[2;35m";
/// Duplication.
pub(super) const BLUE: &[u8] = b"\x1b[1;34m";
/// Dead code.
pub(super) const YELLOW: &[u8] = b"\x1b[1;33m";
/// Unused imports.
pub(super) const YELLOW_DIM: &[u8] = b"\x1b[2;33m";

/// Styling on or off.
#[derive(Clone, Copy, Debug)]
pub(super) struct Style {
    pub(super) on: bool,
}

impl Style {
    /// Starts `sgr` (one of the constants above).
    #[inline]
    pub(super) fn open(self, buf: &mut Vec<u8>, sgr: &[u8]) {
        if self.on {
            buf.extend_from_slice(sgr);
        }
    }

    /// Ends the current style.
    #[inline]
    pub(super) fn close(self, buf: &mut Vec<u8>) {
        if self.on {
            buf.extend_from_slice(RESET);
        }
    }

    /// `text` in style `sgr`.
    #[inline]
    pub(super) fn paint(self, buf: &mut Vec<u8>, sgr: &[u8], text: &[u8]) {
        self.open(buf, sgr);
        buf.extend_from_slice(text);
        self.close(buf);
    }
}

/// Display width of `bytes` in columns: characters, not counting `ESC [ ... m` sequences.
/// Every character counts as one column (the output is ASCII plus `·` and `↔`, and names are
/// mostly ASCII; wide characters only shift the id column of their own line).
pub(super) fn width(bytes: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'm' {
                i += 1;
            }
            i += 1;
            continue;
        }
        // Count UTF-8 lead bytes (and invalid bytes), not continuation bytes.
        if b & 0xc0 != 0x80 {
            n += 1;
        }
        i += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_skips_escapes_and_counts_characters() {
        assert_eq!(width(b"abc"), 3);
        assert_eq!(width("a · b ↔ c".as_bytes()), 9);
        assert_eq!(width(b"\x1b[1;31mArchitecture\x1b[0m  2"), 15);
        assert_eq!(width(b""), 0);
    }

    #[test]
    fn off_writes_nothing() {
        let mut b = Vec::new();
        Style { on: false }.paint(&mut b, RED, b"x");
        assert_eq!(b, b"x");
        Style { on: true }.paint(&mut b, DIM, b"y");
        assert_eq!(b, b"x\x1b[2my\x1b[0m");
    }
}
