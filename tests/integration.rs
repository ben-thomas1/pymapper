use py_mapper::cycles::find_cycles;
use py_mapper::dead_code::{DeadCodeOptions, find_dead_code, find_dead_code_classified};
use py_mapper::graph::build_call_graph;
use py_mapper::loader::load_modules;
use py_mapper::metrics::compute_graph_metrics;
use py_mapper::report::{
    build_module_summary, build_report_rows, check_thresholds, parse_thresholds,
};

#[test]
fn full_pipeline_report() {
    let combined = load_modules(&["tests/fixtures".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);
    let gm = compute_graph_metrics(&cg, &combined.functions);
    let (rows, total) = build_report_rows(&combined, &gm, None, None);
    assert!(total > 0);
    assert_eq!(rows.len(), total);
    // Every row should have a non-empty name
    for r in &rows {
        assert!(!r.name.is_empty());
    }
}

#[test]
fn full_pipeline_dead_code() {
    let combined = load_modules(&["tests/fixtures".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);
    let gm = compute_graph_metrics(&cg, &combined.functions);
    let dead = find_dead_code(&combined.functions, &combined.classes, &gm);
    // There should be at least some uncalled functions in fixtures
    assert!(!dead.is_empty());
}

#[test]
fn full_pipeline_dead_code_classified() {
    let combined = load_modules(&["tests/fixtures".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);
    let gm = compute_graph_metrics(&cg, &combined.functions);
    let options = DeadCodeOptions {
        classify: true,
        ..Default::default()
    };
    let entries = find_dead_code_classified(&combined.functions, &combined.classes, &gm, &options);
    assert!(!entries.is_empty());
}

#[test]
fn full_pipeline_cycles() {
    let combined = load_modules(&["tests/fixtures/cycles.py".to_string()]).unwrap();
    let cg = build_call_graph(&combined, true);
    let found = find_cycles(&cg);
    assert!(
        !found.is_empty(),
        "cycles.py should produce at least one cycle"
    );
}

#[test]
fn full_pipeline_no_cycles_on_simple() {
    let combined = load_modules(&["tests/fixtures/simple.py".to_string()]).unwrap();
    let cg = build_call_graph(&combined, true);
    let found = find_cycles(&cg);
    assert!(found.is_empty());
}

#[test]
fn threshold_check_on_complex() {
    let combined = load_modules(&["tests/fixtures/complex.py".to_string()]).unwrap();
    let t = parse_thresholds("cc=2").unwrap();
    let violations = check_thresholds(&combined, &t);
    // complex_function should violate cc=2
    assert!(!violations.is_empty());
}

#[test]
fn report_with_min_cc_filter() {
    let combined = load_modules(&["tests/fixtures".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);
    let gm = compute_graph_metrics(&cg, &combined.functions);
    let (rows, total) = build_report_rows(&combined, &gm, Some(3), None);
    assert!(rows.len() <= total);
    for r in &rows {
        assert!(r.cc >= 3);
    }
}

#[test]
fn report_with_top_filter() {
    let combined = load_modules(&["tests/fixtures".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);
    let gm = compute_graph_metrics(&cg, &combined.functions);
    let (rows, _) = build_report_rows(&combined, &gm, None, Some(3));
    assert!(rows.len() <= 3);
}

#[test]
fn inheritance_self_call_resolution() {
    let combined = load_modules(&["tests/fixtures/inheritance.py".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);

    // Puppy.play calls self.speak and self.fetch
    // self.speak should resolve to Puppy.speak (or walk up to Dog/Animal)
    let play_qname = combined
        .functions
        .iter()
        .find(|f| f.name == "play")
        .map(|f| f.qualified_name())
        .unwrap();
    let play_idx = cg.node_map[&play_qname];

    // play should have outgoing edges
    let out_count = cg
        .graph
        .edges_directed(play_idx, petgraph::Direction::Outgoing)
        .count();
    assert!(
        out_count >= 2,
        "play() should call at least speak() and fetch()"
    );
}

// --- Cross-module import resolution tests ---

#[test]
fn cross_module_from_import_resolves() {
    let combined = load_modules(&["tests/fixtures/cross_module".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);
    // a::caller calls foo() which is from b import foo
    let caller_idx = cg.node_map["a::caller"];
    let foo_idx = cg.node_map["b::foo"];
    assert!(cg.graph.contains_edge(caller_idx, foo_idx));
    assert!(
        !cg.external_nodes.contains("foo"),
        "foo should not be external"
    );
}

#[test]
fn cross_module_attribute_call_resolves() {
    let combined = load_modules(&["tests/fixtures/cross_module".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);
    // a::caller calls utils.helper() which is import utils
    let caller_idx = cg.node_map["a::caller"];
    let helper_idx = cg.node_map["utils::helper"];
    assert!(cg.graph.contains_edge(caller_idx, helper_idx));
}

#[test]
fn cross_module_aliased_import_resolves() {
    let combined = load_modules(&["tests/fixtures/cross_module".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);
    // a::caller calls b_bar() which is from b import bar as b_bar
    let caller_idx = cg.node_map["a::caller"];
    let bar_idx = cg.node_map["b::bar"];
    assert!(cg.graph.contains_edge(caller_idx, bar_idx));
}

#[test]
fn cross_module_same_module_priority() {
    let combined = load_modules(&["tests/fixtures/cross_module".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);
    // a::same_name_test calls local_func() — should resolve to a::local_func, not anything else
    let test_idx = cg.node_map["a::same_name_test"];
    let local_idx = cg.node_map["a::local_func"];
    assert!(cg.graph.contains_edge(test_idx, local_idx));
}

#[test]
fn cross_module_unanalyzed_import_remains_external() {
    let combined = load_modules(&["tests/fixtures/cross_module".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);
    // a::caller calls join() from os.path — should be external
    assert!(
        cg.external_nodes.contains("join") || cg.external_nodes.iter().any(|n| n.contains("join")),
        "join from os.path should remain external"
    );
}

#[test]
fn cross_module_dead_code_excludes_imported_functions() {
    let combined = load_modules(&["tests/fixtures/cross_module".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);
    let gm = compute_graph_metrics(&cg, &combined.functions);
    let dead = find_dead_code(&combined.functions, &combined.classes, &gm);
    let dead_names: Vec<_> = dead.iter().map(|f| f.qualified_name()).collect();
    // b::foo is called from a, so it should NOT be dead
    assert!(
        !dead_names.contains(&"b::foo".to_string()),
        "b::foo should not be dead"
    );
    // b::bar is called from a (as b_bar), so it should NOT be dead
    assert!(
        !dead_names.contains(&"b::bar".to_string()),
        "b::bar should not be dead"
    );
    // b::unused is not called anywhere, so it SHOULD be dead
    assert!(
        dead_names.contains(&"b::unused".to_string()),
        "b::unused should be dead"
    );
}

#[test]
fn cross_module_coupling_metrics_nonzero() {
    let combined = load_modules(&["tests/fixtures/cross_module".to_string()]).unwrap();
    let cg = build_call_graph(&combined, false);
    let gm = compute_graph_metrics(&cg, &combined.functions);
    let summary = build_module_summary(&combined, &gm, None);
    let mod_a = summary.iter().find(|r| r.module == "a").unwrap();
    // Module a imports from b and utils, so Ce >= 2
    assert!(mod_a.ce >= 2, "a should have Ce >= 2, got {}", mod_a.ce);
    let mod_b = summary.iter().find(|r| r.module == "b").unwrap();
    // Module b is imported by a, so Ca >= 1
    assert!(mod_b.ca >= 1, "b should have Ca >= 1, got {}", mod_b.ca);
}
