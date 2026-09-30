//! Regression checks using only generated, synthetic Python inputs.
use assert_cmd::cargo::cargo_bin_cmd;
use py_mapper::v2::{Confidence, EdgeKind, Profile, analyze_paths};
use std::fs;

fn fixture(source: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mod.py");
    fs::write(&path, source).unwrap();
    (dir, path.to_str().unwrap().to_owned())
}

#[test]
fn both_clis_fail_for_partial_or_malformed_inputs() {
    let (_dir, good) = fixture("def good():\n    pass\n");
    let (_bad_dir, bad) = fixture("def broken(:\n    pass\n");
    for bad in [bad, format!("{good}.missing")] {
        cargo_bin_cmd!("pymapper")
            .args(["map", &good, &bad])
            .assert()
            .failure();
        cargo_bin_cmd!("pymapper-v1")
            .args(["report", &good, &bad])
            .assert()
            .failure();
    }
}

#[test]
fn overlapping_inputs_are_deduplicated() {
    let (dir, file) = fixture("def f():\n    pass\n");
    let bundle = analyze_paths(
        &[file.clone(), dir.path().display().to_string(), file],
        Profile::General,
    )
    .unwrap();
    assert_eq!(bundle.project.file_count, 1);
    assert_eq!(bundle.symbols.len(), 1);
}

#[test]
fn relative_imports_work_in_modules_and_package_initializers() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("pkg");
    fs::create_dir(&pkg).unwrap();
    fs::write(
        pkg.join("__init__.py"),
        "from .helper import work\ndef init():\n    work()\n",
    )
    .unwrap();
    fs::write(
        pkg.join("client.py"),
        "from .helper import work\ndef run():\n    work()\n",
    )
    .unwrap();
    fs::write(pkg.join("helper.py"), "def work():\n    pass\n").unwrap();
    let bundle = analyze_paths(&[dir.path().display().to_string()], Profile::General).unwrap();
    let target = bundle
        .symbols
        .iter()
        .find(|s| s.qualified_name == "pkg.helper::work")
        .unwrap();
    for name in ["init", "run"] {
        let caller = bundle.symbols.iter().find(|s| s.name == name).unwrap();
        assert!(
            bundle
                .edges
                .iter()
                .any(|e| e.from == caller.id && e.to.as_ref() == Some(&target.id))
        );
    }
}

#[test]
fn nested_functions_resolve_in_their_own_scope() {
    let (_dir, file) = fixture(
        "def first():\n    def helper():\n        pass\n    helper()\ndef second():\n    def helper():\n        pass\n    helper()\ndef outside():\n    helper()\n",
    );
    let bundle = analyze_paths(&[file], Profile::General).unwrap();
    for name in ["first", "second"] {
        let caller = bundle.symbols.iter().find(|s| s.name == name).unwrap();
        let target = bundle
            .symbols
            .iter()
            .find(|s| s.parent_symbol.as_ref() == Some(&caller.id))
            .unwrap();
        assert!(
            bundle
                .edges
                .iter()
                .any(|e| e.from == caller.id && e.to.as_ref() == Some(&target.id))
        );
    }
    let outside = bundle.symbols.iter().find(|s| s.name == "outside").unwrap();
    assert!(
        bundle
            .edges
            .iter()
            .any(|e| e.from == outside.id && e.to.is_none())
    );
}

#[test]
fn pytest_helpers_are_not_all_entrypoints() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("test_sample.py");
    fs::write(
        &file,
        "def helper():\n    pass\ndef test_case():\n    pass\n",
    )
    .unwrap();
    let bundle = analyze_paths(&[file.display().to_string()], Profile::Pytest).unwrap();
    let helper = bundle.symbols.iter().find(|s| s.name == "helper").unwrap();
    assert!(
        !bundle
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::PytestEntrypoint && e.to.as_ref() == Some(&helper.id))
    );
}

#[test]
fn v2_maintainability_uses_measured_halstead_volume() {
    let source = "def compute(x):\n    return (x + 2) * (x - 3)\n";
    let (_dir, file) = fixture(source);
    let legacy = py_mapper::parser::parse_python(source);
    let bundle = analyze_paths(&[file], Profile::General).unwrap();
    assert!(
        (bundle.symbols[0].maintainability_index - legacy.functions[0].maintainability_index).abs()
            < 1e-9
    );
}

#[test]
fn json_respects_confidence_and_unknown_explain_fails() {
    let (_dir, file) = fixture("def caller():\n    unknown()\n");
    for command in ["map", "scan"] {
        let output = cargo_bin_cmd!("pymapper")
            .args([
                command,
                &file,
                "--format",
                "json",
                "--confidence",
                "certain",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert!(
            value["edges"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["confidence"] == "certain")
        );
        assert!(
            value["findings"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["confidence"] == "certain")
        );
    }
    cargo_bin_cmd!("pymapper")
        .args(["explain", "missing", &file, "--format", "json"])
        .assert()
        .failure();
}

#[test]
fn baseline_save_never_overwrites_existing_data() {
    let (_dir, file) = fixture("def original():\n    pass\n");
    let before = fs::read(&file).unwrap();
    cargo_bin_cmd!("pymapper-v1")
        .args(["report", &file, "--save-baseline", &file])
        .assert()
        .failure();
    assert_eq!(fs::read(file).unwrap(), before);
}

#[test]
fn invalid_thresholds_and_histogram_threshold_violations_fail() {
    let (_dir, file) = fixture("def f(x):\n    if x:\n        return 1\n    return 0\n");
    for threshold in ["unknown=2", "cc=oops", "cc", "", "cc=1,cc=10"] {
        cargo_bin_cmd!("pymapper-v1")
            .args(["report", &file, "--fail-above", threshold])
            .assert()
            .failure();
    }
    cargo_bin_cmd!("pymapper-v1")
        .args(["report", &file, "--histogram", "--fail-above", "cc=1"])
        .assert()
        .failure();
}

#[test]
fn unrelated_module_class_is_not_a_certain_constructor() {
    let (dir, file) = fixture("def f():\n    Unimported()\n");
    fs::write(dir.path().join("other.py"), "class Unimported:\n    pass\n").unwrap();
    let bundle =
        analyze_paths(&[dir.path().display().to_string(), file], Profile::General).unwrap();
    let edge = bundle
        .edges
        .iter()
        .find(|e| e.kind == EdgeKind::Call)
        .unwrap();
    assert_eq!(edge.confidence, Confidence::Unresolved);
}

#[test]
fn custom_sort_happens_before_top_limit() {
    let (_dir, file) = fixture(
        "def a(x):\n    if x:\n        return 1\n    return 0\ndef z(a, b, c, d):\n    pass\n",
    );
    let output = cargo_bin_cmd!("pymapper-v1")
        .args([
            "report", &file, "--sort", "params", "--top", "1", "--format", "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert!(value[0]["Function"].as_str().unwrap().ends_with("::z"));
}

#[test]
fn local_imports_do_not_leak_and_parameters_shadow_globals() {
    let (dir, _) = fixture(
        "def imported():\n    from other import helper\n    helper()\ndef outside():\n    helper()\ndef target():\n    pass\ndef shadowed(target):\n    target()\n",
    );
    fs::write(dir.path().join("other.py"), "def helper():\n    pass\n").unwrap();
    let bundle = analyze_paths(&[dir.path().display().to_string()], Profile::General).unwrap();
    for (name, resolved) in [("imported", true), ("outside", false), ("shadowed", false)] {
        let caller = bundle.symbols.iter().find(|s| s.name == name).unwrap();
        let edge = bundle
            .edges
            .iter()
            .find(|e| e.from == caller.id && e.kind == EdgeKind::Call)
            .unwrap();
        assert_eq!(edge.to.is_some(), resolved, "{name}");
    }
}

#[test]
fn legacy_cyclic_inheritance_terminates() {
    let (_dir, file) =
        fixture("class A(B):\n    def run(self):\n        self.missing()\nclass B(A):\n    pass\n");
    cargo_bin_cmd!("pymapper-v1")
        .timeout(std::time::Duration::from_secs(5))
        .args(["graph", &file])
        .assert()
        .success();
}

#[test]
fn excessive_syntax_nesting_is_reported() {
    let source = format!(
        "def f():\n    return {}1{}\n",
        "(".repeat(300),
        ")".repeat(300)
    );
    let (_dir, file) = fixture(&source);
    assert!(analyze_paths(&[file.clone()], Profile::General).is_err());
    cargo_bin_cmd!("pymapper-v1")
        .args(["report", &file])
        .assert()
        .failure();
}

#[cfg(unix)]
#[test]
fn directory_walk_does_not_follow_symlinks_or_virtual_environments() {
    let (dir, _) = fixture("def f():\n    pass\n");
    let (outside, _) = fixture("def private_example():\n    pass\n");
    std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("mod.py"), dir.path().join("linked.py"))
        .unwrap();
    fs::create_dir(dir.path().join(".venv")).unwrap();
    fs::write(
        dir.path().join(".venv/dependency.py"),
        "def dependency():\n    pass\n",
    )
    .unwrap();
    let bundle = analyze_paths(&[dir.path().display().to_string()], Profile::General).unwrap();
    assert_eq!(bundle.project.file_count, 1);
}

#[cfg(unix)]
#[test]
fn baseline_refuses_symlink_destinations() {
    let (dir, file) = fixture("def f():\n    pass\n");
    let link = dir.path().join("baseline.json");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    let before = fs::read(&file).unwrap();
    assert!(py_mapper::report::save_baseline(&[], link.to_str().unwrap()).is_err());
    assert_eq!(fs::read(file).unwrap(), before);
}

#[test]
fn empty_grade_is_valid_json_and_invalid_config_is_an_error() {
    let (dir, file) = fixture("# An empty module\n");
    let output = cargo_bin_cmd!("pymapper-v1")
        .current_dir(dir.path())
        .args(["grade", &file, "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert!(value["score"].is_null());
    fs::write(dir.path().join(".pymapper.toml"), "[invalid").unwrap();
    cargo_bin_cmd!("pymapper-v1")
        .current_dir(dir.path())
        .args(["report", &file])
        .assert()
        .failure();
}

#[test]
fn graph_depth_is_independent_of_call_order() {
    for calls in ["    b()\n    c()\n", "    c()\n    b()\n"] {
        let source = format!("def a():\n{calls}def b():\n    c()\ndef c():\n    pass\n");
        let (_dir, file) = fixture(&source);
        let bundle = analyze_paths(&[file], Profile::General).unwrap();
        let a = bundle.symbols.iter().find(|s| s.name == "a").unwrap();
        assert_eq!(
            bundle
                .metrics
                .iter()
                .find(|m| m.symbol == a.id)
                .unwrap()
                .call_depth,
            1
        );
        let module = py_mapper::parser::parse_python(&source);
        let graph = py_mapper::graph::build_call_graph(&module, false);
        let metrics = py_mapper::metrics::compute_graph_metrics(&graph, &module.functions);
        assert_eq!(metrics["a"].call_depth, 1);
        let summary = py_mapper::report::build_module_summary(&module, &metrics, None);
        assert_eq!(summary[0].cross_module, 0);
    }
}

#[test]
fn complexity_counts_conditions_and_halstead_tokens_once() {
    let module = py_mapper::parser::parse_python(
        "def f(a, b):\n    if a and b:\n        return 1\n    elif a:\n        return 2\n    else:\n        return 3\n",
    );
    assert_eq!(module.functions[0].cognitive_complexity, 4);
    let module = py_mapper::parser::parse_python("def f():\n    return 1\n");
    assert_eq!(module.functions[0].halstead_volume, 2.0);
}
