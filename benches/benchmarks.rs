use std::fs;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use py_mapper::complexity::{
    compute_cognitive_complexity, compute_complexity, compute_halstead, compute_max_nesting,
};
use py_mapper::cycles::find_cycles;
use py_mapper::dead_code::find_dead_code;
use py_mapper::graph::{build_call_graph, emit_dot};
use py_mapper::loader::load_modules;
use py_mapper::metrics::compute_graph_metrics;
use py_mapper::parser::parse_python;
use py_mapper::report::{build_module_summary, build_report_rows};
use py_mapper::types::Splines;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_first_function_node(tree: &tree_sitter::Tree) -> Option<tree_sitter::Node<'_>> {
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        if node.kind() == "function_definition" {
            return Some(node);
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return None;
            }
        }
    }
}

fn make_parser() -> tree_sitter::Parser {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .expect("failed to load Python grammar");
    parser
}

// ---------------------------------------------------------------------------
// Parsing benchmarks
// ---------------------------------------------------------------------------

fn bench_parse_small(c: &mut Criterion) {
    let source = fs::read_to_string("tests/fixtures/complex.py").expect("read complex.py");
    c.bench_function("parse_small (complex.py)", |b| {
        b.iter(|| parse_python(black_box(&source)))
    });
}

fn bench_parse_large(c: &mut Criterion) {
    let source = fs::read_to_string("tests/fixtures/inheritance.py").expect("read inheritance.py");
    c.bench_function("parse_inheritance", |b| {
        b.iter(|| parse_python(black_box(&source)))
    });
}

fn bench_load_corpus(c: &mut Criterion) {
    let paths = vec!["tests/fixtures/cross_module/".to_string()];
    c.bench_function("load_modules (full corpus)", |b| {
        b.iter(|| load_modules(black_box(&paths)).expect("load benchmark fixtures"))
    });
}

// ---------------------------------------------------------------------------
// Complexity benchmarks
// ---------------------------------------------------------------------------

fn bench_complexity(c: &mut Criterion) {
    let source = fs::read_to_string("tests/fixtures/complex.py").expect("read complex.py");
    let src_bytes = source.as_bytes();
    let tree = make_parser().parse(&source, None).expect("parse");
    let func_node = find_first_function_node(&tree).expect("find function_definition");
    let body = func_node
        .child_by_field_name("body")
        .expect("function body");

    let mut group = c.benchmark_group("complexity");
    group.bench_function("mccabe", |b| {
        b.iter(|| compute_complexity(black_box(&body)))
    });
    group.bench_function("cognitive", |b| {
        b.iter(|| compute_cognitive_complexity(black_box(&body)))
    });
    group.bench_function("max_nesting", |b| {
        b.iter(|| compute_max_nesting(black_box(&body)))
    });
    group.bench_function("halstead", |b| {
        b.iter(|| compute_halstead(black_box(&body), black_box(src_bytes)))
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Graph benchmarks
// ---------------------------------------------------------------------------

fn bench_graph(c: &mut Criterion) {
    let paths = vec!["tests/fixtures/cross_module/".to_string()];
    let combined = load_modules(&paths).unwrap();

    let mut group = c.benchmark_group("graph");
    group.bench_function("build_call_graph", |b| {
        b.iter(|| build_call_graph(black_box(&combined), false))
    });

    let call_graph = build_call_graph(&combined, false);
    group.bench_function("compute_graph_metrics", |b| {
        b.iter(|| compute_graph_metrics(black_box(&call_graph), black_box(&combined.functions)))
    });
    group.bench_function("emit_dot", |b| {
        b.iter(|| emit_dot(black_box(&combined), black_box(&call_graph), Splines::Ortho))
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Analysis benchmarks
// ---------------------------------------------------------------------------

fn bench_analysis(c: &mut Criterion) {
    let paths = vec!["tests/fixtures/cross_module/".to_string()];
    let combined = load_modules(&paths).unwrap();
    let call_graph = build_call_graph(&combined, false);
    let gm = compute_graph_metrics(&call_graph, &combined.functions);

    let mut group = c.benchmark_group("analysis");
    group.bench_function("find_cycles", |b| {
        b.iter(|| find_cycles(black_box(&call_graph)))
    });
    group.bench_function("find_dead_code", |b| {
        b.iter(|| {
            find_dead_code(
                black_box(&combined.functions),
                black_box(&combined.classes),
                black_box(&gm),
            )
        })
    });
    group.bench_function("build_report_rows", |b| {
        b.iter(|| build_report_rows(black_box(&combined), black_box(&gm), None, None))
    });
    group.bench_function("build_module_summary", |b| {
        b.iter(|| build_module_summary(black_box(&combined), black_box(&gm), None))
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion harness
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_parse_small,
    bench_parse_large,
    bench_load_corpus,
    bench_complexity,
    bench_graph,
    bench_analysis,
);
criterion_main!(benches);
