//! Stable finding ids: `PM401-3fa2` (design: ISSUES O5).
//!
//! # Derivation
//!
//! The id of a finding is its rule code, `-`, then a prefix of the lower-case hex rendering of
//! [`stable_hash`] of its baseline key ([`KeyBuf`]: rule + stable id + stable message
//! arguments, never numbers or lines). So an id survives line moves, metric changes and
//! re-runs, and is the same with or without the cache and from any working directory.
//!
//! - **Length:** 4 hex digits, extended per finding (up to 16) until it differs from every other
//!   finding of the same rule in the report. Only the findings that collide get longer ids;
//!   the others are unaffected.
//! - **Equal keys** (e.g. the same name imported twice in one module) are told apart by their
//!   occurrence in output order: the n-th repeat (n >= 1) hashes the key followed by n, so the
//!   first keeps the plain key's id.
//!
//! # Lookup
//!
//! [`FindingIds::find`] accepts any prefix of at least 4 hex digits that matches exactly one
//! finding of the rule, so an id copied from a run with other filters (and so possibly a
//! different length) still resolves.

use std::fmt;

use super::Report;
use super::baseline::KeyBuf;
use super::buf::push_code;
use crate::intern::FxHashMap;

/// Hex digits of an id unless longer ones are needed to tell findings apart.
pub const MIN_ID_DIGITS: u8 = 4;

/// FNV-1a, 64 bit. Ids are shown to users and stored in agent transcripts, so this function
/// must never change: it does not depend on the Rust release, the platform or [`crate::intern`]
/// hashing (which may be tuned for speed).
pub fn stable_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// One finding's id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FindingId {
    /// Rule code (`401` for PM401).
    pub code: u16,
    /// Full hash; the id shows its top `digits` hex digits.
    pub hash: u64,
    /// Hex digits shown (4..=16).
    pub digits: u8,
}

impl FindingId {
    /// Appends `PMxxx-hhhh` to `out`.
    pub fn push(&self, out: &mut Vec<u8>) {
        push_code(out, self.code);
        out.push(b'-');
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for k in 0..u32::from(self.digits) {
            out.push(HEX[((self.hash >> (60 - 4 * k)) & 0xf) as usize]);
        }
    }
}

impl fmt::Display for FindingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut b = Vec::with_capacity(24);
        self.push(&mut b);
        // ASCII by construction.
        f.write_str(std::str::from_utf8(&b).unwrap_or("?"))
    }
}

/// The ids of every finding of a [`Report`], by output position (see [`Report::ids`]).
#[derive(Clone, Debug, Default)]
pub struct FindingIds {
    ids: Vec<FindingId>,
}

impl FindingIds {
    pub(super) fn build(r: &Report<'_>) -> FindingIds {
        let mut keys = KeyBuf::new();
        // Occurrences of each (code, key hash) so far, to tell equal keys apart.
        let mut seen: FxHashMap<(u16, u64), u32> = FxHashMap::default();
        let mut ids = Vec::with_capacity(r.len());
        let mut scratch = Vec::with_capacity(super::SCRATCH);
        for f in r.iter() {
            let key = keys.build(r, f);
            let base = stable_hash(key);
            let n = seen.entry((f.rule.code(), base)).or_insert(0);
            let hash = if *n == 0 {
                base
            } else {
                scratch.clear();
                scratch.extend_from_slice(key);
                scratch.extend_from_slice(&n.to_le_bytes());
                stable_hash(&scratch)
            };
            *n += 1;
            ids.push(FindingId { code: f.rule.code(), hash, digits: MIN_ID_DIGITS });
        }
        extend(&mut ids);
        FindingIds { ids }
    }

    /// Ids from (code, hash) pairs, extended where they collide (tests).
    #[cfg(test)]
    pub(super) fn from_hashes(pairs: &[(u16, u64)]) -> FindingIds {
        let mut ids: Vec<FindingId> = pairs.iter().map(|&(code, hash)| FindingId { code, hash, digits: MIN_ID_DIGITS }).collect();
        extend(&mut ids);
        FindingIds { ids }
    }

    /// Id of the finding at output position `pos` (the n-th of [`Report::iter`]).
    #[inline]
    pub fn get(&self, pos: usize) -> FindingId {
        self.ids[pos]
    }

    /// Number of ids (= findings in the report).
    #[inline]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether there are none.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Output position of the finding `text` names: `PMxxx-` and at least
    /// [`MIN_ID_DIGITS`] hex digits that prefix exactly one finding's hash (case-insensitive).
    pub fn find(&self, text: &str) -> Result<usize, FindError> {
        let (code, hex) = parse(text).ok_or(FindError::Malformed)?;
        let digits = hex.len() as u32;
        let mut prefix = 0u64;
        for c in hex.bytes() {
            prefix = (prefix << 4) | u64::from((c as char).to_digit(16).unwrap_or(0));
        }
        let shift = 64 - 4 * digits;
        let mut found = None;
        for (pos, id) in self.ids.iter().enumerate() {
            if id.code == code && (shift == 64 || id.hash >> shift == prefix) {
                if found.is_some() {
                    return Err(FindError::Ambiguous);
                }
                found = Some(pos);
            }
        }
        found.ok_or(FindError::NotFound)
    }
}

/// Per rule, sorted by hash: each id gets one digit more than it shares with either neighbour
/// (equal hashes do not occur: equal keys are told apart before hashing).
fn extend(ids: &mut [FindingId]) {
    let mut order: Vec<u32> = (0..ids.len() as u32).collect();
    order.sort_unstable_by_key(|&i| (ids[i as usize].code, ids[i as usize].hash));
    for w in order.windows(2) {
        let (a, b) = (ids[w[0] as usize], ids[w[1] as usize]);
        if a.code != b.code {
            continue;
        }
        let shared = ((a.hash ^ b.hash).leading_zeros() / 4) as u8;
        let need = (shared + 1).clamp(MIN_ID_DIGITS, 16);
        for i in [w[0], w[1]] {
            let d = &mut ids[i as usize].digits;
            *d = (*d).max(need);
        }
    }
}

/// Why [`FindingIds::find`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FindError {
    /// Not `PMxxx-` followed by 4 to 16 hex digits.
    #[error("not a finding id (expected e.g. PM401-3fa2)")]
    Malformed,
    /// No finding has this id (the code changed, or other options hide it).
    #[error("no finding has this id")]
    NotFound,
    /// Several findings start with these digits (use the full id).
    #[error("several findings match this id; use the full id")]
    Ambiguous,
}

/// `PMxxx-hhhh..` into the code and the hex digits.
fn parse(text: &str) -> Option<(u16, &str)> {
    let rest = text.strip_prefix("PM").or_else(|| text.strip_prefix("pm"))?;
    let (code, hex) = rest.split_once('-')?;
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !(usize::from(MIN_ID_DIGITS)..=16).contains(&hex.len()) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((code.parse().ok()?, hex))
}
