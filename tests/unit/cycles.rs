use py_mapper::cycles::{find_cycles, print_cycles};
use py_mapper::graph::build_call_graph;
use py_mapper::parser::parse_python;
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
fn find_cycles_mutual_recursion() {
    let module = parsed_module(
        "\
def ping():
    pong()

def pong():
    ping()
",
    );
    let cg = build_call_graph(&module, true);
    let cycles = find_cycles(&cg);
    assert_eq!(cycles.len(), 1);
    let cycle = &cycles[0];
    assert_eq!(cycle.len(), 2);
    let names: Vec<&str> = cycle.iter().map(|s| s.as_str()).collect();
    assert!(names.contains(&"test::ping"));
    assert!(names.contains(&"test::pong"));
}

#[test]
fn find_cycles_acyclic() {
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
    let cg = build_call_graph(&module, true);
    let cycles = find_cycles(&cg);
    assert!(cycles.is_empty());
}

#[test]
fn find_cycles_self_recursion_not_detected() {
    // Tarjan SCC with len > 1 won't include self-loops as cycles
    let module = parsed_module(
        "\
def recurse():
    recurse()
",
    );
    let cg = build_call_graph(&module, true);
    let cycles = find_cycles(&cg);
    // Single-node SCCs are filtered out by find_cycles
    assert!(cycles.is_empty());
}

#[test]
fn find_cycles_three_way() {
    let module = parsed_module(
        "\
def a():
    b()

def b():
    c()

def c():
    a()
",
    );
    let cg = build_call_graph(&module, true);
    let cycles = find_cycles(&cg);
    assert_eq!(cycles.len(), 1);
    assert_eq!(cycles[0].len(), 3);
}

#[test]
fn print_cycles_table_format() {
    let module = parsed_module(
        "\
def ping():
    pong()

def pong():
    ping()
",
    );
    let cg = build_call_graph(&module, true);
    let cycles = find_cycles(&cg);
    // Should not panic
    print_cycles(&cycles, OutputFormat::Table);
}

#[test]
fn print_cycles_json_format() {
    let module = parsed_module(
        "\
def ping():
    pong()

def pong():
    ping()
",
    );
    let cg = build_call_graph(&module, true);
    let cycles = find_cycles(&cg);
    print_cycles(&cycles, OutputFormat::Json);
}

#[test]
fn print_cycles_csv_format() {
    let module = parsed_module(
        "\
def ping():
    pong()

def pong():
    ping()
",
    );
    let cg = build_call_graph(&module, true);
    let cycles = find_cycles(&cg);
    print_cycles(&cycles, OutputFormat::Csv);
}

#[test]
fn print_cycles_markdown_format() {
    let module = parsed_module(
        "\
def ping():
    pong()

def pong():
    ping()
",
    );
    let cg = build_call_graph(&module, true);
    let cycles = find_cycles(&cg);
    print_cycles(&cycles, OutputFormat::Markdown);
}

#[test]
fn print_cycles_empty() {
    print_cycles(&[], OutputFormat::Table);
    print_cycles(&[], OutputFormat::Markdown);
}
