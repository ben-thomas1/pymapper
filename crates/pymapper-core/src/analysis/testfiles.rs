//! Test-code detection, for `analysis.include-tests-in-metrics`.
//!
//! By default the metric analyses (hotspot PM1xx, coupling PM2xx, duplication PM5xx) skip test
//! code: long test functions, many-method test classes and copy-pasted test setup are normal
//! and drown the findings about production code. Dead code (PM4xx) and cycles / contracts
//! (PM3xx) still apply to tests.
//!
//! A file is test code when
//!
//! - a component of its dotted module name is `tests`, `test` or `testing` (a directory named
//!   so anywhere below its import root: `pkg/tests/util.py` is `pkg.tests.util`), or one of the
//!   private spellings `_tests` / `_testing` (test-support packages shipped inside a library,
//!   such as `pandas/_testing`: assertion helpers and fixtures, long and repetitive by nature),
//!   or
//! - its module's last component starts with `test_`, ends with `_test` or is `conftest`, or
//! - its file name is `test_*.py`, `*_test.py` or `conftest.py` (also `.pyi`; this catches
//!   files whose module name is unknown, e.g. not importable).
//!
//! Module names, not paths, are matched so that directories above the import root (a project
//! checked out under `~/tests/`) never count. Cost: one pass over the file table, a few byte
//! compares per module name component; one `bool` per file.

use std::os::unix::ffi::OsStrExt;

use crate::index::{FileId, Index};

/// Whether a dotted module name names test code (see the module docs).
pub fn is_test_module(module: &[u8]) -> bool {
    if module.is_empty() {
        return false;
    }
    let mut last: &[u8] = &[];
    for part in module.split(|&b| b == b'.') {
        if matches!(part, b"tests" | b"test" | b"testing" | b"_tests" | b"_testing") {
            return true;
        }
        last = part;
    }
    is_test_stem(last)
}

/// `test_*`, `*_test` or `conftest`.
fn is_test_stem(stem: &[u8]) -> bool {
    stem.starts_with(b"test_") || stem.ends_with(b"_test") || stem == b"conftest"
}

/// Whether a file path's name is `test_*.py`, `*_test.py` or `conftest.py` (or `.pyi`).
pub fn is_test_file_name(path: &[u8]) -> bool {
    let name = match path.iter().rposition(|&b| b == b'/') {
        Some(i) => &path[i + 1..],
        None => path,
    };
    let stem = name.strip_suffix(b".py").or_else(|| name.strip_suffix(b".pyi"));
    stem.is_some_and(is_test_stem)
}

/// Whether file `f` of `ix` is test code.
pub fn is_test_file(ix: &Index, f: FileId) -> bool {
    is_test_module(ix.str(ix.file_module(f))) || is_test_file_name(ix.file_path(f).as_os_str().as_bytes())
}

/// Per file of `ix`: whether metric findings are skipped in it, i.e. it is test code and
/// `include_tests` is off. Empty when `include_tests` is on (callers index with `get`).
pub fn excluded_files(ix: &Index, include_tests: bool) -> Vec<bool> {
    if include_tests {
        return Vec::new();
    }
    (0..ix.file_count()).map(|f| is_test_file(ix, f)).collect()
}

/// Whether `excluded` (from [`excluded_files`]) excludes file `f`.
#[inline]
pub fn is_excluded(excluded: &[bool], f: FileId) -> bool {
    excluded.get(f as usize).copied().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_names() {
        for m in [
            "tests",
            "tests.test_x",
            "pkg.tests",
            "pkg.tests.util",
            "pkg.sub.test.deep.helpers",
            "pkg.testing",
            "pkg.testing.fixtures",
            "pandas._testing",
            "pandas._testing.asserters",
            "pkg._tests",
            "pkg._tests.helpers",
            "test_x",
            "pkg.test_x",
            "pkg.x_test",
            "conftest",
            "pkg.sub.conftest",
        ] {
            assert!(is_test_module(m.as_bytes()), "{m}");
        }
        for m in ["", "pkg", "pkg.latest", "pkg.contest", "pkg.testx", "pkg.attest", "pkg.tests_helpers", "pkg.__testing", "pkg.testing_", "pkg._testing_utils", "testament.x"] {
            assert!(!is_test_module(m.as_bytes()), "{m}");
        }
    }

    #[test]
    fn file_names() {
        for p in ["/a/b/test_x.py", "test_x.py", "/a/x_test.py", "/a/conftest.py", "/a/test_x.pyi"] {
            assert!(is_test_file_name(p.as_bytes()), "{p}");
        }
        // Directories are matched through module names, never through paths (a checkout
        // under `~/tests/` is not test code).
        for p in ["/home/u/tests/proj/app.py", "/a/contest.py", "/a/test_x.txt", "/a/test_x.py/b.py", "/a/test.py"] {
            assert!(!is_test_file_name(p.as_bytes()), "{p}");
        }
    }
}
