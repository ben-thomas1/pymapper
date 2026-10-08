//! Installed distributions: `*.dist-info` (and legacy `*.egg-info`) parsing and the
//! file -> distribution index.
//!
//! Per distribution we read the `METADATA` headers (name, version; only the first 4 KiB unless
//! the headers are longer), `RECORD` (the installed files, so third-party code needs no walk),
//! `top_level.txt` and `direct_url.json` (editable installs). The metadata directories of a
//! site are parsed in parallel, then merged sequentially in sorted order, so ids are
//! deterministic.
//!
//! ```text
//!   strings  [u8]       names, versions, metadata dir names, editable sources back to back
//!   per site: files Interner   path relative to the site -> Sym (dense)
//!             owner [DistId]     per file Sym: first distribution (in id order) listing it
//!   dist_files CSR      per distribution: its .py/.pyi files (Syms), RECORD order
//!   tops     Interner   top-level import names; CSR both ways (dist -> names, name -> dists)
//! ```

use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use super::{Diagnostic, DiagnosticKind, bytes_path, is_identifier};
use crate::arena::{Pool, Slice};
use crate::intern::{Interner, Sym, hash_bytes};

/// Bytes of `METADATA` / `PKG-INFO` read first; the headers almost always fit.
const METADATA_PREFIX: usize = 4 * 1024;
/// Larger metadata files (`RECORD`, `METADATA`, ...) are rejected with a diagnostic.
const MAX_METADATA_FILE: u64 = 64 * 1024 * 1024;

/// Index of a distribution in [`Distributions`] (sorted by site, then metadata dir name).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DistId(pub u32);

impl DistId {
    /// Index for side tables.
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

const EDITABLE: u8 = 1;
const EGG_INFO: u8 = 2;
const HAS_RECORD: u8 = 4;
const HAS_TOP_LEVEL: u8 = 8;

/// All distributions found in the site-packages directories, struct-of-arrays.
#[derive(Default)]
pub struct Distributions {
    sites: Vec<PathBuf>,
    strings: Pool<u8>,
    name: Vec<Slice<u8>>,
    version: Vec<Slice<u8>>,
    info: Vec<Slice<u8>>,
    editable_source: Vec<Slice<u8>>,
    norm: Vec<Sym>,
    site: Vec<u8>,
    flags: Vec<u8>,
    record_hash: Vec<u64>,
    /// Normalised names; `norm_owner[sym]` is the first distribution with that name.
    norms: Interner,
    norm_owner: Vec<DistId>,
    /// Per site: RECORD paths relative to the site and their owners.
    site_files: Vec<SiteFiles>,
    file_start: Vec<u32>,
    file_syms: Vec<Sym>,
    tops: Interner,
    /// (dist, top-level name) pairs collected until [`Distributions::finish`].
    top_pairs: Vec<(DistId, Sym)>,
    dist_top_start: Vec<u32>,
    dist_tops: Vec<Sym>,
    top_dist_start: Vec<u32>,
    top_dists: Vec<DistId>,
}

/// The file index of one site directory.
#[derive(Default)]
struct SiteFiles {
    files: Interner,
    owner: Vec<DistId>,
    /// First path components of every RECORD entry (files of any type).
    claimed: Interner,
}

/// One metadata directory parsed on a worker thread. Byte ranges point into `strs`.
#[derive(Default)]
struct RawDist {
    site: u8,
    info: Box<[u8]>,
    strs: Vec<u8>,
    name: (u32, u32),
    version: (u32, u32),
    editable_source: (u32, u32),
    flags: u8,
    record_hash: u64,
    py: Vec<(u32, u32)>,
    pth: Vec<(u32, u32)>,
    tops: Vec<(u32, u32)>,
    claimed: Vec<(u32, u32)>,
    diags: Vec<Diagnostic>,
}

impl RawDist {
    fn push_str(&mut self, s: &[u8]) -> (u32, u32) {
        let start = self.strs.len() as u32;
        self.strs.extend_from_slice(s);
        (start, s.len() as u32)
    }

    fn get(&self, r: (u32, u32)) -> &[u8] {
        &self.strs[r.0 as usize..(r.0 + r.1) as usize]
    }

    fn diag(&mut self, kind: DiagnosticKind, path: PathBuf, message: String) {
        self.diags.push(Diagnostic { kind, path, message });
    }
}

impl Distributions {
    /// Number of distributions.
    pub fn len(&self) -> usize {
        self.name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
    }

    /// Every distribution id, in order.
    pub fn ids(&self) -> impl ExactSizeIterator<Item = DistId> + use<> {
        (0..self.len() as u32).map(DistId)
    }

    /// The site-packages directories, in the order they were added.
    pub fn sites(&self) -> &[PathBuf] {
        &self.sites
    }

    /// Project name as written in the metadata (e.g. `PyYAML`).
    pub fn name(&self, d: DistId) -> &[u8] {
        self.strings.slice(self.name[d.index()])
    }

    /// PEP 503 normalised name (e.g. `pyyaml`, `zope-interface`).
    pub fn normalized_name(&self, d: DistId) -> &[u8] {
        self.norms.resolve(self.norm[d.index()])
    }

    /// Version string (may be empty if neither the metadata nor the dir name has one).
    pub fn version(&self, d: DistId) -> &[u8] {
        self.strings.slice(self.version[d.index()])
    }

    /// Index of the site-packages directory (into [`Distributions::sites`]).
    pub fn site(&self, d: DistId) -> usize {
        usize::from(self.site[d.index()])
    }

    /// The site-packages directory of `d`.
    pub fn site_path(&self, d: DistId) -> &Path {
        &self.sites[self.site(d)]
    }

    /// The metadata directory name, e.g. `PyYAML-6.0.2.dist-info`.
    pub fn info_name(&self, d: DistId) -> &[u8] {
        self.strings.slice(self.info[d.index()])
    }

    /// Full path of the metadata directory.
    pub fn info_path(&self, d: DistId) -> PathBuf {
        self.site_path(d).join(bytes_path(self.info_name(d)))
    }

    /// Installed in editable / development mode (`direct_url.json`).
    pub fn is_editable(&self, d: DistId) -> bool {
        self.flags[d.index()] & EDITABLE != 0
    }

    /// Legacy `*.egg-info` metadata.
    pub fn is_egg_info(&self, d: DistId) -> bool {
        self.flags[d.index()] & EGG_INFO != 0
    }

    /// Whether a file list (`RECORD` / `installed-files.txt`) was read.
    pub fn has_record(&self, d: DistId) -> bool {
        self.flags[d.index()] & HAS_RECORD != 0
    }

    /// Whether `top_level.txt` was present.
    pub fn has_top_level_txt(&self, d: DistId) -> bool {
        self.flags[d.index()] & HAS_TOP_LEVEL != 0
    }

    /// Source directory of an editable install (`file://` URL in `direct_url.json`).
    pub fn editable_source(&self, d: DistId) -> Option<&Path> {
        let s = self.strings.slice(self.editable_source[d.index()]);
        if s.is_empty() { None } else { Some(bytes_path(s)) }
    }

    /// Hash of the raw `RECORD` bytes (0 without one); with name and version it is the
    /// third-party cache key.
    pub fn record_hash(&self, d: DistId) -> u64 {
        self.record_hash[d.index()]
    }

    /// The `.py` / `.pyi` files of `d` as ids in its site's file table (see
    /// [`Distributions::site_file`]), in `RECORD` order. Only files inside the site directory
    /// are listed.
    pub fn file_ids(&self, d: DistId) -> &[Sym] {
        let i = d.index();
        &self.file_syms[self.file_start[i] as usize..self.file_start[i + 1] as usize]
    }

    /// The `.py` / `.pyi` files of `d`, relative to its site directory.
    pub fn files(&self, d: DistId) -> impl ExactSizeIterator<Item = &[u8]> + '_ {
        let site = &self.site_files[self.site(d)];
        self.file_ids(d).iter().map(|&s| site.files.resolve(s))
    }

    /// Path (relative to the site) of a file id of site `site`.
    pub fn site_file(&self, site: usize, file: Sym) -> &[u8] {
        self.site_files[site].files.resolve(file)
    }

    /// Number of distinct indexed files (`.py`, `.pyi` and `.pth` entries of all RECORDs).
    pub fn file_count(&self) -> usize {
        self.site_files.iter().map(|s| s.owner.len()).sum()
    }

    /// Whether some `RECORD` of site `site` lists a path whose first component is `name`.
    pub fn is_claimed(&self, site: usize, name: &[u8]) -> bool {
        self.site_files.get(site).is_some_and(|sf| sf.claimed.get(name).is_some())
    }

    /// Distribution whose `RECORD` lists `rel` (relative to site `site`); the first one in id
    /// order if several do. O(1).
    pub fn owner(&self, site: usize, rel: &[u8]) -> Option<DistId> {
        let sf = self.site_files.get(site)?;
        sf.files.get(rel).map(|s| sf.owner[s.index()])
    }

    /// Top-level import names of `d` (from `top_level.txt`, `RECORD` and editable mappings),
    /// sorted by name. Valid after [`Distributions::finish`].
    pub fn top_level(&self, d: DistId) -> impl ExactSizeIterator<Item = &[u8]> + '_ {
        let i = d.index();
        let r = self.dist_top_start[i] as usize..self.dist_top_start[i + 1] as usize;
        self.dist_tops[r].iter().map(|&s| self.tops.resolve(s))
    }

    /// Distributions providing the top-level import name `name` (several for namespace
    /// packages such as `google`). O(1).
    pub fn providers(&self, name: &[u8]) -> &[DistId] {
        match self.tops.get(name) {
            Some(s) if s.index() + 1 < self.top_dist_start.len() => {
                let i = s.index();
                &self.top_dists[self.top_dist_start[i] as usize..self.top_dist_start[i + 1] as usize]
            }
            _ => &[],
        }
    }

    /// Distribution with the given project name (any spelling; PEP 503 normalised). O(1).
    pub fn by_name(&self, name: &[u8]) -> Option<DistId> {
        let mut norm = Vec::with_capacity(name.len());
        normalize_name(name, &mut norm);
        self.norms.get(&norm).map(|s| self.norm_owner[s.index()])
    }

    /// Records `name` as a top-level import name of `d` (editable import-hook mappings).
    /// Call [`Distributions::finish`] afterwards.
    pub fn add_top_level(&mut self, d: DistId, name: &[u8]) {
        let s = self.tops.intern(name);
        self.top_pairs.push((d, s));
    }

    /// Approximate heap bytes.
    pub fn heap_bytes(&self) -> usize {
        let slices = self.name.capacity() + self.version.capacity() + self.info.capacity() + self.editable_source.capacity();
        self.strings.heap_bytes()
            + slices * size_of::<Slice<u8>>()
            + (self.norm.capacity() + self.file_syms.capacity() + self.dist_tops.capacity()) * 4
            + self.site.capacity()
            + self.flags.capacity()
            + self.record_hash.capacity() * 8
            + self.norms.heap_bytes()
            + self.site_files.iter().map(|s| s.files.heap_bytes() + s.claimed.heap_bytes() + s.owner.capacity() * 4).sum::<usize>()
            + self.tops.heap_bytes()
            + (self.norm_owner.capacity() + self.top_dists.capacity()) * 4
            + (self.file_start.capacity() + self.dist_top_start.capacity() + self.top_dist_start.capacity()) * 4
            + self.top_pairs.capacity() * 8
    }
}

// ---------------------------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------------------------

impl Distributions {
    /// Parses every metadata directory of the site-packages directory `site` (in parallel on
    /// the current rayon pool) and appends the distributions. Returns the site index, or `None`
    /// when 256 sites were already added (a diagnostic is recorded).
    pub(super) fn add_site(&mut self, site: &Path, diags: &mut Vec<Diagnostic>) -> Option<usize> {
        let idx = self.sites.len();
        let Ok(site_byte) = u8::try_from(idx) else {
            let message = "too many site-packages directories (max 256); ignored".to_owned();
            diags.push(Diagnostic { kind: DiagnosticKind::ReadDir, path: site.to_path_buf(), message });
            return None;
        };
        if self.file_start.is_empty() {
            self.file_start.push(0);
        }
        self.sites.push(site.to_path_buf());
        self.site_files.push(SiteFiles::default());
        let mut infos: Vec<Box<[u8]>> = Vec::new();
        match fs::read_dir(site) {
            Err(e) => {
                diags.push(Diagnostic { kind: DiagnosticKind::ReadDir, path: site.to_path_buf(), message: e.to_string() });
                return Some(idx);
            }
            Ok(rd) => {
                for ent in rd.flatten() {
                    let name = ent.file_name();
                    let b = name.as_bytes();
                    if b.ends_with(b".dist-info") || b.ends_with(b".egg-info") {
                        infos.push(b.into());
                    }
                }
            }
        }
        infos.sort_unstable();
        let site_bytes = site.as_os_str().as_bytes();
        let raws: Vec<RawDist> = infos
            .par_iter()
            .map_init(|| (Vec::new(), Vec::new()), |(buf, scratch), info| parse_info(site_bytes, site_byte, info, buf, scratch))
            .collect();
        let mut scratch = Vec::new();
        for raw in raws {
            self.merge(raw, &mut scratch, diags);
        }
        Some(idx)
    }

    fn merge(&mut self, mut raw: RawDist, scratch: &mut Vec<u8>, diags: &mut Vec<Diagnostic>) {
        let id = DistId(self.len() as u32);
        self.site.push(raw.site);
        self.flags.push(raw.flags);
        self.record_hash.push(raw.record_hash);
        self.name.push(self.strings.push_slice(raw.get(raw.name)));
        self.version.push(self.strings.push_slice(raw.get(raw.version)));
        self.info.push(self.strings.push_slice(&raw.info));
        self.editable_source.push(self.strings.push_slice(raw.get(raw.editable_source)));
        normalize_name(raw.get(raw.name), scratch);
        let norm = self.norms.intern(scratch);
        if norm.index() == self.norm_owner.len() {
            self.norm_owner.push(id);
        }
        self.norm.push(norm);
        let n_py = raw.py.len();
        let sf = &mut self.site_files[usize::from(raw.site)];
        for (i, &r) in raw.py.iter().chain(&raw.pth).enumerate() {
            let sym = sf.files.intern(raw.get(r));
            if sym.index() == sf.owner.len() {
                sf.owner.push(id);
            }
            if i < n_py {
                self.file_syms.push(sym);
            }
        }
        for &r in &raw.claimed {
            sf.claimed.intern(raw.get(r));
        }
        self.file_start.push(self.file_syms.len() as u32);
        for &r in &raw.tops {
            let sym = self.tops.intern(raw.get(r));
            self.top_pairs.push((id, sym));
        }
        diags.append(&mut raw.diags);
    }

    /// Builds the top-level name indexes from everything added so far. Idempotent; call again
    /// after [`Distributions::add_top_level`].
    pub fn finish(&mut self) {
        let n = self.len();
        self.top_pairs.sort_unstable();
        self.top_pairs.dedup();
        self.dist_top_start.clear();
        self.dist_top_start.resize(n + 1, 0);
        self.dist_tops.clear();
        self.dist_tops.reserve(self.top_pairs.len());
        let mut counts = vec![0u32; self.tops.len() + 1];
        for &(d, s) in &self.top_pairs {
            self.dist_top_start[d.index() + 1] += 1;
            self.dist_tops.push(s);
            counts[s.index() + 1] += 1;
        }
        for i in 0..n {
            self.dist_top_start[i + 1] += self.dist_top_start[i];
            let r = self.dist_top_start[i] as usize..self.dist_top_start[i + 1] as usize;
            let tops = &self.tops;
            self.dist_tops[r].sort_unstable_by(|&a, &b| tops.resolve(a).cmp(tops.resolve(b)));
        }
        for i in 0..self.tops.len() {
            counts[i + 1] += counts[i];
        }
        self.top_dist_start.clone_from(&counts);
        self.top_dists.clear();
        self.top_dists.resize(self.top_pairs.len(), DistId(0));
        // Pairs are sorted by dist, so each name's providers come out in id order.
        for &(d, s) in &self.top_pairs {
            let slot = &mut counts[s.index()];
            self.top_dists[*slot as usize] = d;
            *slot += 1;
        }
    }
}

/// Parses one metadata directory (or `*.egg-info` file). Never fails: problems become
/// diagnostics and missing fields fall back to the directory name.
fn parse_info(site: &[u8], site_byte: u8, info: &[u8], buf: &mut Vec<u8>, scratch: &mut Vec<u8>) -> RawDist {
    let mut raw = RawDist { site: site_byte, info: info.into(), ..RawDist::default() };
    let egg = info.ends_with(b".egg-info");
    let suffix = if egg { b".egg-info".len() } else { b".dist-info".len() };
    let (dir_name, dir_version) = split_dist_dir(&info[..info.len() - suffix]);
    let mut dir = bytes_path(site).join(bytes_path(info));
    if egg {
        raw.flags |= EGG_INFO;
    }
    let single_file = egg && dir.is_file();

    // METADATA / PKG-INFO headers.
    let meta = if single_file { dir.clone() } else { dir.join(if egg { "PKG-INFO" } else { "METADATA" }) };
    let (mut name, mut version) = ((0, 0), (0, 0));
    match read_prefix(&meta, buf, METADATA_PREFIX) {
        Ok(complete) => {
            let h = metadata_headers(buf);
            if !complete && !h.done && (h.name.is_none() || h.version.is_none()) {
                // Headers longer than the prefix (old-style long `Description:`): read it all.
                if let Err(e) = read_all(&meta, buf) {
                    raw.diag(DiagnosticKind::Metadata, meta.clone(), e.to_string());
                }
            }
            let h = metadata_headers(buf);
            if let Some(n) = h.name {
                name = raw.push_str(n);
            }
            if let Some(v) = h.version {
                version = raw.push_str(v);
            }
        }
        Err(e) => raw.diag(DiagnosticKind::Metadata, meta, e.to_string()),
    }
    if name.1 == 0 {
        name = raw.push_str(dir_name);
    }
    if version.1 == 0 {
        version = raw.push_str(dir_version);
    }
    raw.name = name;
    raw.version = version;
    if single_file {
        return raw;
    }

    // RECORD / installed-files.txt.
    dir.push(if egg { "installed-files.txt" } else { "RECORD" });
    match read_all(&dir, buf) {
        Ok(()) => {
            raw.flags |= HAS_RECORD;
            raw.record_hash = hash_bytes(buf);
            for_each_record_path(buf, scratch, |p| add_record_path(&mut raw, site, egg, p));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound && egg => {}
        Err(e) => {
            let msg = if e.kind() == io::ErrorKind::NotFound {
                "no RECORD; the distribution's files are found by walking".to_owned()
            } else {
                e.to_string()
            };
            raw.diag(DiagnosticKind::Record, dir.clone(), msg);
        }
    }
    dir.pop();

    // top_level.txt.
    dir.push("top_level.txt");
    if read_all(&dir, buf).is_ok() {
        raw.flags |= HAS_TOP_LEVEL;
        for line in buf.split(|&b| b == b'\n') {
            let line = trim(line);
            let top = line.split(|&b| b == b'/' || b == b'\\').next().unwrap_or(line);
            if is_identifier(top) {
                let r = raw.push_str(top);
                raw.tops.push(r);
            }
        }
    }
    dir.pop();

    // direct_url.json (PEP 610): editable installs.
    if !egg {
        dir.push("direct_url.json");
        if read_all(&dir, buf).is_ok() && json_bool(buf, b"editable") == Some(true) {
            raw.flags |= EDITABLE;
            if let Some(path) = json_str(buf, b"url", scratch).and_then(file_url_path) {
                raw.editable_source = raw.push_str(&path);
            }
        }
    }
    raw
}

// ---------------------------------------------------------------------------------------------
// Parsing helpers (pure, over bytes)
// ---------------------------------------------------------------------------------------------

/// Splits a metadata dir stem `name-version[-pyX.Y...]` into name and version.
pub(super) fn split_dist_dir(stem: &[u8]) -> (&[u8], &[u8]) {
    match stem.iter().position(|&b| b == b'-') {
        Some(i) => {
            let rest = &stem[i + 1..];
            let v = rest.iter().position(|&b| b == b'-').map_or(rest, |j| &rest[..j]);
            (&stem[..i], v)
        }
        None => (stem, b""),
    }
}

/// `Name` / `Version` from an RFC 822-style metadata header block.
pub(super) struct Headers<'a> {
    pub name: Option<&'a [u8]>,
    pub version: Option<&'a [u8]>,
    /// The blank line ending the headers was seen.
    pub done: bool,
}

pub(super) fn metadata_headers(buf: &[u8]) -> Headers<'_> {
    let mut h = Headers { name: None, version: None, done: false };
    for line in buf.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            h.done = true;
            break;
        }
        if line[0] == b' ' || line[0] == b'\t' {
            continue;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
        let (key, val) = (&line[..colon], trim(&line[colon + 1..]));
        if h.name.is_none() && key.eq_ignore_ascii_case(b"name") {
            h.name = Some(val).filter(|v| !v.is_empty());
        } else if h.version.is_none() && key.eq_ignore_ascii_case(b"version") {
            h.version = Some(val).filter(|v| !v.is_empty());
        }
        if h.name.is_some() && h.version.is_some() {
            break;
        }
    }
    h
}

/// PEP 503 normalisation: ASCII lowercase, runs of `-`, `_`, `.` become one `-`.
pub fn normalize_name(name: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let mut sep = false;
    for &b in name {
        if matches!(b, b'-' | b'_' | b'.') {
            sep = true;
            continue;
        }
        if sep && !out.is_empty() {
            out.push(b'-');
        }
        sep = false;
        out.push(b.to_ascii_lowercase());
    }
}

pub(super) fn trim(s: &[u8]) -> &[u8] {
    s.trim_ascii()
}

/// Reads at most `limit` bytes of `path` into `buf`; `Ok(true)` if that was the whole file.
fn read_prefix(path: &Path, buf: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    buf.clear();
    let f = File::open(path)?;
    f.take(limit as u64).read_to_end(buf)?;
    Ok(buf.len() < limit)
}

/// Reads all of `path` into `buf` (rejecting files over [`MAX_METADATA_FILE`]).
pub(super) fn read_all(path: &Path, buf: &mut Vec<u8>) -> io::Result<()> {
    buf.clear();
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    if len > MAX_METADATA_FILE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("file too large ({len} bytes)")));
    }
    buf.reserve(len as usize);
    f.read_to_end(buf)?;
    Ok(())
}

/// Calls `f` with the first CSV field (the path) of every `RECORD` line. Quoted fields
/// (`"a,b.py"`, doubled quotes inside) are unescaped into `scratch`.
pub(super) fn for_each_record_path(buf: &[u8], scratch: &mut Vec<u8>, mut f: impl FnMut(&[u8])) {
    for line in buf.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if line[0] == b'"' {
            scratch.clear();
            let mut i = 1;
            while i < line.len() {
                if line[i] == b'"' {
                    if line.get(i + 1) == Some(&b'"') {
                        scratch.push(b'"');
                        i += 2;
                        continue;
                    }
                    break;
                }
                scratch.push(line[i]);
                i += 1;
            }
            f(scratch);
        } else {
            let end = line.iter().position(|&b| b == b',').unwrap_or(line.len());
            f(&line[..end]);
        }
    }
}

/// Classifies one `RECORD` path: `.py`/`.pyi` files and top-level `.pth` files inside the site
/// are kept; every importable entry contributes its top-level name.
fn add_record_path(raw: &mut RawDist, site: &[u8], egg: bool, p: &[u8]) {
    let mut p = p;
    if egg {
        // installed-files.txt is relative to the egg-info directory, which sits in the site.
        match p.strip_prefix(b"../") {
            Some(rest) => p = rest,
            None => return,
        }
    }
    if p.first() == Some(&b'/') {
        match p.strip_prefix(site).and_then(|r| r.strip_prefix(b"/")) {
            Some(rest) => p = rest,
            None => return,
        }
    }
    while let Some(rest) = p.strip_prefix(b"./") {
        p = rest;
    }
    if p.is_empty() || p.split(|&b| b == b'/').any(|c| c == b".." || c == b"__pycache__" || c.is_empty()) {
        return;
    }
    let slash = p.iter().position(|&b| b == b'/');
    let top = slash.map_or(p, |i| &p[..i]);
    if top.ends_with(b".dist-info") || top.ends_with(b".egg-info") || top.ends_with(b".data") {
        return;
    }
    if raw.claimed.last().is_none_or(|&last| raw.get(last) != top) {
        let r = raw.push_str(top);
        raw.claimed.push(r);
    }
    let python = crate::walk::is_python(p);
    let ext_module = p.ends_with(b".so") || p.ends_with(b".pyd");
    if python {
        if top.starts_with(b"__editable__") {
            return;
        }
        let r = raw.push_str(p);
        raw.py.push(r);
    } else if slash.is_none() && p.ends_with(b".pth") {
        let r = raw.push_str(p);
        raw.pth.push(r);
        return;
    } else if !ext_module {
        return;
    }
    let name = match slash {
        Some(_) => top.strip_suffix(b"-stubs").unwrap_or(top),
        None if python => top.strip_suffix(b".pyi").or_else(|| top.strip_suffix(b".py")).unwrap_or(top),
        None => top.split(|&b| b == b'.').next().unwrap_or(top),
    };
    if !is_identifier(name) {
        return;
    }
    // RECORD is usually sorted, so repeats of the last name are cheap to skip; `finish`
    // removes the rest.
    if let Some(&last) = raw.tops.last()
        && raw.get(last) == name
    {
        return;
    }
    let r = raw.push_str(name);
    raw.tops.push(r);
}

/// Value of the first `"key": true|false` in a JSON document (best effort, no full parser).
pub(super) fn json_bool(buf: &[u8], key: &[u8]) -> Option<bool> {
    let rest = json_value(buf, key)?;
    if rest.starts_with(b"true") {
        Some(true)
    } else if rest.starts_with(b"false") {
        Some(false)
    } else {
        None
    }
}

/// Value of the first `"key": "string"` in a JSON document, unescaped into `scratch`
/// (`\uXXXX` escapes are kept verbatim; best effort).
pub(super) fn json_str<'a>(buf: &[u8], key: &[u8], scratch: &'a mut Vec<u8>) -> Option<&'a [u8]> {
    let rest = json_value(buf, key)?.strip_prefix(b"\"")?;
    scratch.clear();
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            b'"' => return Some(scratch),
            b'\\' if i + 1 < rest.len() => {
                match rest[i + 1] {
                    b'n' => scratch.push(b'\n'),
                    b't' => scratch.push(b'\t'),
                    b'r' => scratch.push(b'\r'),
                    b'u' => scratch.extend_from_slice(b"\\u"),
                    other => scratch.push(other),
                }
                i += 2;
            }
            b => {
                scratch.push(b);
                i += 1;
            }
        }
    }
    None
}

/// The bytes after `"key"`, whitespace, `:` and whitespace.
fn json_value<'a>(buf: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut from = 0;
    while let Some(i) = find(&buf[from..], key) {
        let at = from + i;
        from = at + key.len();
        if at == 0 || buf[at - 1] != b'"' || buf.get(from) != Some(&b'"') {
            continue;
        }
        let rest = buf[from + 1..].trim_ascii_start();
        if let Some(rest) = rest.strip_prefix(b":") {
            return Some(rest.trim_ascii_start());
        }
    }
    None
}

/// Position of `needle` in `hay` (short needles, small inputs).
pub(super) fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Local path of a `file://` URL, percent-decoded. `None` for other schemes.
pub(super) fn file_url_path(url: &[u8]) -> Option<Vec<u8>> {
    let rest = url.strip_prefix(b"file://")?;
    let rest = rest.strip_prefix(b"localhost").unwrap_or(rest);
    if rest.first() != Some(&b'/') {
        return None;
    }
    let mut out = Vec::with_capacity(rest.len());
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == b'%'
            && let (Some(h), Some(l)) = (rest.get(i + 1).and_then(|&c| hex(c)), rest.get(i + 2).and_then(|&c| hex(c)))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(rest[i]);
        i += 1;
    }
    while out.len() > 1 && out.last() == Some(&b'/') {
        out.pop();
    }
    Some(out)
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
