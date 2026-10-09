//! Which files of the new inputs keep their facts and which are re-extracted.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::index::{FileId, Index, Inputs, Src};
use crate::intern::{FxHashMap, FxHashSet};
use crate::roots::Origin;

/// The update plan: a source per new file, and the files to re-extract (as inputs of the
/// fresh load, in new FileId order).
pub(super) struct Plan {
    pub src: Vec<Src>,
    pub fresh: Inputs,
    /// Old file of each new file (`u32::MAX`: added).
    pub old_of: Vec<u32>,
    /// New file of each old file (`u32::MAX`: removed).
    pub new_of: Vec<u32>,
    /// The inputs have the old paths, groups, module names and statuses, in the old order
    /// (only contents may have changed).
    pub same_meta: bool,
    /// `same_meta` and no file changed: nothing to do.
    pub noop: bool,
}

/// Matches `new` against the files of `old` by path. A file is re-extracted when it is new,
/// listed in `changed`, or out of order relative to the files kept before it (the fact store
/// keeps FileId order; walks are sorted, so this is rare).
pub(super) fn plan<P: AsRef<Path>>(old: &Index, new: &Inputs, changed: &[P]) -> Plan {
    let n = new.len();
    let old_n = old.file_count() as usize;
    let changed: FxHashSet<&[u8]> = changed.iter().map(|p| p.as_ref().as_os_str().as_bytes()).collect();
    let mut old_of = vec![u32::MAX; n];
    if old.same_paths(new) {
        for (i, o) in old_of.iter_mut().enumerate() {
            *o = i as u32;
        }
    } else {
        let mut by_path: FxHashMap<&[u8], FileId> = FxHashMap::default();
        by_path.reserve(old_n);
        for f in 0..old_n {
            by_path.insert(old.file_path(f as FileId).as_os_str().as_bytes(), f as FileId);
        }
        for (i, o) in old_of.iter_mut().enumerate() {
            if let Some(&f) = by_path.get(new.path(i as FileId).as_os_str().as_bytes()) {
                *o = f;
            }
        }
    }
    let mut src = Vec::with_capacity(n);
    let mut fresh = Inputs::new();
    let g = fresh.add_group(b"fresh", Origin::FirstParty, None);
    let mut new_of = vec![u32::MAX; old_n];
    let mut next_old = 0u32;
    let mut same_meta = n == old_n;
    for (i, &f) in old_of.iter().enumerate() {
        let path = new.path(i as FileId);
        let bytes = path.as_os_str().as_bytes();
        if f != u32::MAX {
            new_of[f as usize] = i as u32;
            same_meta &= f == i as u32
                && old.file_status(f) == new.status(i as FileId)
                && old.str(old.file_module(f)) == new.module(i as FileId)
                && old.groups()[old.file_group(f) as usize] == new.groups()[new.group(i as FileId) as usize];
        } else {
            same_meta = false;
        }
        if f != u32::MAX && f >= next_old && !changed.contains(bytes) {
            next_old = f + 1;
            src.push(Src::Old(f));
            continue;
        }
        let j = fresh
            .push(bytes, g, new.module(i as FileId), new.status(i as FileId))
            .expect("a subset of the paths of a valid `Inputs` fits a path list");
        src.push(Src::Fresh(j));
    }
    let same_meta = same_meta && old.groups() == new.groups();
    let noop = same_meta && fresh.is_empty();
    Plan { src, fresh, old_of, new_of, same_meta, noop }
}
