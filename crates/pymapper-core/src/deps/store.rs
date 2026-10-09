//! Cached scorecards: one file, `scores.pmc`, in the cache directory.
//!
//! ```text
//!   container (cache/format.rs): kind 2, schema = hash(cache schema, SCORE_VERSION, WORDS)
//!   section 0   u64 x n     distribution cache keys, ascending
//!   section 1   u32 x 22n   the scorecards, Scorecard::to_words each
//! ```
//!
//! A distribution's key is the key of its facts segment (normalized name, version and the hash
//! of its `RECORD`, see [`crate::cache`]), so a scorecard is reused exactly when the
//! distribution's files are trusted from the cache, and never for editable installs or
//! distributions without a `RECORD` (they have no key and are scored on every run). The file
//! is rewritten (atomically) only when a scorecard was computed or an entry is no longer used
//! by this load; a missing, foreign or corrupt file is ignored (everything is recomputed).
//! The whole file is a few kilobytes (100 bytes per distribution).

use std::io;

use crate::cache::{self, Cache, CacheError, Plan, Reader, Writer, write_atomic_with};
use crate::index::Group;
use crate::intern::{FxHashMap, FxHasher};
use crate::roots::{Origin, Roots};

use super::score::{SCORE_VERSION, Scorecard, WORDS};

/// File name in the cache directory.
pub const SCORES_FILE: &str = "scores.pmc";

/// Container kind of the scores file.
const KIND_SCORES: u32 = 2;
/// Container key of the scores file (one per cache directory).
const KEY: u64 = 0x5343_4f52_4553_0001;

/// Hash of everything a stored scorecard depends on besides its distribution.
fn schema() -> u64 {
    let mut h = FxHasher::default();
    std::hash::Hasher::write_u64(&mut h, cache::schema());
    std::hash::Hasher::write_u32(&mut h, SCORE_VERSION);
    std::hash::Hasher::write_usize(&mut h, WORDS);
    std::hash::Hasher::finish(&h)
}

/// What the store did over one run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScoreStats {
    /// Scorecards read from the file.
    pub cached: u32,
    /// Scorecards computed (missing, stale, or without a key).
    pub computed: u32,
    /// The file was rewritten.
    pub written: bool,
}

/// Scorecards by distribution key, loaded from and saved to the cache directory.
#[derive(Default)]
pub struct ScoreStore {
    /// Key -> (scorecard, used by this run).
    entries: FxHashMap<u64, (Scorecard, bool)>,
    dirty: bool,
    stats: ScoreStats,
}

impl ScoreStore {
    /// The scorecards stored in `cache` (empty without a cache or a valid file).
    pub fn open(cache: Option<&Cache>) -> ScoreStore {
        let mut st = ScoreStore::default();
        if let Some(c) = cache
            && cfg!(target_endian = "little")
            && let Ok(entries) = read(&c.dir().join(SCORES_FILE))
        {
            st.entries = entries;
        }
        st
    }

    /// The cache key of a group's scorecard: that of its facts segment, for third-party
    /// distributions whose files are trusted from the cache; `None` otherwise.
    pub fn key(group: &Group, roots: &Roots) -> Option<u64> {
        if group.origin != Origin::ThirdParty {
            return None;
        }
        let plan = Plan::for_group(group, Some(roots));
        (plan.mode == cache::Mode::Trusted).then_some(plan.key)
    }

    /// The stored scorecard of `key` (marked used), or `None`.
    pub fn get(&mut self, key: u64) -> Option<Scorecard> {
        let (sc, used) = self.entries.get_mut(&key)?;
        *used = true;
        self.stats.cached += 1;
        Some(*sc)
    }

    /// Records a computed scorecard (`key`: `None` for distributions that are not cached).
    pub fn put(&mut self, key: Option<u64>, sc: Scorecard) {
        self.stats.computed += 1;
        if let Some(k) = key {
            self.entries.insert(k, (sc, true));
            self.dirty = true;
        }
    }

    /// The scorecard of `key`, computing (and recording) it with `compute` when missing.
    pub fn get_or_compute(&mut self, key: Option<u64>, compute: impl FnOnce() -> Scorecard) -> Scorecard {
        if let Some(sc) = key.and_then(|k| self.get(k)) {
            return sc;
        }
        let sc = compute();
        self.put(key, sc);
        sc
    }

    pub fn stats(&self) -> ScoreStats {
        self.stats
    }

    /// Writes the used entries to `cache` when one was computed or an unused one is dropped.
    /// Errors are returned for a warning; the store is only an optimisation.
    pub fn save(&mut self, cache: &Cache) -> io::Result<()> {
        let stale = self.entries.values().any(|&(_, used)| !used);
        if !(self.dirty || stale) || !cfg!(target_endian = "little") {
            return Ok(());
        }
        let mut rows: Vec<(u64, &Scorecard)> =
            self.entries.iter().filter(|(_, (_, used))| *used).map(|(&k, (sc, _))| (k, sc)).collect();
        rows.sort_unstable_by_key(|r| r.0);
        cache.ensure_dir()?;
        let keys: Vec<u64> = rows.iter().map(|r| r.0).collect();
        let mut words: Vec<u32> = Vec::with_capacity(rows.len() * WORDS);
        for (_, sc) in &rows {
            words.extend_from_slice(&sc.to_words());
        }
        let path = cache.dir().join(SCORES_FILE);
        write_atomic_with(&path, |out| {
            let mut w = Writer::new(out);
            w.put(0, &keys);
            w.put(1, &words);
            w.finish(KIND_SCORES, schema(), KEY).map(|_| ())
        })?;
        self.stats.written = true;
        self.dirty = false;
        Ok(())
    }
}

/// Reads and validates a scores file.
fn read(path: &std::path::Path) -> Result<FxHashMap<u64, (Scorecard, bool)>, CacheError> {
    let r = Reader::open(path, KIND_SCORES, schema(), KEY)?;
    let keys: &[u64] = r.get(0)?;
    let words: &[u32] = r.get(1)?;
    if words.len() != keys.len() * WORDS {
        return Err(CacheError::Corrupt("scorecards"));
    }
    let mut out = FxHashMap::default();
    out.reserve(keys.len());
    let (chunks, _) = words.as_chunks::<WORDS>();
    for (k, w) in keys.iter().zip(chunks) {
        out.insert(*k, (Scorecard::from_words(w), false));
    }
    Ok(out)
}
