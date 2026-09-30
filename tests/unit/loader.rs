use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use py_mapper::loader::{
    collect_py_files, compute_base_path, load_modules, module_prefix_from_path,
};

fn fixture_path(relative: &str) -> String {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join(relative).to_str().unwrap().to_string()
}

#[test]
fn module_prefix_simple() {
    let prefix = module_prefix_from_path(Path::new("/base/foo.py"), Path::new("/base"));
    assert_eq!(prefix, "foo");
}

#[test]
fn module_prefix_nested() {
    let prefix = module_prefix_from_path(Path::new("/base/pkg/sub/mod.py"), Path::new("/base"));
    assert_eq!(prefix, "pkg.sub.mod");
}

#[test]
fn module_prefix_same_as_base() {
    let prefix = module_prefix_from_path(Path::new("/base/file.py"), Path::new("/base"));
    assert_eq!(prefix, "file");
}

#[test]
fn compute_base_path_single_dir() {
    let p = fixture_path("tests/fixtures");
    let base = compute_base_path(&[p.clone()]);
    assert_eq!(base, Path::new(&p));
}

#[test]
fn compute_base_path_single_file() {
    let p = fixture_path("tests/fixtures/simple.py");
    let base = compute_base_path(&[p]);
    assert_eq!(base, Path::new(&fixture_path("tests/fixtures")));
}

#[test]
fn compute_base_path_multiple() {
    let a = fixture_path("tests/fixtures/simple.py");
    let b = fixture_path("tests/fixtures/complex.py");
    let base = compute_base_path(&[a, b]);
    assert_eq!(base, Path::new(&fixture_path("tests/fixtures")));
}

#[test]
fn collect_py_files_from_fixture_dir() {
    let files = collect_py_files(&[fixture_path("tests/fixtures")]);
    assert!(
        files.len() >= 6,
        "expected at least 6 .py files, got {}",
        files.len()
    );
    for f in &files {
        assert!(f.extension().is_some_and(|e| e == "py"));
    }
}

#[test]
fn collect_py_files_single_file() {
    let files = collect_py_files(&[fixture_path("tests/fixtures/simple.py")]);
    assert_eq!(files.len(), 1);
}

#[test]
fn collect_py_files_nonexistent_dir() {
    let files = collect_py_files(&["tests/fixtures/nonexistent_dir".to_string()]);
    assert!(files.is_empty());
}

#[test]
fn compute_base_path_empty_paths() {
    let base = compute_base_path(&[]);
    assert_eq!(base, Path::new(""));
}

#[test]
fn collect_py_files_non_py_file() {
    let files = collect_py_files(&[fixture_path("Cargo.toml")]);
    assert!(files.is_empty());
}

#[test]
fn compute_base_path_divergent_prefixes() {
    let a = fixture_path("tests/fixtures/simple.py");
    let b = fixture_path("src/main.rs");
    let base = compute_base_path(&[a, b]);
    // Common prefix is the project root
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert_eq!(base, manifest);
}

#[test]
fn walk_dir_unreadable_directory() {
    use py_mapper::loader::walk_dir;
    let tmp = tempfile::tempdir().unwrap();
    let unreadable = tmp.path().join("noperm");
    std::fs::create_dir(&unreadable).unwrap();
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();

    let mut out = Vec::new();
    walk_dir(&unreadable, &mut out);
    assert!(out.is_empty());

    // Restore permissions for cleanup
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn load_modules_no_py_files_returns_err() {
    let result = load_modules(&["nonexistent_path".to_string()]);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("cannot access"));
}

#[test]
fn module_prefix_init_py() {
    let prefix =
        module_prefix_from_path(Path::new("/base/package/__init__.py"), Path::new("/base"));
    assert_eq!(prefix, "package");
}

#[test]
fn module_prefix_root_init_py() {
    let prefix = module_prefix_from_path(Path::new("/base/__init__.py"), Path::new("/base"));
    assert_eq!(prefix, "");
}

#[test]
fn module_prefix_non_normal_component() {
    let prefix = module_prefix_from_path(Path::new("../pkg/file.py"), Path::new(""));
    assert_eq!(prefix, "pkg.file");
}
