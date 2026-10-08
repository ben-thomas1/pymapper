//! Sequential reference walker for the `walk-seq` benchmark target: finds `.py`/`.pyi` files
//! under a root.
//!
//! Single-threaded, no gitignore, no symlink following, hidden directories entered. The other
//! targets use `pymapper_core::walk`; this one stays as the baseline its parallel speed-up is
//! measured against. Paths are stored compactly (one byte buffer + end offsets) rather than one
//! `PathBuf` allocation per file.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Append-only list of paths in a single byte buffer.
#[derive(Default)]
pub struct PathStore {
    bytes: Vec<u8>,
    ends: Vec<u32>,
}

impl PathStore {
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    #[cfg(test)]
    pub fn get(&self, i: usize) -> &Path {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        let end = self.ends[i] as usize;
        Path::new(OsStr::from_bytes(&self.bytes[start..end]))
    }

    /// Appends `dir/name`.
    pub fn push_join(&mut self, dir: &Path, name: &OsStr) -> io::Result<()> {
        let dir = dir.as_os_str().as_bytes();
        self.bytes.extend_from_slice(dir);
        if !dir.ends_with(b"/") {
            self.bytes.push(b'/');
        }
        self.bytes.extend_from_slice(name.as_bytes());
        match u32::try_from(self.bytes.len()) {
            Ok(end) => {
                self.ends.push(end);
                Ok(())
            }
            Err(_) => Err(io::Error::other("path store exceeds 4 GiB")),
        }
    }
}

/// Whether a file name is a Python source or stub.
pub fn is_python(name: &[u8]) -> bool {
    name.ends_with(b".py") || name.ends_with(b".pyi")
}

/// Directory names never descended into.
fn skip_dir(name: &[u8]) -> bool {
    name == b"__pycache__" || name == b".git"
}

/// Collects every Python file under `root` into `out`. Errors below the root are counted and
/// skipped (returned count); an unreadable root is an error.
pub fn walk_python(root: &Path, out: &mut PathStore) -> io::Result<u64> {
    let mut errors = 0u64;
    let mut stack: Vec<PathBuf> = Vec::new();
    let root_entries = fs::read_dir(root)?;
    scan_dir(root, root_entries, out, &mut stack, &mut errors)?;
    while let Some(dir) = stack.pop() {
        match fs::read_dir(&dir) {
            Ok(entries) => scan_dir(&dir, entries, out, &mut stack, &mut errors)?,
            Err(_) => errors += 1,
        }
    }
    Ok(errors)
}

fn scan_dir(
    dir: &Path,
    entries: fs::ReadDir,
    out: &mut PathStore,
    stack: &mut Vec<PathBuf>,
    errors: &mut u64,
) -> io::Result<()> {
    for entry in entries {
        let Ok(entry) = entry else {
            *errors += 1;
            continue;
        };
        // DirEntry::file_type does not follow symlinks, so links are neither files nor dirs here.
        let Ok(ty) = entry.file_type() else {
            *errors += 1;
            continue;
        };
        let name = entry.file_name();
        if ty.is_dir() {
            if !skip_dir(name.as_bytes()) {
                stack.push(entry.path());
            }
        } else if ty.is_file() && is_python(name.as_bytes()) {
            out.push_join(dir, &name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-walk-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn collect(store: &PathStore) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = (0..store.len()).map(|i| store.get(i).to_path_buf()).collect();
        v.sort();
        v
    }

    #[test]
    fn path_store_round_trips() {
        let mut s = PathStore::default();
        s.push_join(Path::new("/a/b"), OsStr::new("c.py")).unwrap();
        s.push_join(Path::new("/"), OsStr::new("d.pyi")).unwrap();
        s.push_join(Path::new("rel/"), OsStr::new("e.py")).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s.get(0), Path::new("/a/b/c.py"));
        assert_eq!(s.get(1), Path::new("/d.pyi"));
        assert_eq!(s.get(2), Path::new("rel/e.py"));
    }

    #[test]
    fn python_names() {
        assert!(is_python(b"a.py"));
        assert!(is_python(b"a.pyi"));
        assert!(is_python(b".py"));
        assert!(!is_python(b"a.pyc"));
        assert!(!is_python(b"a.py.bak"));
        assert!(!is_python(b"a.ipynb"));
        assert!(!is_python(b"py"));
    }

    #[test]
    fn walks_nested_python_files_only() {
        let root = tempdir("nested");
        fs::create_dir_all(root.join("pkg/sub")).unwrap();
        fs::create_dir_all(root.join("__pycache__")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("top.py"), "x = 1\n").unwrap();
        fs::write(root.join("pkg/__init__.py"), "").unwrap();
        fs::write(root.join("pkg/sub/stub.pyi"), "def f() -> int: ...\n").unwrap();
        fs::write(root.join("pkg/readme.txt"), "no").unwrap();
        fs::write(root.join("__pycache__/cached.py"), "").unwrap();
        fs::write(root.join(".git/hook.py"), "").unwrap();
        // Non-UTF-8 file names are walked where the filesystem allows them (APFS refuses).
        let bad = root.join(OsStr::from_bytes(b"bad\xff.py"));
        let have_bad = fs::write(&bad, "").is_ok();
        // Symlinks are not followed, neither to files nor to directories.
        std::os::unix::fs::symlink(root.join("top.py"), root.join("link.py")).unwrap();
        std::os::unix::fs::symlink(root.join("pkg"), root.join("linkdir")).unwrap();

        let mut store = PathStore::default();
        let errors = walk_python(&root, &mut store).expect("walk");
        assert_eq!(errors, 0);
        let mut want = vec![root.join("top.py"), root.join("pkg/__init__.py"), root.join("pkg/sub/stub.pyi")];
        if have_bad {
            want.push(bad);
        }
        want.sort();
        assert_eq!(collect(&store), want);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_root_is_an_error() {
        let mut store = PathStore::default();
        assert!(walk_python(Path::new("/definitely/not/here/xtask"), &mut store).is_err());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn empty_root_yields_nothing() {
        let root = tempdir("empty");
        let mut store = PathStore::default();
        assert_eq!(walk_python(&root, &mut store).unwrap(), 0);
        assert_eq!(store.len(), 0);
        fs::remove_dir_all(&root).unwrap();
    }
}
