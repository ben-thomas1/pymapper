use py_mapper::graph::build_call_graph;
use py_mapper::metrics::compute_graph_metrics;
use py_mapper::parser::parse_python;

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
fn fan_in_fan_out() {
    let module = parsed_module(
        "\
def a():
    b()
    c()

def b():
    c()

def c():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);

    let a = &gm["test::a"];
    assert_eq!(a.fan_in, 0);
    assert_eq!(a.fan_out, 2);

    let b = &gm["test::b"];
    assert_eq!(b.fan_in, 1);
    assert_eq!(b.fan_out, 1);

    let c = &gm["test::c"];
    assert_eq!(c.fan_in, 2);
    assert_eq!(c.fan_out, 0);
}

#[test]
fn call_depth() {
    let module = parsed_module(
        "\
def a():
    b()

def b():
    c()

def c():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);

    assert_eq!(gm["test::a"].call_depth, 2);
    assert_eq!(gm["test::b"].call_depth, 1);
    assert_eq!(gm["test::c"].call_depth, 0);
}

#[test]
fn rfc_counts_reachable() {
    let module = parsed_module(
        "\
def a():
    b()

def b():
    c()

def c():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);

    // RFC = count of reachable nodes including self
    assert_eq!(gm["test::a"].rfc, 3); // a, b, c
    assert_eq!(gm["test::b"].rfc, 2); // b, c
    assert_eq!(gm["test::c"].rfc, 1); // c
}

#[test]
fn transitive_loc_and_cc() {
    let module = parsed_module(
        "\
def a():
    b()

def b():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);

    let a = &gm["test::a"];
    // t_loc should include both a and b's LOC
    assert!(a.t_loc >= 2);
    // t_cc should include both a and b's CC
    assert!(a.t_cc >= 2);
}

#[test]
fn fan_out_counts_unique_callees() {
    let module = parsed_module(
        "\
def chatty():
    log()
    log()
    log()
    process()

def log():
    pass

def process():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let chatty = &gm["test::chatty"];
    assert_eq!(
        chatty.fan_out, 2,
        "fan-out should count unique callees (log, process), not call sites"
    );
}
