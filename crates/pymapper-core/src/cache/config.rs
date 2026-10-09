//! Cache of config-file evidence ([`crate::extract::config`]): one file,
//! `config-evidence.pmc`, holding per config file its path, size, mtime and hashes.
//!
//! ```text
//!   magic "PMCFGEV\0" | version u32 | entries u32
//!   per entry: path_len u32 | path bytes | size u64 | mtime_ns u64 | n u32 | n x hash u32
//! ```
//!
//! A file whose size and mtime match its entry reuses the hashes; others are re-read. The cache
//! file is rewritten (atomically) only when an entry changed, appeared or disappeared. A
//! corrupt or foreign file is ignored (everything is re-read).

use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use super::Cache;
use super::format::write_atomic;
use crate::extract::config::{ConfigEvidence, FileTokens, MAX_CONFIG_BYTES, tokens_of};
use crate::intern::FxHashMap;
use crate::walk::PathList;

const MAGIC: &[u8; 8] = b"PMCFGEV\0";
/// Bump when the tokenizer's output changes.
const VERSION: u32 = 1;
/// File name in the cache directory.
pub const CONFIG_EVIDENCE_FILE: &str = "config-evidence.pmc";

/// A parsed cache file: path -> (size, mtime, hashes).
struct Entries<'a> {
    map: FxHashMap<&'a [u8], (u64, u64, &'a [u8])>,
}

fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn parse(b: &[u8]) -> Option<Entries<'_>> {
    if b.get(..8)? != MAGIC || le_u32(b, 8)? != VERSION {
        return None;
    }
    let n = le_u32(b, 12)? as usize;
    let mut map = FxHashMap::default();
    let mut at = 16;
    for _ in 0..n {
        let pl = le_u32(b, at)? as usize;
        let path = b.get(at + 4..at + 4 + pl)?;
        at += 4 + pl;
        let size = le_u64(b, at)?;
        let mtime = le_u64(b, at + 8)?;
        let k = le_u32(b, at + 16)? as usize;
        let hashes = b.get(at + 20..at + 20 + k * 4)?;
        at += 20 + k * 4;
        map.insert(path, (size, mtime, hashes));
    }
    Some(Entries { map })
}

/// Size and mtime (ns since the epoch) of `path`.
fn stat(path: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    let mtime = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos() as u64;
    Some((m.len(), mtime))
}

/// One file's result plus what to store for it.
struct Got {
    tokens: FileTokens,
    stat: Option<(u64, u64)>,
}

/// Collects the config evidence of `paths` through the cache in `cache` (or without one).
/// Write errors are ignored: the cache is an optimisation.
pub fn config_evidence(paths: &PathList, cache: Option<&Cache>) -> ConfigEvidence {
    let Some(cache) = cache else { return ConfigEvidence::collect(paths) };
    let file = cache.dir().join(CONFIG_EVIDENCE_FILE);
    let bytes = std::fs::read(&file).unwrap_or_default();
    let old = parse(&bytes);
    let got: Vec<Got> = (0..paths.len())
        .into_par_iter()
        .map(|i| {
            let p = paths.get(i);
            let Some((size, mtime)) = stat(p) else { return Got { tokens: FileTokens::Unreadable, stat: None } };
            if size > MAX_CONFIG_BYTES {
                return Got { tokens: FileTokens::TooLarge, stat: None };
            }
            if let Some(&(s, m, h)) = old.as_ref().and_then(|o| o.map.get(p.as_os_str().as_bytes()))
                && s == size
                && m == mtime
            {
                let hashes = h.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
                return Got { tokens: FileTokens::Cached(hashes), stat: Some((size, mtime)) };
            }
            let tokens = match std::fs::read(p) {
                Ok(src) => tokens_of(p, &src),
                Err(_) => FileTokens::Unreadable,
            };
            Got { tokens, stat: Some((size, mtime)) }
        })
        .collect();
    let stored = |g: &Got| g.stat.is_some() && matches!(g.tokens, FileTokens::Hashes(..) | FileTokens::Cached(_));
    let dirty = got.iter().any(|g| matches!(g.tokens, FileTokens::Hashes(..)))
        || old.as_ref().is_none_or(|o| o.map.len() != got.iter().filter(|g| stored(g)).count());
    if dirty {
        let mut out = Vec::with_capacity(bytes.len().max(1024));
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(got.iter().filter(|g| stored(g)).count() as u32).to_le_bytes());
        for (i, g) in got.iter().enumerate() {
            let (Some((size, mtime)), FileTokens::Hashes(h, _) | FileTokens::Cached(h)) = (g.stat, &g.tokens) else { continue };
            let p = paths.get_bytes(i);
            out.extend_from_slice(&(p.len() as u32).to_le_bytes());
            out.extend_from_slice(p);
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&mtime.to_le_bytes());
            out.extend_from_slice(&(h.len() as u32).to_le_bytes());
            for &x in h {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        if cache.ensure_dir().is_ok() {
            let _ = write_atomic(&file, &out);
        }
    }
    drop(old);
    let mut ev = ConfigEvidence::new();
    for g in got {
        ev.add(g.tokens);
    }
    ev
}
