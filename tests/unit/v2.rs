use std::fs;

use py_mapper::v2::{Confidence, EdgeKind, Profile, SymbolKind, analyze_paths};

#[test]
fn resolves_from_import_call() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("a.py"),
        "from b import foo\n\ndef caller():\n    foo()\n",
    )
    .unwrap();
    fs::write(dir.path().join("b.py"), "def foo():\n    return 1\n").unwrap();

    let bundle = analyze_paths(
        &[dir.path().to_str().unwrap().to_string()],
        Profile::General,
    )
    .unwrap();
    let caller = bundle
        .symbols
        .iter()
        .find(|s| s.qualified_name == "a::caller")
        .unwrap();
    let foo = bundle
        .symbols
        .iter()
        .find(|s| s.qualified_name == "b::foo")
        .unwrap();

    assert!(bundle.edges.iter().any(|e| {
        e.from == caller.id
            && e.to.as_deref() == Some(foo.id.as_str())
            && e.kind == EdgeKind::Call
            && e.confidence == Confidence::Certain
    }));
}

#[test]
fn duplicate_function_names_have_distinct_ids() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("mod.py"),
        "def f():\n    return 1\n\ndef f():\n    return 2\n",
    )
    .unwrap();

    let bundle = analyze_paths(
        &[dir.path().to_str().unwrap().to_string()],
        Profile::General,
    )
    .unwrap();
    let funcs: Vec<_> = bundle.symbols.iter().filter(|s| s.name == "f").collect();

    assert_eq!(funcs.len(), 2);
    assert_ne!(funcs[0].id, funcs[1].id);
}

#[test]
fn nested_functions_are_symbols() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("mod.py"),
        "def outer():\n    def inner():\n        return 1\n    return inner()\n",
    )
    .unwrap();

    let bundle = analyze_paths(
        &[dir.path().to_str().unwrap().to_string()],
        Profile::General,
    )
    .unwrap();

    assert!(
        bundle
            .symbols
            .iter()
            .any(|s| s.kind == SymbolKind::NestedFunction && s.name == "inner")
    );
}

#[test]
fn self_method_resolves_through_base_classes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("mod.py"),
        "class Base:\n    def save(self):\n        pass\n\nclass Child(Base):\n    def run(self):\n        self.save()\n",
    )
    .unwrap();

    let bundle = analyze_paths(
        &[dir.path().to_str().unwrap().to_string()],
        Profile::General,
    )
    .unwrap();
    let run = bundle
        .symbols
        .iter()
        .find(|s| s.qualified_name == "mod::Child.run")
        .unwrap();
    let save = bundle
        .symbols
        .iter()
        .find(|s| s.qualified_name == "mod::Base.save")
        .unwrap();

    assert!(bundle.edges.iter().any(|e| {
        e.from == run.id
            && e.to.as_deref() == Some(save.id.as_str())
            && e.confidence == Confidence::Certain
    }));
}
