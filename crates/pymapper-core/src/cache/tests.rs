use std::fmt::Write as _;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

use std::time::{Duration, SystemTime};

use super::format::{self, CacheError, Reader, Writer};
use super::mmap::Bytes;
use super::*;
use crate::extract::facts::render;
use crate::index::{Inputs, load as plain_load};
use crate::roots::Origin;
use crate::walk::{WalkOptions, walk};

/// A fresh temporary directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, AtomicOrdering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pymapper-cache-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// Writes `rel` with an mtime an hour ago (so it is not "racy", see `RACY_NS`).
    fn file(&self, rel: &str, contents: &str) -> &TempDir {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, contents).expect("write");
        set_old_mtime(&p, 3600);
        self
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn set_old_mtime(p: &Path, secs_ago: u64) {
    let t = SystemTime::now() - Duration::from_secs(secs_ago);
    fs::File::options().write(true).open(p).and_then(|f| f.set_modified(t)).expect("set mtime");
}

/// Every node's qid, then every file's facts (rendered with resolved strings), header and
/// metrics rows. Independent of `Sym` numbering: equal dumps mean equal indexes.
fn dump(ix: &Index) -> String {
    let mut s = String::new();
    for v in 0..ix.node_count() {
        let r = ix.subtree(v);
        let _ = writeln!(s, "{v} {} {} [{}, {})", ix.qid_string(v), ix.kind(v).as_str(), r.start, r.end);
    }
    for f in 0..ix.file_count() {
        let ff = ix.file_facts(f);
        let module = String::from_utf8_lossy(ix.str(ix.file_module(f))).into_owned();
        let _ = writeln!(s, "file {f} {} {module} lines={} flags={}", ix.file_path(f).display(), ff.lines, ff.flags);
        s.push_str(&render(ix.file_view(f), |sym| ix.str(sym)));
        let _ = writeln!(s, "evidence {:?}", ix.file_view(f).evidence());
        for d in 1..ff.defs.len {
            let m = ix.metrics_row(ix.def_node(f, d)).expect("metrics row");
            let _ = writeln!(s, "  {:?}", ix.metrics().row(m));
        }
    }
    s
}

fn inputs(root: &Path) -> Inputs {
    let w = walk(&[root], &WalkOptions::everything());
    Inputs::from_root_dir(root, &w.files, Origin::FirstParty, b"proj")
}

fn cached(root: &Path, cache: &Cache) -> (Index, CacheStats) {
    load(inputs(root), None, cache)
}

const SAMPLE: &str = r#"
"""Doc."""
import os
from . import sibling as sib
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    from collections import OrderedDict

__all__ = ["Widget", "make"]

@dataclass(frozen=True)
class Widget(Base, metaclass=Meta):
    x: int = 0

    @property
    def size(self, *args, key=None, **kw) -> int:
        global counter
        if self.x and key or args:
            return os.path.join(self.x, "size")
        for i in range(3):
            while i:
                i -= 1
        return len(args)

def make(a, /, b, *, c=1):
    w = Widget(a)
    w.size()
    return [x for x in b if x]  # pymapper: ignore[PM101, PM102]

if __name__ == "__main__":
    make(1, 2)
"#;

fn sample_project(tag: &str) -> TempDir {
    let t = TempDir::new(tag);
    t.file("pkg/__init__.py", "from .mod import make\n")
        .file("pkg/mod.py", SAMPLE)
        .file("pkg/sibling.py", "def helper():\n    return 1\n")
        .file("pkg/broken.py", "def f(:\n    x = (\nclass\n")
        .file("pkg/stub.pyi", "def g(x: int) -> int: ...\n")
        .file("tests/test_mod.py", "from pkg.mod import make\n\ndef test_make():\n    assert make(1, [2])\n")
        .file("empty.py", "");
    t
}

// -------------------------------------------------------------------------------------------
// Container format
// -------------------------------------------------------------------------------------------

fn sample_container() -> Vec<u8> {
    let mut w = Writer::new(Cursor::new(Vec::new()));
    w.put::<u32>(3, &[1, 2, 3]);
    w.put::<u8>(0, b"hello");
    w.put::<u64>(7, &[u64::MAX]);
    w.put::<u16>(9, &[]);
    let (c, len) = w.finish(5, 11, 13).expect("write");
    assert_eq!(len as usize, c.get_ref().len());
    c.into_inner()
}

#[test]
fn container_roundtrip() {
    let b = sample_container();
    assert_eq!(&b[..8], b"PYMCACHE");
    let r = Reader::from_bytes(Bytes::copy_from(&b), 5, 11, 13).expect("valid");
    assert_eq!(r.get::<u32>(3).expect("u32"), &[1, 2, 3]);
    assert_eq!(r.bytes(0), b"hello");
    assert_eq!(r.get::<u64>(7).expect("u64"), &[u64::MAX]);
    assert!(r.get::<u16>(9).expect("empty").is_empty());
    // Absent sections are empty.
    assert!(r.bytes(1).is_empty() && r.bytes(4000).is_empty());
    // Misaligned/odd-sized views are refused, not misread.
    assert!(r.get::<u32>(0).is_err());
}

#[test]
fn container_rejects_mismatch_and_corruption() {
    let b = sample_container();
    let open = |b: &[u8]| Reader::from_bytes(Bytes::copy_from(b), 5, 11, 13).map(|_| ());
    assert!(matches!(Reader::from_bytes(Bytes::copy_from(&b), 6, 11, 13), Err(CacheError::Key)));
    assert!(matches!(Reader::from_bytes(Bytes::copy_from(&b), 5, 12, 13), Err(CacheError::Schema)));
    assert!(matches!(Reader::from_bytes(Bytes::copy_from(&b), 5, 11, 14), Err(CacheError::Key)));
    assert!(matches!(open(&b[..b.len() - 1]), Err(CacheError::Corrupt(_))));
    assert!(matches!(open(&b[..10]), Err(CacheError::Magic)));
    assert!(matches!(open(b""), Err(CacheError::Magic)));
    let mut v = b.clone();
    v[8] = 99;
    assert!(matches!(open(&v), Err(CacheError::Version(_))));
    // Every single-byte flip of the payload is caught by the checksum.
    for i in format::HEADER_LEN..b.len() {
        let mut v = b.clone();
        v[i] ^= 0x20;
        assert!(matches!(open(&v), Err(CacheError::Checksum)), "flip at {i}");
    }
}

#[test]
fn checksum_depends_on_every_byte_and_length() {
    let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
    let base = format::checksum(&data);
    for i in [0, 7, 31, 32, 500, 999] {
        let mut d = data.clone();
        d[i] ^= 1;
        assert_ne!(format::checksum(&d), base, "byte {i}");
    }
    assert_ne!(format::checksum(&data[..999]), base);
    assert_ne!(format::checksum(&[0u8; 8]), format::checksum(&[0u8; 16]));
}

#[test]
fn streaming_checksum_matches_one_shot() {
    let data: Vec<u8> = (0..777u32).map(|i| (i * 131 % 253) as u8).collect();
    let whole = format::checksum(&data);
    for step in [1, 3, 8, 31, 32, 33, 100, 777] {
        let mut c = format::Checksum::new();
        for part in data.chunks(step) {
            c.update(part);
        }
        assert_eq!(c.finish(), whole, "parts of {step}");
    }
}

#[test]
fn segments_are_unmapped_when_their_files_are_done() {
    let t = sample_project("unmap");
    let ix = plain_load(inputs(t.path()));
    let seg = open_bytes(&encode_index(&ix)).expect("segment");
    let plan = Plan { file: String::new(), key: 2, mode: Mode::Trusted, base: PathBuf::new() };
    let l = Lookup::with_segments(vec![plan], vec![Some(seg)], &[2]);
    let mapped = |l: &Lookup| matches!(*read(&l.open[0].state), State::Ready(_));
    l.done(0);
    assert!(mapped(&l));
    l.done(0);
    assert!(!mapped(&l));
    // A late copy finds nothing (the loader then reads the file instead).
    let (mut store, mut metrics) = (FactStore::new(), MetricsTable::new());
    let (mut strings, mut remap) = (Interner::new(), SegRemap::default());
    let mut fps = Fingerprints::new();
    assert!(l.copy(0, 0, 0, false, &mut store, &mut metrics, &mut strings, &mut remap, &mut fps).is_none());
    assert_eq!(l.probe(0, ix.file_path(0)), Probe::Miss);
}

#[test]
fn atomic_write_replaces_and_leaves_no_temp() {
    let t = TempDir::new("atomic");
    let p = t.path().join("x.pmc");
    format::write_atomic(&p, b"one").expect("write");
    format::write_atomic(&p, b"two").expect("rewrite");
    assert_eq!(fs::read(&p).expect("read"), b"two");
    let names: Vec<_> = fs::read_dir(t.path()).expect("ls").map(|e| e.expect("ent").file_name()).collect();
    assert_eq!(names, ["x.pmc"]);
}

#[test]
fn large_files_are_mapped() {
    let t = TempDir::new("mmap");
    let big: Vec<u32> = (0..100_000).collect();
    let p = t.path().join("big.pmc");
    format::write_atomic_with(&p, |out| {
        let mut w = Writer::new(out);
        w.put(1, &big);
        w.finish(1, 2, 3)
    })
    .expect("write");
    let r = Reader::open(&p, 1, 2, 3).expect("open");
    assert_eq!(r.get::<u32>(1).expect("col"), &big[..]);
}

// -------------------------------------------------------------------------------------------
// Segments
// -------------------------------------------------------------------------------------------

/// Encodes every file of `ix` as one segment (absolute paths) and returns its bytes.
fn encode_index(ix: &Index) -> Vec<u8> {
    let files: Vec<segment::EncFile<'_>> = (0..ix.file_count())
        .map(|f| {
            let ff = *ix.file_facts(f);
            let path = ix.file_path(f).as_os_str().as_bytes();
            let stamp = Stamp { size: u64::from(f) * 10, mtime_ns: 7, hash: u64::from(f) };
            segment::EncFile { path, ff, metrics: ff.defs.start - f, stamp, fps: &[] }
        })
        .collect();
    let mut map = segment::SymMap::default();
    let out = Cursor::new(Vec::new());
    let (out, _) = segment::encode(ix.strings(), ix.facts(), ix.metrics(), &files, None, &mut map, KIND_FACTS, 1, 2, out)
        .expect("encode");
    out.into_inner()
}

fn open_bytes(b: &[u8]) -> Result<Segment, CacheError> {
    Segment::from_reader(Reader::from_bytes(Bytes::copy_from(b), KIND_FACTS, 1, 2)?)
}

/// Loads `inputs` with every file a trusted hit in `seg` (one group).
fn load_from_segment(inputs: Inputs, seg: Segment) -> (Index, Vec<Stamp>, u32) {
    let plan = Plan { file: String::new(), key: 2, mode: Mode::Trusted, base: PathBuf::new() };
    let n = inputs.len() as u32;
    let lookup = Lookup::with_segments(vec![plan], vec![Some(seg)], &[n]);
    let (ix, _, stamps) = crate::index::load_cached(inputs, &LoadOptions::default(), Some(&lookup));
    let hits = lookup.hits[0].load(Ordering::Relaxed);
    (ix, stamps, hits)
}

/// Encodes every file of `ix` with `rows[f]` as file `f`'s fingerprints (key `[1, 2, 3]`).
fn encode_with_fps(ix: &Index, rows: &[Vec<crate::analysis::dupes::Fingerprint>]) -> Vec<u8> {
    let files: Vec<segment::EncFile<'_>> = (0..ix.file_count())
        .map(|f| {
            let ff = *ix.file_facts(f);
            let path = ix.file_path(f).as_os_str().as_bytes();
            segment::EncFile { path, ff, metrics: ff.defs.start - f, stamp: Stamp::default(), fps: &rows[f as usize] }
        })
        .collect();
    let mut map = segment::SymMap::default();
    let out = Cursor::new(Vec::new());
    let (out, _) =
        segment::encode(ix.strings(), ix.facts(), ix.metrics(), &files, Some([1, 2, 3]), &mut map, KIND_FACTS, 1, 2, out)
            .expect("encode");
    out.into_inner()
}

#[test]
fn segment_fingerprints_roundtrip_and_are_validated() {
    use crate::analysis::dupes::Fingerprint;
    let t = sample_project("segfp");
    let ix = plain_load(inputs(t.path()));
    let n = ix.file_count();
    let fp = |hash, pos| Fingerprint { hash, file: 99, pos };
    let mut rows: Vec<Vec<Fingerprint>> = (0..n).map(|f| (0..f).map(|i| fp(f * 100 + i, i * 3)).collect()).collect();
    let seg = open_bytes(&encode_with_fps(&ix, &rows)).expect("segment");
    assert_eq!(seg.fingerprint_key(), Some([1, 2, 3]));
    for f in 0..n {
        let mut out = Fingerprints::new();
        seg.copy_fingerprints(f, f + 1000, &mut out);
        let want: Vec<Fingerprint> = rows[f as usize].iter().map(|r| Fingerprint { file: f + 1000, ..*r }).collect();
        assert_eq!(out.as_slice(), &want[..]);
    }
    // No fingerprint sections: no key, nothing copied.
    let plain = open_bytes(&encode_index(&ix)).expect("segment");
    assert_eq!(plain.fingerprint_key(), None);
    let mut out = Fingerprints::new();
    plain.copy_fingerprints(n - 1, 0, &mut out);
    assert!(out.is_empty());
    // Positions out of order within a file are rejected (the matcher relies on the order).
    rows[n as usize - 1].swap(0, 1);
    assert!(matches!(open_bytes(&encode_with_fps(&ix, &rows)), Err(CacheError::Corrupt("fingerprints"))));
    rows[n as usize - 1].swap(0, 1);
    rows[n as usize - 1][1].pos = u32::MAX;
    assert!(matches!(open_bytes(&encode_with_fps(&ix, &rows)), Err(CacheError::Corrupt("fingerprints"))));
}

#[test]
fn colliding_path_hashes_are_both_found() {
    // Two real paths (of the msgraph SDK) with the same `hash_bytes`.
    let dir = "/Users/bt386671/Code/gsk/aiml-lectio/.venv/lib/python3.12/site-packages/msgraph/generated/drives/item/items/item/workbook/functions";
    let p1 = format!("{dir}/im_csch/im_csch_request_builder.py");
    let p2 = format!("{dir}/im_sech/im_sech_request_builder.py");
    assert_eq!(crate::intern::hash_bytes(p1.as_bytes()), crate::intern::hash_bytes(p2.as_bytes()));
    let t = sample_project("collide");
    let ix = plain_load(inputs(t.path()));
    let files: Vec<segment::EncFile<'_>> = (0..ix.file_count())
        .map(|f| {
            let ff = *ix.file_facts(f);
            let path = match f {
                0 => p1.as_bytes(),
                1 => p2.as_bytes(),
                _ => ix.file_path(f).as_os_str().as_bytes(),
            };
            segment::EncFile { path, ff, metrics: ff.defs.start - f, stamp: Stamp::default(), fps: &[] }
        })
        .collect();
    let mut map = segment::SymMap::default();
    let (out, _) = segment::encode(ix.strings(), ix.facts(), ix.metrics(), &files, None, &mut map, KIND_FACTS, 1, 2, Cursor::new(Vec::new()))
        .expect("encode");
    let seg = open_bytes(&out.into_inner()).expect("segment");
    assert_eq!(seg.find(p1.as_bytes()), Some(0));
    assert_eq!(seg.find(p2.as_bytes()), Some(1));
    assert_eq!(seg.find(format!("{dir}/im_sech/other.py").as_bytes()), None);
    for f in 2..ix.file_count() {
        assert_eq!(seg.find(ix.file_path(f).as_os_str().as_bytes()), Some(f));
    }
}

#[test]
fn segment_roundtrip_through_the_loader() {
    let t = sample_project("seg");
    let ix = plain_load(inputs(t.path()));
    let seg = open_bytes(&encode_index(&ix)).expect("segment");
    assert_eq!(seg.file_count(), ix.file_count());
    for f in 0..ix.file_count() {
        let p = ix.file_path(f).as_os_str().as_bytes();
        assert_eq!(seg.find(p), Some(f));
        assert_eq!(seg.path(f), p);
        assert_eq!(seg.stamp(f), Stamp { size: u64::from(f) * 10, mtime_ns: 7, hash: u64::from(f) });
    }
    assert_eq!(seg.find(b"/nope.py"), None);
    // Only the strings the files use, each once.
    assert!(seg.string_count() as usize <= ix.strings().len());
    let mut seen = crate::intern::FxHashSet::default();
    for k in 0..seg.string_count() as usize {
        assert!(seen.insert(seg.string(k).to_vec()), "duplicate string");
    }

    let (warm, stamps, hits) = load_from_segment(inputs(t.path()), seg);
    assert_eq!(hits, ix.file_count());
    assert_eq!(stamps.len(), ix.file_count() as usize);
    assert_eq!(dump(&warm), dump(&ix));
    assert_eq!(warm.stats().lines, ix.stats().lines);
}

#[test]
fn segment_of_corpus_roundtrips() {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    let ix = plain_load(inputs(&corpus));
    assert!(ix.file_count() > 10);
    let seg = open_bytes(&encode_index(&ix)).expect("segment");
    let (warm, _, hits) = load_from_segment(inputs(&corpus), seg);
    assert_eq!(hits, ix.file_count());
    assert_eq!(dump(&warm), dump(&ix));
}

/// Offset of section `id` in a container (from its table).
fn section_offset(b: &[u8], id: u32) -> usize {
    let table = u64::from_le_bytes(b[48..56].try_into().expect("8")) as usize;
    let n = u32::from_le_bytes(b[56..60].try_into().expect("4")) as usize;
    for i in 0..n {
        let e = table + i * 24;
        if u32::from_le_bytes(b[e..e + 4].try_into().expect("4")) == id {
            return u64::from_le_bytes(b[e + 8..e + 16].try_into().expect("8")) as usize;
        }
    }
    panic!("no section {id}");
}

#[test]
fn segment_validation_rejects_bad_references() {
    let t = sample_project("segbad");
    let ix = plain_load(inputs(t.path()));
    let good = encode_index(&ix);
    // Overwrites u32 `at` of section `id` and re-seals the checksum, so that only the
    // structural validation can catch it.
    let patch = |id: u32, at: usize, v: u32| -> Result<Segment, CacheError> {
        let mut b = good.clone();
        let pos = section_offset(&b, id) + at * 4;
        b[pos..pos + 4].copy_from_slice(&v.to_le_bytes());
        let sum = format::checksum(&b[format::HEADER_LEN..]);
        b[40..48].copy_from_slice(&sum.to_le_bytes());
        open_bytes(&b)
    };
    assert!(patch(18, 1, 0).is_ok(), "a None name is fine");
    assert!(matches!(patch(18, 1, u32::MAX), Err(CacheError::Corrupt(_))), "name id out of range");
    assert!(matches!(patch(19, 1, 1000), Err(CacheError::Corrupt(_))), "parent outside the file");
    assert!(matches!(patch(56, 0, 1000), Err(CacheError::Corrupt(_))), "import scope outside the file");
    assert!(matches!(patch(segment::STR_OFFS, 1, u32::MAX), Err(CacheError::Corrupt(_))), "string offsets");
    assert!(matches!(patch(segment::FILE_HDR, 1, 0), Err(CacheError::Corrupt(_))), "file without defs");
    assert!(matches!(patch(segment::FILE_HDR, 0, u32::MAX), Err(CacheError::Corrupt(_))), "run overflow");
    assert!(matches!(patch(segment::FILE_HDR, 1, 100_000), Err(CacheError::Corrupt(_))), "run past the column");
}

#[test]
fn def_parents_must_form_a_preorder_tree() {
    use crate::extract::facts::NO_PARENT as N;
    let ok = |p: &[u32]| segment::preorder(p, &mut Vec::new());
    assert!(ok(&[N]));
    // <module>; f; f.g; f.g.h; k (back to the module); k.m
    assert!(ok(&[N, 0, 1, 2, 0, 4]));
    assert!(!ok(&[]), "no <module> row");
    assert!(!ok(&[0]), "<module> with a parent");
    assert!(!ok(&[N, N]), "second root");
    assert!(!ok(&[N, 1]), "own parent");
    assert!(!ok(&[N, 2, 0]), "parent after the row");
    assert!(!ok(&[N, 0, 0, 1]), "parent's subtree already closed");
}

/// Random words of a valid segment overwritten (checksum re-sealed, so only the structural
/// validation stands between the bytes and the loader): opening either fails cleanly or the
/// segment loads into an index without panicking.
#[test]
fn corrupted_segments_never_panic() {
    let t = sample_project("segfuzz");
    let ix = plain_load(inputs(t.path()));
    let good = encode_index(&ix);
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let words = (good.len() - format::HEADER_LEN) / 4;
    let (mut opened, mut rejected) = (0, 0);
    // `PYMAPPER_CACHE_FUZZ_ITERS` runs a longer campaign.
    let iters = std::env::var("PYMAPPER_CACHE_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(600u32);
    for _ in 0..iters {
        let mut b = good.clone();
        for _ in 0..1 + next() % 3 {
            let pos = format::HEADER_LEN + (next() as usize % words) * 4;
            let r = next();
            // Small values hit ids, lengths and offsets near their bounds; others are noise.
            let v = match r % 4 {
                0 => (r >> 8) as u32 % 8,
                1 => u32::from_le_bytes(b[pos..pos + 4].try_into().expect("4")).wrapping_add(1),
                2 => u32::MAX - (r >> 8) as u32 % 4,
                _ => (r >> 16) as u32,
            };
            b[pos..pos + 4].copy_from_slice(&v.to_le_bytes());
        }
        let sum = format::checksum(&b[format::HEADER_LEN..]);
        b[40..48].copy_from_slice(&sum.to_le_bytes());
        match open_bytes(&b) {
            Ok(seg) => {
                opened += 1;
                let (warm, _, _) = load_from_segment(inputs(t.path()), seg);
                assert_eq!(warm.file_count(), ix.file_count());
            }
            Err(_) => rejected += 1,
        }
    }
    // Both outcomes are exercised (most payload words are plain data that loads as-is).
    assert!(opened > 0 && rejected > 0, "opened {opened}, rejected {rejected}");
}

// -------------------------------------------------------------------------------------------
// End to end: cache::load
// -------------------------------------------------------------------------------------------

fn pmc_files(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.ends_with(".pmc")).collect())
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn cold_then_warm_load_is_identical() {
    let t = sample_project("warm");
    let cache = Cache::new(t.path().join(".pymapper_cache"));
    let reference = dump(&plain_load(inputs(t.path())));
    let n = 7;

    let (cold, s) = cached(t.path(), &cache);
    assert_eq!(dump(&cold), reference);
    assert_eq!((s.segments, s.opened, s.invalid, s.hits, s.verified, s.misses), (1, 0, 0, 0, 0, n));
    assert_eq!(s.written, 1);
    assert!(s.written_bytes > 0 && s.write_error.is_none());
    let gi = fs::read_to_string(cache.dir().join(".gitignore")).expect(".gitignore written");
    assert!(gi.lines().any(|l| l == "*"));
    assert_eq!(pmc_files(cache.dir()).len(), 1);

    let (warm, s) = cached(t.path(), &cache);
    assert_eq!(dump(&warm), reference);
    assert_eq!((s.opened, s.hits, s.verified, s.misses, s.written), (1, n, 0, 0, 0));
    assert_eq!(warm.stats().bytes, cold.stats().bytes);

    // Same result on one thread (determinism of the merge with segment interners).
    let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().expect("pool");
    let (one, _) = pool.install(|| cached(t.path(), &cache));
    assert_eq!(dump(&one), reference);
}

#[test]
fn changed_files_are_detected() {
    let t = sample_project("changes");
    let cache = Cache::new(t.path().join("c"));
    let _ = cached(t.path(), &cache);

    // Same size, other content, other mtime: content hash differs -> extracted.
    t.file("pkg/sibling.py", "def helpe2():\n    return 2\n");
    let (ix, s) = cached(t.path(), &cache);
    assert_eq!((s.hits, s.verified, s.misses, s.written), (6, 0, 1, 1));
    assert_eq!(dump(&ix), dump(&plain_load(inputs(t.path()))));

    // Only the mtime changes: verified by content, then re-stamped.
    set_old_mtime(&t.path().join("pkg/mod.py"), 7200);
    let (_, s) = cached(t.path(), &cache);
    assert_eq!((s.hits, s.verified, s.misses, s.written), (6, 1, 0, 1));
    let (_, s) = cached(t.path(), &cache);
    assert_eq!((s.hits, s.verified, s.misses, s.written), (7, 0, 0, 0));

    // Other size.
    t.file("empty.py", "x = 1\n");
    let (ix, s) = cached(t.path(), &cache);
    assert_eq!((s.hits, s.misses), (6, 1));
    assert_eq!(dump(&ix), dump(&plain_load(inputs(t.path()))));

    // A deleted file is dropped from the segment, a new one extracted.
    fs::remove_file(t.path().join("tests/test_mod.py")).expect("rm");
    t.file("pkg/new.py", "class N:\n    pass\n");
    let (ix, s) = cached(t.path(), &cache);
    assert_eq!((s.hits, s.misses, s.written), (6, 1, 1));
    assert_eq!(dump(&ix), dump(&plain_load(inputs(t.path()))));
    let (_, s) = cached(t.path(), &cache);
    assert_eq!((s.hits, s.misses, s.written), (7, 0, 0));
}

#[test]
fn racy_mtimes_are_verified_by_content() {
    let t = sample_project("racy");
    // Written just now: within RACY_NS of the cache write.
    fs::write(t.path().join("fresh.py"), "def f():\n    pass\n").expect("write");
    let cache = Cache::new(t.path().join("c"));
    let _ = cached(t.path(), &cache);
    let (_, s) = cached(t.path(), &cache);
    assert_eq!((s.hits, s.verified, s.misses), (7, 1, 0));
}

/// Duplication fingerprints of an uncached load of `root` with `opts`.
fn fresh_fps(root: &Path, opts: &LoadOptions) -> Vec<crate::analysis::dupes::Fingerprint> {
    crate::index::load_with(inputs(root), opts).1.expect("fingerprints").as_slice().to_vec()
}

/// [`cached`] with `opts`; returns the fingerprints as a vector.
fn cached_with(root: &Path, cache: &Cache, opts: &LoadOptions) -> (Index, CacheStats, Vec<crate::analysis::dupes::Fingerprint>) {
    let (ix, s, fps) = load_with(inputs(root), None, cache, opts);
    let fps = fps.map(|f| f.as_slice().to_vec()).unwrap_or_default();
    (ix, s, fps)
}

#[test]
fn fingerprints_are_cached_with_the_facts() {
    use crate::analysis::dupes::DupesParams;
    let t = sample_project("fps");
    // A second copy of the sample so that fingerprints repeat across files.
    t.file("pkg/copy.py", SAMPLE);
    set_old_mtime(&t.path().join("pkg/copy.py"), 3600);
    let cache = Cache::new(t.path().join("c"));
    let n = 8;
    let opts = LoadOptions { dupes: Some(DupesParams::new(10, 2)) };
    let reference = fresh_fps(t.path(), &opts);
    assert!(reference.len() > 10, "{} fingerprints", reference.len());

    let (_, s, cold) = cached_with(t.path(), &cache, &opts);
    assert_eq!((s.hits, s.misses, s.written), (0, n, 1));
    assert_eq!(cold, reference);
    // Warm: every file is a hit and gets its stored fingerprints.
    let (warm_ix, s, warm) = cached_with(t.path(), &cache, &opts);
    assert_eq!((s.hits, s.verified, s.misses, s.written), (n, 0, 0, 0));
    assert_eq!(warm, reference);
    assert_eq!(dump(&warm_ix), dump(&plain_load(inputs(t.path()))));

    // A changed file is extracted (and fingerprinted); the others keep their stored rows.
    t.file("pkg/sibling.py", "def helper(a, b, c):\n    return [a + b * c for _ in range(a)] + [b, c, a, b]\n");
    set_old_mtime(&t.path().join("pkg/sibling.py"), 3600);
    let reference = fresh_fps(t.path(), &opts);
    let (_, s, fps) = cached_with(t.path(), &cache, &opts);
    assert_eq!((s.hits, s.misses, s.written), (n - 1, 1, 1));
    assert_eq!(fps, reference);
    let (_, s, fps) = cached_with(t.path(), &cache, &opts);
    assert_eq!((s.hits, s.misses, s.written), (n, 0, 0));
    assert_eq!(fps, reference);

    // Without dupes the cached fingerprints are not needed and stay stored.
    let (_, s, fps) = cached_with(t.path(), &cache, &LoadOptions::default());
    assert_eq!((s.hits, s.misses, s.written), (n, 0, 0));
    assert!(fps.is_empty());
    let (_, s, _) = cached_with(t.path(), &cache, &opts);
    assert_eq!((s.hits, s.misses), (n, 0));

    // Other thresholds (other k / w): stored rows do not fit, everything is extracted once.
    let other = LoadOptions { dupes: Some(DupesParams::new(16, 2)) };
    let reference16 = fresh_fps(t.path(), &other);
    assert_ne!(reference16, reference);
    let (_, s, fps) = cached_with(t.path(), &cache, &other);
    assert_eq!((s.hits, s.misses, s.written), (0, n, 1));
    assert_eq!(fps, reference16);
    let (_, s, fps) = cached_with(t.path(), &cache, &other);
    assert_eq!((s.hits, s.misses), (n, 0));
    assert_eq!(fps, reference16);

    // A load without dupes that rewrites the segment drops the fingerprints: the next load
    // with dupes extracts again, then hits.
    t.file("empty.py", "y = 2\n");
    set_old_mtime(&t.path().join("empty.py"), 3600);
    let (_, s, _) = cached_with(t.path(), &cache, &LoadOptions::default());
    assert_eq!((s.misses, s.written), (1, 1));
    let reference16 = fresh_fps(t.path(), &other);
    let (_, s, fps) = cached_with(t.path(), &cache, &other);
    assert_eq!((s.hits, s.misses, s.written), (0, n, 1));
    assert_eq!(fps, reference16);
    let (_, s, fps) = cached_with(t.path(), &cache, &other);
    assert_eq!((s.hits, s.misses), (n, 0));
    assert_eq!(fps, reference16);
}

#[test]
fn third_party_groups_store_no_fingerprints() {
    use crate::analysis::dupes::DupesParams;
    let t = sample_project("fps3p");
    let cache = Cache::new(t.path().join("c"));
    let w = walk(&[t.path()], &WalkOptions::everything());
    let inp = || Inputs::from_root_dir(t.path(), &w.files, Origin::ThirdParty, b"lib");
    let opts = LoadOptions { dupes: Some(DupesParams::new(10, 2)) };
    let (_, s, fps) = load_with(inp(), None, &cache, &opts);
    assert_eq!((s.misses, s.written), (7, 1));
    assert!(fps.expect("requested").is_empty());
    // Third-party hits need no fingerprints.
    let (_, s, fps) = load_with(inp(), None, &cache, &opts);
    assert_eq!((s.hits, s.misses), (7, 0));
    assert!(fps.expect("requested").is_empty());
}

#[test]
fn corrupt_or_foreign_segments_are_rebuilt() {
    let t = sample_project("corrupt");
    let cache = Cache::new(t.path().join("c"));
    let _ = cached(t.path(), &cache);
    let seg = cache.dir().join(&pmc_files(cache.dir())[0]);
    let mut b = fs::read(&seg).expect("read");
    let mid = b.len() / 2;
    b[mid] ^= 0xff;
    fs::write(&seg, &b).expect("write");
    let (ix, s) = cached(t.path(), &cache);
    assert_eq!((s.opened, s.invalid, s.hits, s.misses, s.written), (0, 1, 0, 7, 1));
    assert_eq!(dump(&ix), dump(&plain_load(inputs(t.path()))));
    // Truncated to garbage.
    fs::write(&seg, b"PYMCACHE").expect("write");
    let (_, s) = cached(t.path(), &cache);
    assert_eq!((s.invalid, s.misses, s.written), (1, 7, 1));
    let (_, s) = cached(t.path(), &cache);
    assert_eq!((s.opened, s.hits), (1, 7));
}

#[test]
fn unwritable_cache_never_fails_the_load() {
    let t = sample_project("rofs");
    // A file where the directory should be.
    fs::write(t.path().join("c"), "not a dir").expect("write");
    let cache = Cache::new(t.path().join("c"));
    let (ix, s) = cached(t.path(), &cache);
    assert!(s.write_error.is_some());
    assert_eq!(s.written, 0);
    assert_eq!(dump(&ix), dump(&plain_load(inputs(t.path()))));
}

#[test]
fn unreadable_files_are_not_cached() {
    let t = sample_project("unread");
    let mut inp = inputs(t.path());
    let g = 0;
    inp.push(t.path().join("missing.py").as_os_str().as_bytes(), g, b"missing", crate::roots::FileStatus::Active)
        .expect("push");
    let cache = Cache::new(t.path().join("c"));
    let (ix, s) = load(inp, None, &cache);
    assert_eq!(ix.read_errors().len(), 1);
    assert_eq!(s.misses, 8);
    assert_eq!(open_dir_segment(&cache).file_count(), 7);
}

fn open_dir_segment(cache: &Cache) -> Segment {
    let name = &pmc_files(cache.dir())[0];
    let key = Plan::for_group(&Group { name: b"proj".to_vec().into(), origin: Origin::FirstParty, dist: None }, None).key;
    Segment::open(&cache.dir().join(name), KIND_FACTS, schema(), key).expect("segment")
}

#[test]
fn clear_and_config() {
    let t = sample_project("clear");
    let mut cfg = Config::new(t.path());
    let cache = Cache::from_config(&cfg).expect("enabled by default");
    assert_eq!(cache.dir(), t.path().join(crate::config::DEFAULT_CACHE_DIR));
    cache.clear().expect("clear of a missing dir is fine");
    let _ = cached(t.path(), &cache);
    assert_eq!(pmc_files(cache.dir()).len(), 1);
    cache.clear().expect("clear");
    assert!(pmc_files(cache.dir()).is_empty());
    assert!(cache.dir().join(".gitignore").exists());
    cfg.cache = false;
    assert!(Cache::from_config(&cfg).is_none());
}

/// A distribution in `site` owning `files` (`path=contents`), with a `RECORD`.
fn dist(t: &TempDir, site: &str, name: &str, files: &[&str], extra_record: &str) {
    let info = format!("{site}/{}-1.0.dist-info", name.replace('-', "_"));
    t.file(&format!("{info}/METADATA"), &format!("Metadata-Version: 2.1\nName: {name}\nVersion: 1.0\n"));
    let mut record = String::new();
    for f in files {
        let (path, contents) = f.split_once('=').unwrap_or((f, ""));
        t.file(&format!("{site}/{path}"), contents);
        record.push_str(&format!("{path},sha256=x,1\n"));
    }
    record.push_str(extra_record);
    t.file(&format!("{info}/RECORD"), &record);
}

#[test]
fn distributions_and_stdlib_are_trusted_by_key() {
    use crate::roots::{RootOptions, discover};
    const SITE: &str = "proj/.venv/lib/python3.12/site-packages";
    const STDLIB: &str = "python/lib/python3.12";
    let t = TempDir::new("roots");
    let root = t.path().display().to_string();
    t.file("proj/pyproject.toml", "[project]\nname = \"Proj\"\n")
        .file("proj/proj/__init__.py", "import json\nimport google.protobuf\n")
        .file("proj/.venv/pyvenv.cfg", &format!("home = {root}/python/bin\nversion_info = 3.12.3\n"))
        .file(&format!("{STDLIB}/os.py"), "def getcwd():\n    pass\n")
        .file(&format!("{STDLIB}/json/__init__.py"), "def dumps(o):\n    return str(o)\n");
    fs::create_dir_all(t.path().join("python/bin")).expect("mkdir");
    let protobuf = ["google/protobuf/__init__.py=class Message:\n    pass\n"];
    dist(&t, SITE, "protobuf", &protobuf, "");
    dist(&t, SITE, "six", &["six.py=def b(s):\n    return s\n"], "");
    let opts = RootOptions {
        include_deps: true,
        include_stdlib: true,
        path_env: Some(Default::default()),
        ..RootOptions::default()
    };
    let run = |cache: &Cache| {
        let roots = discover(&t.path().join("proj"), &opts);
        assert!(roots.stdlib.is_some(), "fake stdlib found");
        let w = walk(&[t.path().join("proj"), t.path().join(STDLIB)], &WalkOptions::everything());
        let reference = dump(&plain_load(Inputs::from_roots(&roots, &w.files)));
        let (ix, s) = load(Inputs::from_roots(&roots, &w.files), Some(&roots), cache);
        assert_eq!(dump(&ix), reference);
        s
    };
    let cache = Cache::new(t.path().join("c"));
    let s = run(&cache);
    assert_eq!((s.segments, s.hits, s.misses, s.written), (4, 0, 5, 4));
    let names = pmc_files(cache.dir());
    assert_eq!(names.len(), 4);
    assert!(names.iter().any(|n| n.starts_with("dist-protobuf@1.0@")));
    assert!(names.iter().any(|n| n.starts_with("dist-six@1.0@")));
    assert!(names.iter().any(|n| n.starts_with("stdlib-3.12-")));
    assert!(names.iter().any(|n| n.starts_with("files-")));

    let s = run(&cache);
    assert_eq!((s.opened, s.hits, s.verified, s.misses, s.written), (4, 5, 0, 0, 0));

    // A trusted file edited without its RECORD changing is not even looked at.
    t.file(&format!("{SITE}/six.py"), "def b(s, t):\n    return s\n");
    let roots = discover(&t.path().join("proj"), &opts);
    let w = walk(&[t.path().join("proj"), t.path().join(STDLIB)], &WalkOptions::everything());
    let (_, s) = load(Inputs::from_roots(&roots, &w.files), Some(&roots), &cache);
    assert_eq!((s.hits, s.misses), (5, 0));

    // A new RECORD (reinstall, upgrade) is a new key: re-extracted, the old segment removed.
    dist(&t, SITE, "six", &["six.py=def b(s, t):\n    return s\n"], "six-1.0.dist-info/INSTALLER,,\n");
    let s = run(&cache);
    assert_eq!((s.hits, s.misses, s.written), (4, 1, 1));
    let names = pmc_files(cache.dir());
    assert_eq!(names.iter().filter(|n| n.starts_with("dist-six@")).count(), 1);

    // A patch upgrade of the interpreter rewrites os.py: new stdlib key.
    set_old_mtime(&t.path().join(STDLIB).join("os.py"), 60);
    let s = run(&cache);
    assert_eq!((s.hits, s.misses, s.written), (3, 2, 1));
    assert_eq!(pmc_files(cache.dir()).iter().filter(|n| n.starts_with("stdlib-")).count(), 1);
}

#[test]
fn stored_paths_and_families() {
    assert_eq!(stored_path(b"/a/b/c.py", b"/a"), b"b/c.py");
    assert_eq!(stored_path(b"/a/b/c.py", b"/a/"), b"b/c.py");
    assert_eq!(stored_path(b"/ab/c.py", b"/a"), b"/ab/c.py");
    assert_eq!(stored_path(b"/a", b"/a"), b"/a");
    assert_eq!(stored_path(b"/a/b.py", b""), b"/a/b.py");
    assert_eq!(family("dist-foo-bar@1.0@0123.pmc"), Some("dist-foo-bar@"));
    assert_eq!(family("stdlib-3.13-abcd-0123.pmc"), Some("stdlib-3.13-abcd-"));
    assert_eq!(family("files-0123.pmc"), None);
    assert_eq!(sanitize(b"1.0+local/x y"), "1.0+local_x_y");
}

#[test]
fn superseded_segments_are_removed_but_used_ones_kept() {
    let t = TempDir::new("superseded");
    let cache = Cache::new(t.path());
    let names = [
        "dist-foo@0.9@0001.pmc",
        "dist-foo@1.0@0002.pmc",
        "dist-foo@2.0@0003.pmc",
        "dist-bar@1.0@0004.pmc",
        "stdlib-3.13-abcd-0005.pmc",
        "files-0006.pmc",
    ];
    for n in names {
        fs::write(t.path().join(n), b"x").expect("write");
    }
    let plan = |file: &str| Plan { file: file.to_owned(), key: 0, mode: Mode::Trusted, base: PathBuf::new() };
    // foo 1.0 (system site-packages, a hit) and foo 2.0 (venv, just written) in one load.
    let plans = [plan("dist-foo@1.0@0002.pmc"), plan("dist-foo@2.0@0003.pmc"), plan("files-0007.pmc")];
    remove_superseded(&cache, &plans, &[1, 2]);
    let want = [
        "dist-bar@1.0@0004.pmc",
        "dist-foo@1.0@0002.pmc",
        "dist-foo@2.0@0003.pmc",
        "files-0006.pmc",
        "stdlib-3.13-abcd-0005.pmc",
    ];
    assert_eq!(pmc_files(t.path()), want);
}

#[test]
fn config_evidence_is_cached_by_size_and_mtime() {
    use crate::extract::facts::evidence_hash;
    let t = TempDir::new("cfgev");
    t.file("proj/a.yaml", "module_name: pkg.tools\n").file("proj/b.toml", "x = \"first_name\"\n");
    let cache = Cache::new(t.path().join("cache"));
    let mut paths = crate::walk::PathList::default();
    for f in ["proj/a.yaml", "proj/b.toml"] {
        paths.push(t.path().join(f).as_os_str().as_encoded_bytes()).expect("push");
    }
    let cold = config_evidence(&paths, Some(&cache));
    assert_eq!((cold.stats.files, cold.stats.cached), (2, 0));
    assert!(t.path().join("cache").join(CONFIG_EVIDENCE_FILE).exists());
    let warm = config_evidence(&paths, Some(&cache));
    assert_eq!((warm.stats.files, warm.stats.cached), (2, 2));
    assert_eq!(warm.file(0), cold.file(0));
    assert!(warm.contains(evidence_hash(b"pkg.tools")) && warm.contains(evidence_hash(b"first_name")));
    // A changed file is re-read; the other stays cached.
    t.file("proj/b.toml", "x = \"second_name_longer\"\n");
    let again = config_evidence(&paths, Some(&cache));
    assert_eq!((again.stats.files, again.stats.cached), (2, 1));
    assert!(again.contains(evidence_hash(b"second_name_longer")) && !again.contains(evidence_hash(b"first_name")));
    // A corrupt cache file is ignored.
    fs::write(t.path().join("cache").join(CONFIG_EVIDENCE_FILE), b"garbage").expect("write");
    let fresh = config_evidence(&paths, Some(&cache));
    assert_eq!((fresh.stats.files, fresh.stats.cached), (2, 0));
    // Without a cache: the same evidence.
    assert_eq!(config_evidence(&paths, None).file(0), cold.file(0));
}
