use py_mapper::graph::{build_call_graph, emit_dot};
use py_mapper::parser::parse_python;
use py_mapper::types::Splines;

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
fn build_graph_creates_nodes() {
    let module = parsed_module(
        "\
def foo():
    bar()

def bar():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    assert!(cg.node_map.contains_key("test::foo"));
    assert!(cg.node_map.contains_key("test::bar"));
}

#[test]
fn build_graph_creates_edges() {
    let module = parsed_module(
        "\
def foo():
    bar()

def bar():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let foo_idx = cg.node_map["test::foo"];
    let bar_idx = cg.node_map["test::bar"];
    assert!(cg.graph.contains_edge(foo_idx, bar_idx));
}

#[test]
fn build_graph_external_node() {
    let module = parsed_module(
        "\
def foo():
    unknown()
",
    );
    let cg = build_call_graph(&module, false);
    assert!(cg.external_nodes.contains("unknown"));
    assert!(cg.node_map.contains_key("unknown"));
}

#[test]
fn build_graph_no_externals() {
    let module = parsed_module(
        "\
def foo():
    unknown()
",
    );
    let cg = build_call_graph(&module, true);
    assert!(cg.external_nodes.is_empty());
    assert!(!cg.node_map.contains_key("unknown"));
}

#[test]
fn emit_dot_contains_digraph() {
    let module = parsed_module("def foo():\n    pass\n");
    let cg = build_call_graph(&module, false);
    let dot = emit_dot(&module, &cg, Splines::Ortho);
    assert!(dot.contains("digraph PyMapper"));
    assert!(dot.contains("test::foo"));
}

#[test]
fn emit_dot_contains_subgraph_cluster() {
    let module = parsed_module(
        "\
class MyClass:
    def method(self):
        pass
",
    );
    let cg = build_call_graph(&module, false);
    let dot = emit_dot(&module, &cg, Splines::Ortho);
    assert!(dot.contains("subgraph cluster_"));
    assert!(dot.contains("MyClass"));
}

#[test]
fn emit_dot_contains_edges() {
    let module = parsed_module(
        "\
def caller():
    callee()

def callee():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let dot = emit_dot(&module, &cg, Splines::Ortho);
    assert!(dot.contains("->"));
}

#[test]
fn self_call_resolves_to_same_class() {
    let module = parsed_module(
        "\
class C:
    def a(self):
        self.b()

    def b(self):
        pass
",
    );
    let cg = build_call_graph(&module, false);
    let a_idx = cg.node_map["test::C.a"];
    let b_idx = cg.node_map["test::C.b"];
    assert!(cg.graph.contains_edge(a_idx, b_idx));
}

fn root_module(src: &str) -> py_mapper::types::ModuleInfo {
    // No source_module set — simulates root module (empty prefix)
    parse_python(src)
}

#[test]
fn emit_dot_root_module_class() {
    let module = root_module(
        "\
class RootClass:
    def method(self):
        pass
",
    );
    let cg = build_call_graph(&module, false);
    let dot = emit_dot(&module, &cg, Splines::Ortho);
    assert!(dot.contains("RootClass"));
    assert!(dot.contains("cluster_"));
    // Root module method should be qualified as "RootClass.method"
    assert!(dot.contains("RootClass.method"));
}

#[test]
fn resolve_call_known_name_without_prefix() {
    // Two functions in root module, one calls the other
    let module = root_module(
        "\
def caller():
    callee()

def callee():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let caller_idx = cg.node_map["caller"];
    let callee_idx = cg.node_map["callee"];
    assert!(cg.graph.contains_edge(caller_idx, callee_idx));
}

#[test]
fn resolve_call_unknown_passthrough() {
    let module = root_module(
        "\
def foo():
    totally_unknown()
",
    );
    let cg = build_call_graph(&module, false);
    assert!(cg.external_nodes.contains("totally_unknown"));
}

#[test]
fn self_call_without_source_module() {
    // Class in root module — self call resolution without module prefix
    let module = root_module(
        "\
class C:
    def a(self):
        self.b()

    def b(self):
        pass
",
    );
    let cg = build_call_graph(&module, false);
    let a_idx = cg.node_map["C.a"];
    let b_idx = cg.node_map["C.b"];
    assert!(cg.graph.contains_edge(a_idx, b_idx));
}
