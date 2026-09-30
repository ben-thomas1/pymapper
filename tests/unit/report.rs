use py_mapper::graph::build_call_graph;
use py_mapper::metrics::compute_graph_metrics;
use py_mapper::parser::parse_python;
use py_mapper::report::{
    build_report_rows, check_thresholds, compare_baseline, load_baseline, parse_thresholds,
    print_column_descriptions, print_module_summary, print_report, print_sorted_report,
    print_violations, save_baseline, sort_rows_by_column,
};
use py_mapper::types::OutputFormat;

fn parsed_module(src: &str) -> py_mapper::types::ModuleInfo {
    let mut module = parse_python(src);
    for f in &mut module.functions {
        f.source_module = "test".to_string();
    }
    for c in &mut module.classes {
        c.source_module = "test".to_string();
    }
    module
}

#[test]
fn parse_thresholds_cc_only() {
    let t = parse_thresholds("cc=10").unwrap();
    assert_eq!(t.cc, Some(10));
    assert_eq!(t.cognitive, None);
}

#[test]
fn parse_thresholds_multiple() {
    let t = parse_thresholds("cc=10,cognitive=15,nesting=5").unwrap();
    assert_eq!(t.cc, Some(10));
    assert_eq!(t.cognitive, Some(15));
    assert_eq!(t.nesting, Some(5));
    assert_eq!(t.loc, None);
    assert_eq!(t.params, None);
}

#[test]
fn parse_thresholds_with_spaces() {
    let t = parse_thresholds(" cc = 5 , loc = 100 ").unwrap();
    assert_eq!(t.cc, Some(5));
    assert_eq!(t.loc, Some(100));
}

#[test]
fn build_report_rows_unfiltered() {
    let module = parsed_module(
        "\
def a():
    pass

def b():
    if True:
        pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let (rows, total) = build_report_rows(&module, &gm, None, None);
    assert_eq!(total, 2);
    assert_eq!(rows.len(), 2);
}

#[test]
fn build_report_rows_min_cc_filter() {
    let module = parsed_module(
        "\
def simple():
    pass

def complex_one(x):
    if x > 0:
        if x > 10:
            pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let (rows, total) = build_report_rows(&module, &gm, Some(2), None);
    assert_eq!(total, 2);
    // Only complex_one should pass (cc >= 2)
    assert!(rows.len() <= total);
    for r in &rows {
        assert!(r.cc >= 2);
    }
}

#[test]
fn build_report_rows_top_filter() {
    let module = parsed_module(
        "\
def a():
    pass

def b():
    pass

def c():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let (rows, total) = build_report_rows(&module, &gm, None, Some(2));
    assert_eq!(total, 3);
    assert_eq!(rows.len(), 2);
}

#[test]
fn sort_rows_by_column_valid() {
    let module = parsed_module(
        "\
def a():
    pass

def b():
    if True:
        pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let (mut rows, _) = build_report_rows(&module, &gm, None, None);

    assert!(sort_rows_by_column(&mut rows, "name", true));
    assert!(rows[0].name <= rows[1].name);

    assert!(sort_rows_by_column(&mut rows, "cc", false));
    assert!(rows[0].cc >= rows[1].cc);
}

#[test]
fn sort_rows_by_column_unknown_returns_false() {
    let module = parsed_module("def a():\n    pass\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let (mut rows, _) = build_report_rows(&module, &gm, None, None);
    assert!(!sort_rows_by_column(&mut rows, "nonexistent", true));
}

#[test]
fn check_thresholds_no_violations() {
    let module = parsed_module("def simple():\n    return 1\n");
    let t = parse_thresholds("cc=10").unwrap();
    let violations = check_thresholds(&module, &t);
    assert!(violations.is_empty());
}

#[test]
fn check_thresholds_with_violations() {
    let src = "\
def complex_func(a, b, c, d, e):
    if a:
        if b:
            if c:
                if d:
                    if e:
                        pass
";
    let module = parsed_module(src);
    let t = parse_thresholds("cc=2").unwrap();
    let violations = check_thresholds(&module, &t);
    assert!(!violations.is_empty());
}

#[test]
fn save_and_load_baseline_roundtrip() {
    let module = parsed_module(
        "\
def foo():
    return 1

def bar(x):
    if x:
        return 2
    return 3
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let (rows, _) = build_report_rows(&module, &gm, None, None);

    let tmp = tempfile::tempdir().unwrap();
    let destination = tmp.path().join("baseline.json");
    let path = destination.to_str().unwrap();

    save_baseline(&rows, path).unwrap();
    let loaded = load_baseline(path).unwrap();

    assert_eq!(loaded.len(), rows.len());
    for (orig, loaded_row) in rows.iter().zip(loaded.iter()) {
        assert_eq!(orig.name, loaded_row.name);
        assert_eq!(orig.cc, loaded_row.cc);
        assert_eq!(orig.cognitive, loaded_row.cognitive);
    }
}

#[test]
fn compare_baseline_no_regression() {
    let module = parsed_module("def foo():\n    return 1\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let (rows, _) = build_report_rows(&module, &gm, None, None);

    // Compare against itself — no regressions
    let failed = compare_baseline(&rows, &rows, true);
    assert!(!failed);
}

#[test]
fn print_report_all_formats() {
    let module = parsed_module("def a():\n    pass\n\ndef b():\n    if True:\n        pass\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    for fmt in [
        OutputFormat::Table,
        OutputFormat::Json,
        OutputFormat::Csv,
        OutputFormat::Markdown,
    ] {
        print_report(&module, &gm, None, None, fmt);
    }
}

#[test]
fn print_sorted_report_exercises_path() {
    let module = parsed_module("def a():\n    pass\n\ndef b():\n    if True:\n        pass\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let (mut rows, total) = build_report_rows(&module, &gm, None, None);
    sort_rows_by_column(&mut rows, "cognitive", false);
    for fmt in [
        OutputFormat::Table,
        OutputFormat::Json,
        OutputFormat::Csv,
        OutputFormat::Markdown,
    ] {
        print_sorted_report(&rows, total, false, fmt);
        print_sorted_report(&rows, total, true, fmt);
    }
}

#[test]
fn print_violations_with_data() {
    let src = "\
def complex_func(a, b, c, d, e):
    if a:
        if b:
            if c:
                if d:
                    if e:
                        pass
";
    let module = parsed_module(src);
    let t = parse_thresholds("cc=2").unwrap();
    let violations = check_thresholds(&module, &t);
    assert!(print_violations(&violations));
}

#[test]
fn print_violations_empty() {
    assert!(!print_violations(&[]));
}

#[test]
fn print_module_summary_all_formats() {
    let module = parsed_module("def a():\n    pass\n\ndef b():\n    a()\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    for fmt in [
        OutputFormat::Table,
        OutputFormat::Json,
        OutputFormat::Csv,
        OutputFormat::Markdown,
    ] {
        print_module_summary(&module, &gm, None, fmt);
    }
}

#[test]
fn print_column_descriptions_runs() {
    print_column_descriptions();
}

#[test]
fn compare_baseline_with_regression() {
    let base_module = parsed_module("def foo():\n    return 1\n");
    let base_cg = build_call_graph(&base_module, false);
    let base_gm = compute_graph_metrics(&base_cg, &base_module.functions);
    let (base_rows, _) = build_report_rows(&base_module, &base_gm, None, None);

    let cur_module = parsed_module(
        "\
def foo():
    if True:
        if True:
            return 1
    return 2
",
    );
    let cur_cg = build_call_graph(&cur_module, false);
    let cur_gm = compute_graph_metrics(&cur_cg, &cur_module.functions);
    let (cur_rows, _) = build_report_rows(&cur_module, &cur_gm, None, None);

    let failed = compare_baseline(&cur_rows, &base_rows, true);
    assert!(failed);
}

#[test]
fn compare_baseline_deleted_function() {
    let base_module = parsed_module("def foo():\n    pass\n\ndef bar():\n    pass\n");
    let base_cg = build_call_graph(&base_module, false);
    let base_gm = compute_graph_metrics(&base_cg, &base_module.functions);
    let (base_rows, _) = build_report_rows(&base_module, &base_gm, None, None);

    let cur_module = parsed_module("def foo():\n    pass\n");
    let cur_cg = build_call_graph(&cur_module, false);
    let cur_gm = compute_graph_metrics(&cur_cg, &cur_module.functions);
    let (cur_rows, _) = build_report_rows(&cur_module, &cur_gm, None, None);

    // Deleted function should show up but not cause regression
    let failed = compare_baseline(&cur_rows, &base_rows, true);
    assert!(!failed);
}

#[test]
fn sort_rows_all_columns() {
    let module = parsed_module("def a():\n    pass\n\ndef b():\n    if True:\n        pass\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let (mut rows, _) = build_report_rows(&module, &gm, None, None);

    let columns = [
        "cognitive",
        "nesting",
        "loc",
        "params",
        "fi",
        "fo",
        "rfc",
        "depth",
        "t-loc",
        "tloc",
        "t-cc",
        "tcc",
        "t-mcc",
        "tmcc",
        "mi",
        "returns",
        "ret",
        "bool-expr",
        "bool",
    ];
    for col in columns {
        assert!(
            sort_rows_by_column(&mut rows, col, true),
            "sort by '{}' failed",
            col
        );
    }
}

#[test]
fn parse_thresholds_all_keys() {
    let t = parse_thresholds("cc=1,cognitive=2,nesting=3,loc=4,params=5").unwrap();
    assert_eq!(t.cc, Some(1));
    assert_eq!(t.cognitive, Some(2));
    assert_eq!(t.nesting, Some(3));
    assert_eq!(t.loc, Some(4));
    assert_eq!(t.params, Some(5));
}

#[test]
fn parse_thresholds_invalid_value() {
    assert!(parse_thresholds("cc=abc").is_err());
}

#[test]
fn parse_thresholds_unknown_key() {
    assert!(parse_thresholds("unknown=5").is_err());
}

#[test]
fn print_report_empty_module_table() {
    let module = py_mapper::types::ModuleInfo::default();
    let gm = std::collections::HashMap::new();
    print_report(&module, &gm, None, None, OutputFormat::Table);
}

#[test]
fn print_report_empty_module_markdown() {
    let module = py_mapper::types::ModuleInfo::default();
    let gm = std::collections::HashMap::new();
    print_report(&module, &gm, None, None, OutputFormat::Markdown);
}

#[test]
fn print_module_summary_empty_table() {
    let module = py_mapper::types::ModuleInfo::default();
    let gm = std::collections::HashMap::new();
    print_module_summary(&module, &gm, None, OutputFormat::Table);
}

#[test]
fn print_module_summary_empty_markdown() {
    let module = py_mapper::types::ModuleInfo::default();
    let gm = std::collections::HashMap::new();
    print_module_summary(&module, &gm, None, OutputFormat::Markdown);
}

#[test]
fn compare_baseline_regression_no_fail() {
    let base_module = parsed_module("def foo():\n    return 1\n");
    let base_cg = build_call_graph(&base_module, false);
    let base_gm = compute_graph_metrics(&base_cg, &base_module.functions);
    let (base_rows, _) = build_report_rows(&base_module, &base_gm, None, None);

    let cur_module = parsed_module(
        "\
def foo():
    if True:
        if True:
            return 1
    return 2
",
    );
    let cur_cg = build_call_graph(&cur_module, false);
    let cur_gm = compute_graph_metrics(&cur_cg, &cur_module.functions);
    let (cur_rows, _) = build_report_rows(&cur_module, &cur_gm, None, None);

    // Regressions exist but fail_regression is false
    let failed = compare_baseline(&cur_rows, &base_rows, false);
    assert!(!failed);
}

#[test]
fn compare_baseline_new_function() {
    let base_module = parsed_module("def foo():\n    return 1\n");
    let base_cg = build_call_graph(&base_module, false);
    let base_gm = compute_graph_metrics(&base_cg, &base_module.functions);
    let (base_rows, _) = build_report_rows(&base_module, &base_gm, None, None);

    // Current has an additional function
    let cur_module = parsed_module("def foo():\n    return 1\n\ndef new_func():\n    pass\n");
    let cur_cg = build_call_graph(&cur_module, false);
    let cur_gm = compute_graph_metrics(&cur_cg, &cur_module.functions);
    let (cur_rows, _) = build_report_rows(&cur_module, &cur_gm, None, None);

    let failed = compare_baseline(&cur_rows, &base_rows, false);
    assert!(!failed);
}

#[test]
fn save_baseline_error_path() {
    let module = parsed_module("def foo():\n    return 1\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let (rows, _) = build_report_rows(&module, &gm, None, None);

    let result = save_baseline(&rows, "/nonexistent/dir/baseline.json");
    assert!(result.is_err());
}

#[test]
fn load_baseline_error_path() {
    let result = load_baseline("/nonexistent/file.json");
    assert!(result.is_err());
}

#[test]
fn load_baseline_invalid_json() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), "not valid json").unwrap();
    let result = load_baseline(tmp.path().to_str().unwrap());
    assert!(result.is_err());
}

#[test]
fn module_summary_multi_module_with_coupling() {
    // Create functions in different source modules to exercise cross-module coupling.
    // Use call targets that include the module prefix so resolve actually finds them.
    use py_mapper::types::{CallTarget, FunctionInfo, ModuleInfo};

    fn make_func(name: &str, module: &str, calls: Vec<CallTarget>) -> FunctionInfo {
        FunctionInfo {
            name: name.to_string(),
            start_line: 1,
            end_line: 2,
            complexity: 1,
            cognitive_complexity: 0,
            max_nesting: 0,
            line_count: 2,
            param_count: 0,
            parent_class: None,
            return_count: 0,
            bool_expr_count: 0,
            halstead_volume: 0.0,
            maintainability_index: 100.0,
            calls,
            decorators: Vec::new(),
            source_module: module.to_string(),
            source_file: String::new(),
        }
    }

    let mut combined = ModuleInfo::default();
    combined
        .functions
        .push(make_func("helper", "mod_a", vec![]));
    combined.functions.push(make_func(
        "caller",
        "mod_b",
        vec![
            CallTarget {
                name: "mod_a::helper".to_string(),
                is_self: false,
            },
            CallTarget {
                name: "root_fn".to_string(),
                is_self: false,
            },
        ],
    ));
    // Root module function that calls mod_a::helper
    combined.functions.push(make_func(
        "root_fn",
        "",
        vec![CallTarget {
            name: "mod_a::helper".to_string(),
            is_self: false,
        }],
    ));

    let cg = build_call_graph(&combined, false);
    let gm = compute_graph_metrics(&cg, &combined.functions);

    for fmt in [
        OutputFormat::Table,
        OutputFormat::Json,
        OutputFormat::Csv,
        OutputFormat::Markdown,
    ] {
        print_module_summary(&combined, &gm, None, fmt);
    }
}
