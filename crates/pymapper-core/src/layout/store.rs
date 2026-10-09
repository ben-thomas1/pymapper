//! On-disk layout cache: the computed levels of one (tree, config) pair.
//!
//! ```text
//!   <dir>/layout-<key:016x>.pml          key = hash(format version, tree hash, config fingerprint)
//!
//!   offset  size  field
//!   0       8     magic "PMLAYOUT"
//!   8       4     format version (u32 LE)
//!   12      8     key (u64 LE)
//!   20      4     node count of the tree (u32 LE)
//!   24      4     levels stored, L (u32 LE)
//!   28      ...   L times: rect count (u32 LE), then x0 y0 x1 y1 (f32 LE) per rect
//!   end-8   8     checksum (u64 LE) over the level counts and rect bytes
//! ```
//!
//! Written atomically (temp file + rename). Reading streams one level at a time through a
//! reused buffer, so the only large allocation is the levels themselves. Anything that does
//! not validate (magic, version, key, node count, level lengths against the tree, checksum)
//! is an error and the caller recomputes: the cache never changes what is laid out.

use std::fs::File;
use std::hash::Hasher;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{LayoutConfig, LayoutTree, Rect};
use crate::intern::{FxHasher, hash_bytes};

const MAGIC: &[u8; 8] = b"PMLAYOUT";
/// Bump when the format or the layout algorithm changes what is produced.
const VERSION: u32 = 1;
const HEADER: usize = 28;

/// Why a cached layout was not used.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("layout cache I/O: {0}")]
    Io(#[from] io::Error),
    #[error("layout cache invalid: {0}")]
    Invalid(&'static str),
}

/// The cache key of a tree laid out with a config.
pub fn cache_key(tree: &LayoutTree, cfg: &LayoutConfig) -> u64 {
    let mut h = FxHasher::default();
    h.write_u32(VERSION);
    h.write_u64(tree.hash());
    h.write_u64(cfg.fingerprint());
    h.finish()
}

/// The cache file of a tree laid out with a config, in `dir`.
pub fn cache_path(dir: &Path, tree: &LayoutTree, cfg: &LayoutConfig) -> PathBuf {
    let mut name = String::with_capacity(28);
    name.push_str("layout-");
    let key = cache_key(tree, cfg);
    for i in (0..16).rev() {
        let d = ((key >> (i * 4)) & 0xf) as u8;
        name.push(char::from(if d < 10 { b'0' + d } else { b'a' + d - 10 }));
    }
    name.push_str(".pml");
    dir.join(name)
}

#[inline]
fn fold(acc: u64, h: u64) -> u64 {
    (acc.rotate_left(5) ^ h).wrapping_mul(0x517c_c1b7_2722_0a95)
}

/// Writes the levels of `tree` (laid out with `cfg`) to `path` atomically, creating its
/// directory if needed.
pub fn save_levels(path: &Path, tree: &LayoutTree, cfg: &LayoutConfig, levels: &[Arc<[Rect]>]) -> Result<(), StoreError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let key = cache_key(tree, cfg);
    crate::cache::write_atomic_with(path, |mut f| {
        let mut head = [0u8; HEADER];
        head[..8].copy_from_slice(MAGIC);
        head[8..12].copy_from_slice(&VERSION.to_le_bytes());
        head[12..20].copy_from_slice(&key.to_le_bytes());
        head[20..24].copy_from_slice(&(tree.len() as u32).to_le_bytes());
        head[24..28].copy_from_slice(&(levels.len() as u32).to_le_bytes());
        f.write_all(&head)?;
        let mut sum = 0u64;
        let mut buf = Vec::new();
        for lv in levels {
            let cnt = (lv.len() as u32).to_le_bytes();
            f.write_all(&cnt)?;
            sum = fold(sum, hash_bytes(&cnt));
            buf.clear();
            buf.reserve(lv.len() * 16);
            for r in lv.iter() {
                for v in [r.x0, r.y0, r.x1, r.y1] {
                    buf.extend_from_slice(&v.to_le_bytes());
                }
            }
            sum = fold(sum, hash_bytes(&buf));
            f.write_all(&buf)?;
        }
        f.write_all(&sum.to_le_bytes())?;
        f.flush()
    })?;
    Ok(())
}

/// Reads the levels stored for `tree` laid out with `cfg`. Every level must have the length
/// of the tree's level of the same depth.
pub fn load_levels(path: &Path, tree: &LayoutTree, cfg: &LayoutConfig) -> Result<Vec<Arc<[Rect]>>, StoreError> {
    let mut f = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut head = [0u8; HEADER];
    f.read_exact(&mut head)?;
    if &head[..8] != MAGIC {
        return Err(StoreError::Invalid("magic"));
    }
    let word = |o: usize| u32::from_le_bytes([head[o], head[o + 1], head[o + 2], head[o + 3]]);
    if word(8) != VERSION {
        return Err(StoreError::Invalid("version"));
    }
    let mut k = [0u8; 8];
    k.copy_from_slice(&head[12..20]);
    if u64::from_le_bytes(k) != cache_key(tree, cfg) {
        return Err(StoreError::Invalid("key"));
    }
    if word(20) as usize != tree.len() {
        return Err(StoreError::Invalid("node count"));
    }
    let n = word(24);
    if n > tree.levels() {
        return Err(StoreError::Invalid("level count"));
    }
    let mut levels = Vec::with_capacity(n as usize);
    let mut sum = 0u64;
    let mut buf = Vec::new();
    for d in 0..n {
        let mut cnt = [0u8; 4];
        f.read_exact(&mut cnt)?;
        let len = u32::from_le_bytes(cnt) as usize;
        if len != tree.level(d).len() {
            return Err(StoreError::Invalid("level length"));
        }
        sum = fold(sum, hash_bytes(&cnt));
        buf.clear();
        buf.resize(len * 16, 0);
        f.read_exact(&mut buf)?;
        sum = fold(sum, hash_bytes(&buf));
        let mut lv = super::engine::new_level(len);
        let rects = Arc::get_mut(&mut lv).ok_or(StoreError::Invalid("shared level"))?;
        for (c, r) in buf.as_chunks::<16>().0.iter().zip(rects.iter_mut()) {
            let v = |o: usize| f32::from_le_bytes([c[o], c[o + 1], c[o + 2], c[o + 3]]);
            *r = Rect::new(v(0), v(4), v(8), v(12));
        }
        levels.push(lv);
    }
    let mut tail = [0u8; 8];
    f.read_exact(&mut tail)?;
    if u64::from_le_bytes(tail) != sum {
        return Err(StoreError::Invalid("checksum"));
    }
    if f.read(&mut tail)? != 0 {
        return Err(StoreError::Invalid("trailing bytes"));
    }
    Ok(levels)
}
