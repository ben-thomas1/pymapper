use py_mapper::parser::parse_python;

#[test]
fn parse_bare_function() {
    let src = "def foo(x):\n    return x + 1\n";
    let module = parse_python(src);
    assert_eq!(module.functions.len(), 1);
    assert_eq!(module.functions[0].name, "foo");
    assert_eq!(module.functions[0].param_count, 1);
    assert!(module.classes.is_empty());
}

#[test]
fn parse_class_with_methods() {
    let src = "\
class MyClass:
    def method_a(self):
        pass

    def method_b(self, x):
        self.method_a()
";
    let module = parse_python(src);
    assert_eq!(module.classes.len(), 1);
    assert_eq!(module.classes[0].name, "MyClass");
    assert_eq!(module.classes[0].method_names.len(), 2);
    // Methods should be in the functions list with parent_class set
    let methods: Vec<_> = module
        .functions
        .iter()
        .filter(|f| f.parent_class.is_some())
        .collect();
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0].parent_class.as_deref(), Some("MyClass"));
}

#[test]
fn param_count_excludes_self() {
    let src = "\
class C:
    def m(self, a, b):
        pass
";
    let module = parse_python(src);
    let m = &module.functions[0];
    assert_eq!(m.name, "m");
    assert_eq!(m.param_count, 2); // self excluded
}

#[test]
fn param_count_excludes_cls() {
    let src = "\
class C:
    @classmethod
    def create(cls, value):
        pass
";
    let module = parse_python(src);
    let m = module
        .functions
        .iter()
        .find(|f| f.name == "create")
        .unwrap();
    assert_eq!(m.param_count, 1); // cls excluded
}

#[test]
fn decorator_extraction() {
    let src = "\
@my_decorator
def decorated():
    pass

@app.route(\"/index\")
def index():
    pass
";
    let module = parse_python(src);
    let decorated = module
        .functions
        .iter()
        .find(|f| f.name == "decorated")
        .unwrap();
    assert!(decorated.decorators.contains(&"my_decorator".to_string()));

    let index = module.functions.iter().find(|f| f.name == "index").unwrap();
    assert!(index.decorators.contains(&"app.route".to_string()));
}

#[test]
fn call_target_regular() {
    let src = "def foo():\n    bar()\n";
    let module = parse_python(src);
    let calls = &module.functions[0].calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "bar");
    assert!(!calls[0].is_self);
}

#[test]
fn call_target_self_method() {
    let src = "\
class C:
    def a(self):
        self.b()
";
    let module = parse_python(src);
    let a = &module.functions[0];
    assert_eq!(a.calls.len(), 1);
    assert_eq!(a.calls[0].name, "b");
    assert!(a.calls[0].is_self);
}

#[test]
fn call_target_chained() {
    let src = "def foo():\n    obj.method()\n";
    let module = parse_python(src);
    let calls = &module.functions[0].calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "obj.method");
    assert!(!calls[0].is_self);
}

#[test]
fn empty_source() {
    let module = parse_python("");
    assert!(module.functions.is_empty());
    assert!(module.classes.is_empty());
}

#[test]
fn syntax_error_source_is_reported() {
    assert!(py_mapper::parser::parse_python_checked("def broken(\n").is_err());
}

#[test]
fn base_classes_extracted() {
    let src = "class Child(Parent, Mixin):\n    pass\n";
    let module = parse_python(src);
    assert_eq!(module.classes[0].base_classes, vec!["Parent", "Mixin"]);
}

#[test]
fn return_count() {
    let src = "\
def multi_return(x):
    if x > 0:
        return 1
    return 0
";
    let module = parse_python(src);
    assert_eq!(module.functions[0].return_count, 2);
}

#[test]
fn bool_expr_count() {
    let src = "\
def check(a, b, c):
    if a and b:
        pass
    if b or c:
        pass
";
    let module = parse_python(src);
    assert_eq!(module.functions[0].bool_expr_count, 2);
}

#[test]
fn decorated_class_at_module_level() {
    let src = "\
@some_decorator
class MyClass:
    def method(self):
        pass
";
    let module = parse_python(src);
    assert_eq!(module.classes.len(), 1);
    assert_eq!(module.classes[0].name, "MyClass");
    assert_eq!(module.functions.len(), 1);
    assert_eq!(module.functions[0].name, "method");
}

#[test]
fn function_no_params_node() {
    // Lambda-like bare function — tree-sitter always has parameters node,
    // but test a minimal case to exercise the path
    let src = "def bare():\n    pass\n";
    let module = parse_python(src);
    assert_eq!(module.functions[0].param_count, 0);
}

#[test]
fn call_on_call_result() {
    // foo()() — the inner call is on a call expression, not an identifier/attribute
    let src = "def bar():\n    foo()()\n";
    let module = parse_python(src);
    // foo() should be extracted, the outer call on the result is not
    let calls = &module.functions[0].calls;
    assert!(calls.iter().any(|c| c.name == "foo"));
}

// --- Import extraction tests ---

#[test]
fn import_plain() {
    let module = parse_python("import utils\n");
    assert_eq!(module.imports.len(), 1);
    assert_eq!(module.imports[0].local_name, "utils");
    assert_eq!(module.imports[0].source_module, "utils");
    assert!(module.imports[0].imported_name.is_none());
}

#[test]
fn import_dotted() {
    let module = parse_python("import utils.helpers\n");
    assert_eq!(module.imports.len(), 1);
    assert_eq!(module.imports[0].local_name, "utils.helpers");
    assert_eq!(module.imports[0].source_module, "utils.helpers");
    assert!(module.imports[0].imported_name.is_none());
}

#[test]
fn import_aliased() {
    let module = parse_python("import utils as u\n");
    assert_eq!(module.imports.len(), 1);
    assert_eq!(module.imports[0].local_name, "u");
    assert_eq!(module.imports[0].source_module, "utils");
    assert!(module.imports[0].imported_name.is_none());
}

#[test]
fn from_import() {
    let module = parse_python("from utils import helper\n");
    assert_eq!(module.imports.len(), 1);
    assert_eq!(module.imports[0].local_name, "helper");
    assert_eq!(module.imports[0].source_module, "utils");
    assert_eq!(module.imports[0].imported_name.as_deref(), Some("helper"));
}

#[test]
fn from_import_aliased() {
    let module = parse_python("from utils import helper as h\n");
    assert_eq!(module.imports.len(), 1);
    assert_eq!(module.imports[0].local_name, "h");
    assert_eq!(module.imports[0].source_module, "utils");
    assert_eq!(module.imports[0].imported_name.as_deref(), Some("helper"));
}

#[test]
fn from_import_multiple() {
    let module = parse_python("from utils import helper, formatter\n");
    assert_eq!(module.imports.len(), 2);
    assert_eq!(module.imports[0].local_name, "helper");
    assert_eq!(module.imports[1].local_name, "formatter");
    assert_eq!(module.imports[0].source_module, "utils");
    assert_eq!(module.imports[1].source_module, "utils");
}

#[test]
fn from_import_wildcard_skipped() {
    let module = parse_python("from utils import *\n");
    assert!(module.imports.is_empty());
}

#[test]
fn decorated_method_in_class() {
    let src = "\
class C:
    @staticmethod
    def static_method():
        pass

    @classmethod
    def class_method(cls):
        pass
";
    let module = parse_python(src);
    let static_m = module
        .functions
        .iter()
        .find(|f| f.name == "static_method")
        .unwrap();
    assert!(static_m.decorators.contains(&"staticmethod".to_string()));
    let class_m = module
        .functions
        .iter()
        .find(|f| f.name == "class_method")
        .unwrap();
    assert!(class_m.decorators.contains(&"classmethod".to_string()));
}

#[test]
fn return_count_excludes_nested_def() {
    let src = "\
def outer():
    def inner():
        return 1
    return inner
";
    let module = parse_python(src);
    assert_eq!(
        module.functions[0].return_count, 1,
        "only outer return should count"
    );
}
