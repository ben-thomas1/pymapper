//! On-disk cache of extracted facts (design: Loading / index): our own flat little-endian
//! format, a versioned header and checksum, zero-copy `mmap` readers and atomic writes.
//!
//! ```text
//!   .pymapper_cache/                 (config `cache-dir`; created on first write)
//!     .gitignore                     "*": the cache is never committed
//!     files-<hash>.pmc               first-party (and loose third-party) files, one per group
//!     stdlib-3.13-<hash>.pmc         the stdlib, one per interpreter (version + install)
//!     dist-<name>@<version>@<hash>.pmc   one per third-party distribution
//! ```
//!
//! Each file is a facts segment ([`segment`]): the facts and metrics of a set of files with a
//! local string table, laid out as the in-memory columns, so loading a cached file is a copy
//! of its rows (no parse). What makes an entry valid depends on the group:
//!
//! - First-party (and third-party files without a `RECORD`, editable installs): per file, the
//!   stored path, size and mtime must match a `stat` of the file; failing that (same size,
//!   other mtime: a checkout, a `touch`) the file is read and its content hash compared. Only
//!   files that really changed are extracted again.
//! - A third-party distribution: the file name holds the normalized name, version and the
//!   hash of its `RECORD`; with those equal the files are trusted without any `stat` or read.
//!   The file list itself comes from `RECORD` ([`crate::roots::Source::Record`]), so a cached
//!   distribution needs neither a walk nor a read.
//! - The stdlib: per interpreter version and install (path, plus a `stat` of `os.py`, which a
//!   patch upgrade rewrites); files are trusted like a distribution's.
//!
//! [`load`] wraps [`index::load`]: it opens the segments of every group in parallel, the loader
//! copies hits and extracts the rest, then every group whose segment is missing or stale is
//! re-encoded from the finished index and written atomically (temp file + `rename`). A
//! segment that fails validation (version, schema, checksum, structure) is ignored and
//! rewritten: the cache never makes a load fail.
//!
//! The schema hash ([`schema`]) covers the format version, the fact layout and the crate
//! version: any of them changing invalidates every segment. Bump [`FACTS_VERSION`] whenever
//! extraction changes what it produces.

mod format;
mod mmap;
mod segment;

#[cfg(test)]
mod tests;

pub use format::CacheError;
pub use segment::Stamp;

pub(crate) use segment::Segment;

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::arena::PerThread;
use crate::config::Config;
use crate::extract::MetricsTable;
use crate::extract::facts::{FactStore, FileFacts};
use crate::index::{FileId, Group, Index, Inputs};
use crate::intern::{FxHasher, Interner, hash_bytes};
use crate::roots::{Origin, Roots};

/// Bump whenever the extractor changes the facts or metrics it produces for some input.
pub const FACTS_VERSION: u32 = 1;

/// Kind of a facts segment in the container header.
const KIND_FACTS: u32 = 1;

/// Name of the file written inside the cache directory so that it is never committed.
const GITIGNORE: &str = ".gitignore";

/// Files whose mtime is this close to the time they were cached are stored with an unknown
/// mtime, so the next load compares their content: a write within the same mtime tick after
/// caching would otherwise go unnoticed ("racy" stat, as in git).
const RACY_NS: u64 = 2_000_000_000;

/// Hash of everything that must match for a segment to be readable by this build: container
/// version, [`FACTS_VERSION`], crate version and the row layout of every table.
pub fn schema() -> u64 {
    use crate::extract::facts::{Bases, Bindings, Calls, Decls, Decorators, Defs, Diags, Imports, Params, Suppressions};
    let mut h = FxHasher::default();
    let parts = [
        format::FORMAT_VERSION as usize,
        FACTS_VERSION as usize,
        Defs::ROW_BYTES,
        Decorators::ROW_BYTES,
        Bases::ROW_BYTES,
        Params::ROW_BYTES,
        Imports::ROW_BYTES,
        Calls::ROW_BYTES,
        Bindings::ROW_BYTES,
        Decls::ROW_BYTES,
        Diags::ROW_BYTES,
        Suppressions::ROW_BYTES,
        MetricsTable::ROW_BYTES,
        size_of::<FileFacts>(),
    ];
    for p in parts {
        std::hash::Hasher::write_usize(&mut h, p);
    }
    std::hash::Hasher::write(&mut h, env!("CARGO_PKG_VERSION").as_bytes());
    std::hash::Hasher::finish(&h)
}

/// A cache directory. Creating one touches nothing on disk; the directory (and its
/// `.gitignore`) are created on the first write.
#[derive(Clone, Debug)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(dir: impl Into<PathBuf>) -> Cache {
        Cache { dir: dir.into() }
    }

    /// The configured cache (`cache-dir`, default `.pymapper_cache` in the project root), or
    /// `None` when `cache = false`.
    pub fn from_config(config: &Config) -> Option<Cache> {
        config.cache.then(|| Cache::new(&config.cache_dir))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Creates the directory and its `.gitignore` if missing.
    pub fn ensure_dir(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let gi = self.dir.join(GITIGNORE);
        if !gi.exists() {
            format::write_atomic(&gi, b"# Created by pymapper: the cache is never committed.\n*\n")?;
        }
        Ok(())
    }

    /// Removes every cache file (`*.pmc` and stale temp files); keeps the directory.
    pub fn clear(&self) -> io::Result<()> {
        let rd = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        for ent in rd {
            let ent = ent?;
            let n = ent.file_name();
            let n = n.as_bytes();
            if n.ends_with(b".pmc") || n.starts_with(b".tmp-") {
                std::fs::remove_file(ent.path())?;
            }
        }
        Ok(())
    }
}

/// How the entries of a segment are validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Per file: size + mtime, else content hash.
    Stat,
    /// The segment key covers the content (distribution `RECORD`, stdlib install).
    Trusted,
}

/// What one group of the inputs is cached as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// File name inside the cache directory.
    pub file: String,
    pub key: u64,
    pub mode: Mode,
    /// Paths are stored relative to this directory (when under it).
    pub base: PathBuf,
}

/// Hash of a list of byte strings (length-prefixed, so the split is unambiguous).
fn key_of(parts: &[&[u8]]) -> u64 {
    let mut h = FxHasher::default();
    for p in parts {
        std::hash::Hasher::write_u64(&mut h, hash_bytes(p));
        std::hash::Hasher::write_usize(&mut h, p.len());
    }
    std::hash::Hasher::finish(&h)
}

/// Keeps file names portable: anything but `[A-Za-z0-9._+-]` becomes `_`.
fn sanitize(s: &[u8]) -> String {
    s.iter()
        .map(|&c| if c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'+' | b'-') { c as char } else { '_' })
        .collect()
}

/// `(size, mtime ns)` of a path, zeros if missing.
fn stat_key(p: &Path) -> [u8; 16] {
    let (size, mtime) = std::fs::metadata(p).map_or((0, 0), |m| (m.size(), mtime_ns(&m)));
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&size.to_le_bytes());
    out[8..].copy_from_slice(&mtime.to_le_bytes());
    out
}

/// Modification time in nanoseconds since the epoch (0 before it).
#[inline]
pub(crate) fn mtime_ns(m: &std::fs::Metadata) -> u64 {
    u64::try_from(m.mtime()).map_or(0, |s| s.saturating_mul(1_000_000_000).saturating_add(m.mtime_nsec() as u64))
}

impl Plan {
    /// How group `g` is cached; `roots` (when the inputs came from import roots) identifies
    /// distributions and the stdlib install.
    pub(crate) fn for_group(g: &Group, roots: Option<&Roots>) -> Plan {
        if let Some(r) = roots {
            if g.origin == Origin::Stdlib
                && let (Some(dir), Some(v)) = (&r.stdlib, r.python_version)
            {
                let ver = format!("{v}");
                let dirb = dir.as_os_str().as_bytes();
                let key = key_of(&[b"stdlib", ver.as_bytes(), dirb, &stat_key(&dir.join("os.py"))]);
                let file = format!("stdlib-{ver}-{:08x}-{key:016x}.pmc", hash_bytes(dirb) as u32);
                return Plan { file, key, mode: Mode::Trusted, base: dir.clone() };
            }
            if g.origin == Origin::ThirdParty
                && let Some(d) = g.dist
                && r.dists.record_hash(d) != 0
                && !r.dists.is_editable(d)
            {
                let (name, version) = (r.dists.normalized_name(d), r.dists.version(d));
                let key = key_of(&[b"dist", name, version, &r.dists.record_hash(d).to_le_bytes()]);
                let file = format!("dist-{}@{}@{key:016x}.pmc", sanitize(name), sanitize(version));
                return Plan { file, key, mode: Mode::Trusted, base: r.dists.site_path(d).to_path_buf() };
            }
        }
        let base = roots.map(|r| r.project_root.clone()).unwrap_or_default();
        let origin = [g.origin as u8];
        let key = key_of(&[b"files", &origin, &g.name, base.as_os_str().as_bytes()]);
        Plan { file: format!("files-{key:016x}.pmc"), key, mode: Mode::Stat, base }
    }
}

/// File-name prefix shared by older versions of a segment (another version of the same
/// distribution, another build of the same interpreter install), which a write replaces;
/// `None` for segments without versions.
fn family(file: &str) -> Option<&str> {
    let cut = if file.starts_with("dist-") {
        file.find('@')
    } else if file.starts_with("stdlib-") {
        file.rfind('-')
    } else {
        None
    };
    cut.map(|i| &file[..=i])
}

/// `path` as stored in a segment: relative to `base` when under it.
#[inline]
pub(crate) fn stored_path<'a>(path: &'a [u8], base: &[u8]) -> &'a [u8] {
    if base.is_empty() {
        return path;
    }
    let base = base.strip_suffix(b"/").unwrap_or(base);
    match path.strip_prefix(base).and_then(|r| r.strip_prefix(b"/")) {
        Some(rel) if !rel.is_empty() => rel,
        _ => path,
    }
}

/// What the loader should do with one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    /// Not cached (or another size): read and extract.
    Miss,
    /// Cached and unchanged: copy entry `.0` of the group's segment (stored stamp `.1`).
    Hit(u32, Stamp),
    /// Cached, same size but another mtime: read, and copy entry `.0` if the content hash
    /// matches the stored stamp `.1`.
    Verify(u32, Stamp),
}

/// A group's segment over the load: opened (and validated) by the first worker that probes
/// one of its files, unmapped by the last one done with them. Only the segments being
/// copied are resident, never the whole cache.
enum State {
    Unopened,
    Ready(Segment),
    /// No segment file.
    Missing,
    /// Present but unusable (version, schema, checksum, structure).
    Invalid,
    /// Every file of the group went through the loader.
    Released,
}

struct Open {
    state: RwLock<State>,
    /// Files stored in the segment (`u32::MAX` until opened).
    stored: AtomicU32,
    /// Files of the group the loader has not finished yet.
    remaining: AtomicU32,
}

/// The segments of one load, per group, as seen by the loader's workers.
pub(crate) struct Lookup {
    dir: PathBuf,
    schema: u64,
    plans: Vec<Plan>,
    open: Vec<Open>,
    /// Files copied from each group's segment (hits and verified).
    hits: Vec<AtomicU32>,
    /// Of those, files whose content hash had to be compared.
    verified: Vec<AtomicU32>,
    opened: AtomicU32,
    invalid: AtomicU32,
    opened_bytes: AtomicU64,
}

/// Read access to a segment state, ignoring poisoning (a panicking worker aborts the load
/// anyway).
fn read(s: &RwLock<State>) -> RwLockReadGuard<'_, State> {
    s.read().unwrap_or_else(PoisonError::into_inner)
}

impl Lookup {
    /// A lookup over the segments of `plans` in `dir` (none opened yet); `group_files[g]` is
    /// the number of input files of group `g`.
    fn new(dir: PathBuf, schema: u64, plans: Vec<Plan>, group_files: &[u32]) -> Lookup {
        let groups = plans.len();
        let open = group_files
            .iter()
            .map(|&n| Open {
                state: RwLock::new(State::Unopened),
                stored: AtomicU32::new(u32::MAX),
                remaining: AtomicU32::new(n),
            })
            .collect();
        Lookup {
            dir,
            schema,
            plans,
            open,
            hits: (0..groups).map(|_| AtomicU32::new(0)).collect(),
            verified: (0..groups).map(|_| AtomicU32::new(0)).collect(),
            opened: AtomicU32::new(0),
            invalid: AtomicU32::new(0),
            opened_bytes: AtomicU64::new(0),
        }
    }

    /// A lookup over already opened segments (tests).
    #[cfg(test)]
    fn with_segments(plans: Vec<Plan>, segs: Vec<Option<Segment>>, group_files: &[u32]) -> Lookup {
        let l = Lookup::new(PathBuf::new(), 0, plans, group_files);
        for (o, s) in l.open.iter().zip(segs) {
            let st = match s {
                Some(seg) => {
                    o.stored.store(seg.file_count(), Ordering::Relaxed);
                    State::Ready(seg)
                }
                None => State::Missing,
            };
            *o.state.write().unwrap_or_else(PoisonError::into_inner) = st;
        }
        l
    }

    /// Opens group `g`'s segment if nobody has yet.
    fn open_segment(&self, g: usize) {
        let o = &self.open[g];
        let mut st = o.state.write().unwrap_or_else(PoisonError::into_inner);
        if !matches!(*st, State::Unopened) {
            return;
        }
        let p = &self.plans[g];
        *st = match Segment::open(&self.dir.join(&p.file), KIND_FACTS, self.schema, p.key) {
            Ok(seg) => {
                self.opened.fetch_add(1, Ordering::Relaxed);
                self.opened_bytes.fetch_add(seg.byte_len() as u64, Ordering::Relaxed);
                o.stored.store(seg.file_count(), Ordering::Relaxed);
                State::Ready(seg)
            }
            Err(CacheError::Io(e)) if e.kind() == io::ErrorKind::NotFound => State::Missing,
            Err(_) => {
                self.invalid.fetch_add(1, Ordering::Relaxed);
                State::Invalid
            }
        };
    }

    /// Read access to group `g`'s segment, opening it on first use.
    fn segment(&self, g: usize) -> RwLockReadGuard<'_, State> {
        let o = &self.open[g];
        let guard = read(&o.state);
        if !matches!(*guard, State::Unopened) {
            return guard;
        }
        drop(guard);
        self.open_segment(g);
        read(&o.state)
    }

    /// Decides what to do with file `path` of group `g` (a `stat` for [`Mode::Stat`] groups).
    pub fn probe(&self, g: u32, path: &Path) -> Probe {
        let guard = self.segment(g as usize);
        let State::Ready(seg) = &*guard else { return Probe::Miss };
        let plan = &self.plans[g as usize];
        let Some(i) = seg.find(stored_path(path.as_os_str().as_bytes(), plan.base.as_os_str().as_bytes())) else {
            return Probe::Miss;
        };
        let s = seg.stamp(i);
        drop(guard);
        if plan.mode == Mode::Trusted {
            return Probe::Hit(i, s);
        }
        let Ok(m) = std::fs::metadata(path) else { return Probe::Miss };
        if m.size() != s.size {
            Probe::Miss
        } else if s.mtime_ns != 0 && mtime_ns(&m) == s.mtime_ns {
            Probe::Hit(i, s)
        } else {
            Probe::Verify(i, s)
        }
    }

    /// Copies entry `i` of group `g`'s segment into `store` / `metrics`, interning its
    /// strings into `strings` (through the worker's `remap` of this segment), and counts the
    /// hit. `None` only if the segment is gone (every file of the group was already
    /// [`Lookup::done`]).
    #[allow(clippy::too_many_arguments)]
    pub fn copy(
        &self,
        g: u32,
        i: u32,
        verified: bool,
        store: &mut FactStore,
        metrics: &mut MetricsTable,
        strings: &mut Interner,
        remap: &mut SegRemap,
    ) -> Option<FileFacts> {
        let guard = read(&self.open[g as usize].state);
        let State::Ready(seg) = &*guard else { return None };
        if remap.group != Some(g) {
            remap.group = Some(g);
            remap.ids.clear();
            remap.ids.resize(seg.string_count() as usize, 0);
        }
        let ff = seg.copy_file(i, store, metrics, strings, &mut remap.ids);
        drop(guard);
        self.hits[g as usize].fetch_add(1, Ordering::Relaxed);
        if verified {
            self.verified[g as usize].fetch_add(1, Ordering::Relaxed);
        }
        Some(ff)
    }

    /// The loader is done with one file of group `g`; the last one unmaps the segment.
    pub fn done(&self, g: u32) {
        let o = &self.open[g as usize];
        if o.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            let old = std::mem::replace(&mut *o.state.write().unwrap_or_else(PoisonError::into_inner), State::Released);
            drop(old);
        }
    }

    /// Files stored in group `g`'s segment, if it was opened.
    fn stored(&self, g: usize) -> Option<u32> {
        let n = self.open[g].stored.load(Ordering::Relaxed);
        (n != u32::MAX).then_some(n)
    }
}

/// A worker's translation of the current segment's local string ids into its interner
/// (local index -> raw `Sym`, 0 = not interned yet). Reset when the worker moves to another
/// group's segment; chunks are handed out in file order and groups are contiguous, so a
/// worker resets about once per segment it touches.
#[derive(Default)]
pub(crate) struct SegRemap {
    group: Option<u32>,
    ids: Vec<u32>,
}

/// What one [`load`] did with the cache.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Groups of the inputs (one segment each).
    pub segments: u32,
    /// Segments found and valid.
    pub opened: u32,
    /// Segments present but unusable (other version, corrupt, ...): rewritten.
    pub invalid: u32,
    /// Bytes of the opened segments (mapped or read).
    pub opened_bytes: u64,
    /// Files copied from the cache without reading them.
    pub hits: u64,
    /// Files copied from the cache after a content-hash check (mtime changed, content not).
    pub verified: u64,
    /// Files read and extracted.
    pub misses: u64,
    /// Segments (re)written, and their bytes.
    pub written: u32,
    pub written_bytes: u64,
    /// Encoding and writing segments, wall time.
    pub write: Duration,
    /// First error writing the cache (the load itself never fails).
    pub write_error: Option<String>,
}

/// [`crate::index::load`] through the cache: copies every unchanged file from its group's
/// segment, extracts the rest, then rewrites the segments of groups that changed (see the
/// module docs). `roots` identifies distributions and the stdlib (`None`: every group is
/// validated per file). The index is the same as without a cache.
pub fn load(inputs: Inputs, roots: Option<&Roots>, cache: &Cache) -> (Index, CacheStats) {
    let mut stats = CacheStats { segments: inputs.groups().len() as u32, ..CacheStats::default() };
    if !cfg!(target_endian = "little") {
        // The format is little-endian and read in place; other hosts load without a cache.
        let index = crate::index::load(inputs);
        stats.misses = u64::from(index.file_count());
        return (index, stats);
    }
    let schema = schema();
    let plans: Vec<Plan> = inputs.groups().iter().map(|g| Plan::for_group(g, roots)).collect();
    let mut group_files = vec![0u32; plans.len()];
    for f in 0..inputs.len() as FileId {
        group_files[inputs.group(f) as usize] += 1;
    }
    let mut lookup = Lookup::new(cache.dir.clone(), schema, plans, &group_files);
    let (index, stamps) = crate::index::load_with(inputs, Some(&mut lookup));
    let hits: u64 = lookup.hits.iter().map(|h| u64::from(h.load(Ordering::Relaxed))).sum();
    stats.verified = lookup.verified.iter().map(|h| u64::from(h.load(Ordering::Relaxed))).sum();
    stats.hits = hits - stats.verified;
    stats.misses = u64::from(index.file_count()) - hits;
    stats.opened = lookup.opened.load(Ordering::Relaxed);
    stats.invalid = lookup.invalid.load(Ordering::Relaxed);
    stats.opened_bytes = lookup.opened_bytes.load(Ordering::Relaxed);
    let t1 = Instant::now();
    crate::timing::time("cache.write", || write_back(cache, &index, &stamps, &lookup, schema, &mut stats));
    stats.write = t1.elapsed();
    (index, stats)
}

/// Rewrites the segment of every group that is missing, stale (a file extracted or
/// verified) or holds files this load does not have.
fn write_back(cache: &Cache, index: &Index, stamps: &[Stamp], lookup: &Lookup, schema: u64, stats: &mut CacheStats) {
    let groups = lookup.plans.len();
    let mut members: Vec<Vec<FileId>> = vec![Vec::new(); groups];
    let mut unreadable = vec![false; index.file_count() as usize];
    for e in index.read_errors() {
        unreadable[e.file as usize] = true;
    }
    for f in 0..index.file_count() {
        if !unreadable[f as usize] {
            members[index.file_group(f) as usize].push(f);
        }
    }
    let stale: Vec<usize> = (0..groups)
        .filter(|&g| {
            let files = members[g].len() as u64;
            let hits = u64::from(lookup.hits[g].load(Ordering::Relaxed));
            let verified = lookup.verified[g].load(Ordering::Relaxed);
            let stored = lookup.stored(g).map(u64::from);
            (files > 0 || stored.is_some_and(|s| s > 0)) && (stored != Some(files) || hits != files || verified > 0)
        })
        .collect();
    if stale.is_empty() {
        return;
    }
    if let Err(e) = cache.ensure_dir() {
        stats.write_error = Some(format!("{}: {e}", cache.dir.display()));
        return;
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let maps = PerThread::new(segment::SymMap::default);
    let results: Vec<Result<u64, String>> = stale
        .par_iter()
        .map(|&g| {
            let plan = &lookup.plans[g];
            let base = plan.base.as_os_str().as_bytes();
            let files: Vec<segment::EncFile<'_>> = members[g]
                .iter()
                .map(|&f| {
                    let ff = *index.file_facts(f);
                    let mut stamp = stamps[f as usize];
                    if stamp.mtime_ns.saturating_add(RACY_NS) > now {
                        stamp.mtime_ns = 0;
                    }
                    // One metrics row per def row except `<module>`, files in FileId order.
                    let metrics = ff.defs.start - f;
                    let path = stored_path(index.file_path(f).as_os_str().as_bytes(), base);
                    segment::EncFile { path, ff, metrics, stamp }
                })
                .collect();
            let path = cache.dir.join(&plan.file);
            let (_, len) = format::write_atomic_with(&path, |out| {
                maps.with(|_, m| {
                    segment::encode(index.strings(), index.facts(), index.metrics(), &files, m, KIND_FACTS, schema, plan.key, out)
                })
            })
            .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(len)
        })
        .collect();
    for r in results {
        match r {
            Ok(b) => {
                stats.written += 1;
                stats.written_bytes += b;
            }
            Err(e) => {
                stats.write_error.get_or_insert(e);
            }
        }
    }
    remove_superseded(cache, stale.iter().map(|&g| &lookup.plans[g]));
}

/// Deletes older versions of the segments just written (same family, other file).
fn remove_superseded<'a>(cache: &Cache, written: impl Iterator<Item = &'a Plan>) {
    let mut keep: crate::intern::FxHashMap<&str, &str> = crate::intern::FxHashMap::default();
    for p in written {
        if let Some(fam) = family(&p.file) {
            keep.insert(fam, &p.file);
        }
    }
    if keep.is_empty() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(&cache.dir) else { return };
    for ent in rd.flatten() {
        let name = ent.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.ends_with(".pmc")
            && let Some(fam) = family(name)
            && keep.get(fam).is_some_and(|&k| k != name)
        {
            let _ = std::fs::remove_file(ent.path());
        }
    }
}
