use py_mapper::types::{FunctionInfo, ModuleInfo, Splines, digit_width};

#[test]
fn digit_width_zero() {
    assert_eq!(digit_width(0), 1);
}

#[test]
fn digit_width_single_digits() {
    assert_eq!(digit_width(1), 1);
    assert_eq!(digit_width(9), 1);
}

#[test]
fn digit_width_two_digits() {
    assert_eq!(digit_width(10), 2);
    assert_eq!(digit_width(99), 2);
}

#[test]
fn digit_width_three_digits() {
    assert_eq!(digit_width(100), 3);
}

#[test]
fn digit_width_four_digits() {
    assert_eq!(digit_width(1000), 4);
}

fn make_func(name: &str, parent_class: Option<&str>, module: &str) -> FunctionInfo {
    FunctionInfo {
        name: name.to_string(),
        start_line: 1,
        end_line: 1,
        complexity: 1,
        cognitive_complexity: 0,
        max_nesting: 0,
        line_count: 1,
        param_count: 0,
        parent_class: parent_class.map(String::from),
        return_count: 0,
        bool_expr_count: 0,
        halstead_volume: 0.0,
        maintainability_index: 100.0,
        calls: vec![],
        decorators: vec![],
        source_module: module.to_string(),
        source_file: String::new(),
    }
}

#[test]
fn qualified_name_bare_function() {
    let f = make_func("foo", None, "mymod");
    assert_eq!(f.qualified_name(), "mymod::foo");
}

#[test]
fn qualified_name_method() {
    let f = make_func("bar", Some("MyClass"), "mymod");
    assert_eq!(f.qualified_name(), "mymod::MyClass.bar");
}

#[test]
fn qualified_name_no_module() {
    let f = make_func("baz", None, "");
    assert_eq!(f.qualified_name(), "baz");
}

#[test]
fn qualified_name_method_no_module() {
    let f = make_func("do_thing", Some("Cls"), "");
    assert_eq!(f.qualified_name(), "Cls.do_thing");
}

#[test]
fn module_info_merge() {
    let mut a = ModuleInfo::default();
    a.functions.push(make_func("f1", None, ""));
    let mut b = ModuleInfo::default();
    b.functions.push(make_func("f2", None, ""));
    b.functions.push(make_func("f3", None, ""));

    a.merge(b);
    assert_eq!(a.functions.len(), 3);
}

#[test]
fn splines_as_dot_str() {
    assert_eq!(Splines::Ortho.as_dot_str(), "ortho");
    assert_eq!(Splines::Polyline.as_dot_str(), "polyline");
    assert_eq!(Splines::Curved.as_dot_str(), "curved");
    assert_eq!(Splines::Spline.as_dot_str(), "spline");
    assert_eq!(Splines::Line.as_dot_str(), "line");
}
