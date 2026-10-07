//! Parallel collection of `.py` / `.pyi` files for `diff-ruff`.
//!
//! Dev-tooling walker: no gitignore handling (the product walker lives in
//! `pymapper-core::walk`). Symlinks below the root are never followed; `__pycache__` and `.git`
//! are not entered (the same rules as the bench walker, so file counts agree).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Files found plus walk errors (`path: error`), both sorted for deterministic output.
pub struct Walked {
    pub files: Vec<PathBuf>,
    pub errors: Vec<String>,
}

fn is_python(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()), Some("py" | "pyi"))
}

fn skip_dir(name: &[u8]) -> bool {
    name == b"__pycache__" || name == b".git"
}

/// Walks `root` (a directory or a single file).
pub fn walk(root: &Path) -> std::io::Result<Walked> {
    // The root itself is followed if it is a symlink (it was named explicitly); nothing below it is.
    let meta = fs::metadata(root)?;
    if !meta.is_dir() {
        return Ok(Walked { files: vec![root.to_path_buf()], errors: Vec::new() });
    }
    let files = Mutex::new(Vec::new());
    let errors = Mutex::new(Vec::new());
    rayon::scope(|s| visit(s, root.to_path_buf(), &files, &errors));
    let mut files = files.into_inner().unwrap_or_else(|e| e.into_inner());
    let mut errors = errors.into_inner().unwrap_or_else(|e| e.into_inner());
    files.sort_unstable();
    errors.sort_unstable();
    Ok(Walked { files, errors })
}

fn visit<'s>(s: &rayon::Scope<'s>, dir: PathBuf, files: &'s Mutex<Vec<PathBuf>>, errors: &'s Mutex<Vec<String>>) {
    let rd = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) => {
            push(errors, format!("{}: {e}", dir.display()));
            return;
        }
    };
    let mut local = Vec::new();
    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                push(errors, format!("{}: {e}", dir.display()));
                continue;
            }
        };
        // `DirEntry::file_type` does not follow symlinks.
        let Ok(ty) = entry.file_type() else { continue };
        let path = entry.path();
        if ty.is_dir() {
            if !skip_dir(entry.file_name().as_encoded_bytes()) {
                s.spawn(move |s| visit(s, path, files, errors));
            }
        } else if ty.is_file() && is_python(&path) {
            local.push(path);
        }
    }
    if !local.is_empty() {
        files.lock().unwrap_or_else(|e| e.into_inner()).append(&mut local);
    }
}

fn push(errors: &Mutex<Vec<String>>, msg: String) {
    errors.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_python_files_and_skips_caches() {
        let root = std::env::temp_dir().join(format!("pymapper-walk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for d in ["pkg/sub", "pkg/__pycache__", ".git", ".hidden", "empty"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in ["a.py", "pkg/b.pyi", "pkg/sub/c.py", "pkg/__pycache__/d.py", ".git/e.py", ".hidden/f.py", "notes.txt", "pkg/x.pyc"] {
            fs::write(root.join(f), "").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("pkg"), root.join("link")).unwrap();
        let w = walk(&root).unwrap();
        let rel: Vec<String> =
            w.files.iter().map(|p| p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(rel, [".hidden/f.py", "a.py", "pkg/b.pyi", "pkg/sub/c.py"]);
        assert!(w.errors.is_empty());
        // A file root yields just that file.
        let one = walk(&root.join("a.py")).unwrap();
        assert_eq!(one.files, [root.join("a.py")]);
        assert!(walk(&root.join("missing")).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
