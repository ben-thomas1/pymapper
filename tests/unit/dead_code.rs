use py_mapper::dead_code::{
    DeadCodeClass, DeadCodeOptions, find_dead_code, find_dead_code_classified, print_dead_code,
    print_dead_code_classified,
};
use py_mapper::graph::build_call_graph;
use py_mapper::metrics::compute_graph_metrics;
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
fn find_dead_code_detects_uncalled() {
    let module = parsed_module(
        "\
def called():
    pass

def caller():
    called()

def uncalled():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let dead = find_dead_code(&module.functions, &module.classes, &gm);
    let dead_names: Vec<_> = dead.iter().map(|f| f.name.as_str()).collect();
    assert!(dead_names.contains(&"uncalled"));
    assert!(dead_names.contains(&"caller")); // caller itself has zero callers
    assert!(!dead_names.contains(&"called")); // called has fan-in > 0
}

#[test]
fn find_dead_code_excludes_dunder() {
    let module = parsed_module(
        "\
class C:
    def __init__(self):
        pass

    def unused(self):
        pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let dead = find_dead_code(&module.functions, &module.classes, &gm);
    let dead_names: Vec<_> = dead.iter().map(|f| f.name.as_str()).collect();
    assert!(!dead_names.contains(&"__init__"));
    assert!(dead_names.contains(&"unused"));
}

#[test]
fn find_dead_code_excludes_main() {
    let module = parsed_module(
        "\
def main():
    pass

def unused():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let dead = find_dead_code(&module.functions, &module.classes, &gm);
    let dead_names: Vec<_> = dead.iter().map(|f| f.name.as_str()).collect();
    assert!(!dead_names.contains(&"main"));
    assert!(dead_names.contains(&"unused"));
}

#[test]
fn classify_truly_dead() {
    let module = parsed_module(
        "\
def used():
    pass

def caller():
    used()

def dead_func():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        classify: true,
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);

    let dead_entry = entries.iter().find(|e| e.func.name == "dead_func").unwrap();
    assert_eq!(dead_entry.classification, DeadCodeClass::TrulyDead);
}

#[test]
fn classify_test_fixture() {
    let module = parsed_module(
        "\
def test_something():
    pass

def helper():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        classify: true,
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);

    let test_entry = entries
        .iter()
        .find(|e| e.func.name == "test_something")
        .unwrap();
    assert_eq!(test_entry.classification, DeadCodeClass::TestFixture);
}

#[test]
fn classify_override() {
    let module = parsed_module(
        "\
class Base:
    def method(self):
        pass

class Child(Base):
    def method(self):
        pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        classify: true,
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);

    let child_method = entries
        .iter()
        .find(|e| e.func.name == "method" && e.func.parent_class.as_deref() == Some("Child"))
        .unwrap();
    assert_eq!(child_method.classification, DeadCodeClass::Override);
}

#[test]
fn exclude_decorated_filters() {
    let module = parsed_module(
        "\
@app.route(\"/\")
def index():
    pass

def unused():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        exclude_decorated: vec!["app.route".to_string()],
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    // index should be filtered out (it's an entry point)
    let names: Vec<_> = entries.iter().map(|e| e.func.name.as_str()).collect();
    assert!(!names.contains(&"index"));
    assert!(names.contains(&"unused"));
}

#[test]
fn profile_web_excludes_routes() {
    let module = parsed_module(
        "\
@app.route(\"/\")
def index():
    pass

def orphan():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        profile: vec!["web".to_string()],
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    let names: Vec<_> = entries.iter().map(|e| e.func.name.as_str()).collect();
    assert!(!names.contains(&"index"));
}

#[test]
fn print_dead_code_table() {
    let module = parsed_module("def orphan():\n    pass\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let dead = find_dead_code(&module.functions, &module.classes, &gm);
    print_dead_code(&dead, OutputFormat::Table);
}

#[test]
fn print_dead_code_json() {
    let module = parsed_module("def orphan():\n    pass\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let dead = find_dead_code(&module.functions, &module.classes, &gm);
    print_dead_code(&dead, OutputFormat::Json);
}

#[test]
fn print_dead_code_csv() {
    let module = parsed_module("def orphan():\n    pass\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let dead = find_dead_code(&module.functions, &module.classes, &gm);
    print_dead_code(&dead, OutputFormat::Csv);
}

#[test]
fn print_dead_code_markdown() {
    let module = parsed_module("def orphan():\n    pass\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let dead = find_dead_code(&module.functions, &module.classes, &gm);
    print_dead_code(&dead, OutputFormat::Markdown);
}

#[test]
fn print_dead_code_classified_table() {
    let module = parsed_module("def orphan():\n    pass\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        classify: true,
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    print_dead_code_classified(&entries, OutputFormat::Table);
}

#[test]
fn print_dead_code_classified_json() {
    let module = parsed_module("def orphan():\n    pass\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        classify: true,
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    print_dead_code_classified(&entries, OutputFormat::Json);
}

#[test]
fn print_dead_code_empty_table() {
    let empty: Vec<&py_mapper::types::FunctionInfo> = vec![];
    print_dead_code(&empty, OutputFormat::Table);
}

#[test]
fn classify_entry_point_via_pydantic_decorator() {
    let module = parsed_module(
        "\
@field_validator
def validate_name(cls, v):
    return v
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        classify: true,
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    let entry = entries
        .iter()
        .find(|e| e.func.name == "validate_name")
        .unwrap();
    assert_eq!(entry.classification, DeadCodeClass::EntryPoint);
}

#[test]
fn profile_cli_excludes() {
    let module = parsed_module(
        "\
@click.command
def cli_main():
    pass

def orphan():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        profile: vec!["cli".to_string()],
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    let names: Vec<_> = entries.iter().map(|e| e.func.name.as_str()).collect();
    assert!(!names.contains(&"cli_main"));
    assert!(names.contains(&"orphan"));
}

#[test]
fn exclude_overrides_filters() {
    let module = parsed_module(
        "\
class Base:
    def method(self):
        pass

class Child(Base):
    def method(self):
        pass

def orphan():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        exclude_overrides: true,
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    let has_child_method = entries
        .iter()
        .any(|e| e.func.name == "method" && e.func.parent_class.as_deref() == Some("Child"));
    assert!(!has_child_method, "override should be filtered out");
    let names: Vec<_> = entries.iter().map(|e| e.func.name.as_str()).collect();
    assert!(names.contains(&"orphan"));
}

#[test]
fn print_dead_code_classified_csv() {
    let module = parsed_module(
        "\
def orphan():
    pass

def test_thing():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        classify: true,
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    print_dead_code_classified(&entries, OutputFormat::Csv);
}

#[test]
fn print_dead_code_classified_markdown() {
    let module = parsed_module(
        "\
def orphan():
    pass

def test_thing():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        classify: true,
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    print_dead_code_classified(&entries, OutputFormat::Markdown);
}

#[test]
fn profile_pydantic_excludes() {
    let module = parsed_module(
        "\
@field_validator
def validate_name(cls, v):
    return v

def orphan():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        profile: vec!["pydantic".to_string()],
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    let names: Vec<_> = entries.iter().map(|e| e.func.name.as_str()).collect();
    assert!(!names.contains(&"validate_name"));
}

#[test]
fn profile_unknown_returns_empty() {
    let module = parsed_module("def orphan():\n    pass\n");
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        profile: vec!["nonexistent".to_string()],
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    assert!(!entries.is_empty());
}

#[test]
fn classify_with_user_exclude_pattern() {
    let module = parsed_module(
        "\
@custom.handler
def my_handler():
    pass

def orphan():
    pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        classify: true,
        exclude_decorated: vec!["custom.handler".to_string()],
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    let handler = entries
        .iter()
        .find(|e| e.func.name == "my_handler")
        .unwrap();
    assert_eq!(handler.classification, DeadCodeClass::EntryPoint);
}

#[test]
fn print_override_classification_table() {
    let module = parsed_module(
        "\
class Base:
    def method(self):
        pass

class Child(Base):
    def method(self):
        pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        classify: true,
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    let child_method = entries
        .iter()
        .find(|e| e.func.name == "method" && e.func.parent_class.as_deref() == Some("Child"))
        .unwrap();
    assert_eq!(child_method.classification, DeadCodeClass::Override);
    print_dead_code_classified(&entries, OutputFormat::Table);
}

#[test]
fn classify_inherited_excluded_from_output() {
    // Base method inherited by subclass should be excluded from dead code output
    let module = parsed_module(
        "\
class Base:
    def evaluate(self):
        pass

class Sub(Base):
    def other(self):
        pass
",
    );
    let cg = build_call_graph(&module, false);
    let gm = compute_graph_metrics(&cg, &module.functions);
    let options = DeadCodeOptions {
        classify: true,
        ..Default::default()
    };
    let entries = find_dead_code_classified(&module.functions, &module.classes, &gm, &options);
    let base_method = entries
        .iter()
        .find(|e| e.func.name == "evaluate" && e.func.parent_class.as_deref() == Some("Base"));
    assert!(
        base_method.is_none(),
        "inherited base method should be excluded from dead code output"
    );
    // Sub::other should still appear as dead
    let sub_method = entries
        .iter()
        .find(|e| e.func.name == "other" && e.func.parent_class.as_deref() == Some("Sub"));
    assert!(
        sub_method.is_some(),
        "Sub::other should still appear in dead code"
    );
}
