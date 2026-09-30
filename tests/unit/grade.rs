use std::collections::HashMap;

use py_mapper::grade::print_grade;
use py_mapper::graph::build_call_graph;
use py_mapper::metrics::compute_graph_metrics;
use py_mapper::parser::parse_python;
use py_mapper::types::{ModuleInfo, OutputFormat};

fn parsed_module(src: &str) -> ModuleInfo {
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
fn grade_empty_module() {
    let module = ModuleInfo::default();
    let gm = HashMap::new();
    print_grade(&module, &gm, OutputFormat::Table, 5);
}

#[test]
fn grade_empty_module_json() {
    let module = ModuleInfo::default();
    let gm = HashMap::new();
    print_grade(&module, &gm, OutputFormat::Json, 5);
}

#[test]
fn grade_simple_module() {
    let module = parsed_module("def a():\n    pass\n\ndef b():\n    a()\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    print_grade(&module, &gm, OutputFormat::Table, 5);
}

#[test]
fn grade_json_format() {
    let module = parsed_module("def a():\n    pass\n\ndef b():\n    a()\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    print_grade(&module, &gm, OutputFormat::Json, 5);
}

#[test]
fn grade_d_grade_dimension() {
    // Create functions with high complexity to drive scores into D range (60-69)
    let src = "\
def f1(a, b, c, d, e, f, g, h):
    if a:
        if b:
            if c:
                if d:
                    if e:
                        if f:
                            if g:
                                if h:
                                    return 1
    return 0

def f2(a, b, c, d, e, f, g):
    if a:
        if b:
            if c:
                if d:
                    if e:
                        if f:
                            if g:
                                return 1
    return 0

def f3(a, b, c, d, e, f):
    if a:
        if b:
            if c:
                if d:
                    if e:
                        if f:
                            return 1
    return 0

def ok1():
    pass

def ok2():
    pass

def ok3():
    pass

def ok4():
    pass
";
    let module = parsed_module(src);
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    print_grade(&module, &gm, OutputFormat::Table, 3);
}
