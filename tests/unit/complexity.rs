use py_mapper::parser::parse_python;

fn get_complexity(src: &str) -> (usize, usize, usize) {
    let module = parse_python(src);
    let f = &module.functions[0];
    (f.complexity, f.cognitive_complexity, f.max_nesting)
}

#[test]
fn linear_function_cc_1() {
    let src = "def foo():\n    return 42\n";
    let (cc, _, _) = get_complexity(src);
    assert_eq!(cc, 1);
}

#[test]
fn single_if_cc_2() {
    let src = "\
def foo(x):
    if x > 0:
        return 1
    return 0
";
    let (cc, _, _) = get_complexity(src);
    assert_eq!(cc, 2);
}

#[test]
fn if_elif_else_cc() {
    let src = "\
def foo(x):
    if x > 0:
        return 1
    elif x == 0:
        return 0
    else:
        return -1
";
    let (cc, _, _) = get_complexity(src);
    assert_eq!(cc, 3); // 1 (base) + if + elif
}

#[test]
fn loop_cc() {
    let src = "\
def foo(items):
    for item in items:
        if item > 0:
            print(item)
";
    let (cc, _, _) = get_complexity(src);
    assert_eq!(cc, 3); // 1 + for + if
}

#[test]
fn boolean_ops_add_cc() {
    let src = "\
def foo(a, b, c):
    if a and b or c:
        return True
    return False
";
    let (cc, _, _) = get_complexity(src);
    // if + and + or = 3 decision points, +1 base = 4
    assert_eq!(cc, 4);
}

#[test]
fn cognitive_nested_higher() {
    let flat = "\
def flat(x, y):
    if x > 0:
        pass
    if y > 0:
        pass
";
    let nested = "\
def nested(x, y):
    if x > 0:
        if y > 0:
            pass
";
    let (_, cog_flat, _) = get_complexity(flat);
    let (_, cog_nested, _) = get_complexity(nested);
    assert!(
        cog_nested > cog_flat,
        "nested ({}) should be higher than flat ({})",
        cog_nested,
        cog_flat
    );
}

#[test]
fn max_nesting_depth() {
    let src = "\
def deep(x):
    if x > 0:
        for i in range(x):
            if i > 5:
                while True:
                    break
";
    let (_, _, nesting) = get_complexity(src);
    assert_eq!(nesting, 4);
}

#[test]
fn max_nesting_flat() {
    let src = "def flat():\n    return 1\n";
    let (_, _, nesting) = get_complexity(src);
    assert_eq!(nesting, 0);
}

#[test]
fn maintainability_index_range() {
    let src = "\
def foo(x):
    if x > 0:
        return x
    return 0
";
    let module = parse_python(src);
    let mi = module.functions[0].maintainability_index;
    assert!(
        mi >= 0.0 && mi <= 100.0,
        "MI should be in [0, 100], got {}",
        mi
    );
}

#[test]
fn halstead_volume_positive_for_nontrivial() {
    let src = "\
def calc(a, b):
    x = a + b
    y = x * 2
    return y
";
    let module = parse_python(src);
    assert!(module.functions[0].halstead_volume > 0.0);
}

#[test]
fn maintainability_index_zero_loc() {
    use py_mapper::complexity::compute_maintainability_index;
    let mi = compute_maintainability_index(0.0, 1, 0);
    assert_eq!(mi, 100.0);
}

#[test]
fn nested_def_excluded_from_mccabe() {
    // Inner function's try/except should NOT count in outer McC
    let src = "\
def instrument(func):
    def wrapper(*args, **kwargs):
        try:
            result = func(*args, **kwargs)
            return result
        except Exception as e:
            raise
    return wrapper
";
    let (cc, _, _) = get_complexity(src);
    assert_eq!(
        cc, 1,
        "outer function has no decision points, McC should be 1"
    );
}

#[test]
fn nested_def_excluded_from_cognitive() {
    // Inner function's except should NOT count in outer cognitive complexity
    let src = "\
def instrument(func):
    def wrapper(*args, **kwargs):
        try:
            result = func(*args, **kwargs)
            return result
        except Exception as e:
            raise
    return wrapper
";
    let (_, cog, _) = get_complexity(src);
    assert_eq!(cog, 0, "cognitive should exclude inner def, got {}", cog);
}

#[test]
fn nested_def_excluded_from_max_nesting() {
    // Inner function's nesting should NOT count in outer max nesting
    let src = "\
def outer():
    def inner():
        for x in range(10):
            if x > 5:
                while True:
                    break
    return inner
";
    let (_, _, nesting) = get_complexity(src);
    assert_eq!(
        nesting, 0,
        "outer has no control flow, max nesting should be 0"
    );
}

#[test]
fn nested_lambda_excluded_from_mccabe() {
    // Lambda's conditional should NOT count in outer McC
    let src = "\
def foo(items):
    fn = lambda x: x if x > 0 else -x
    return fn
";
    let (cc, _, _) = get_complexity(src);
    assert_eq!(
        cc, 1,
        "outer function has no decision points, McC should be 1"
    );
}

#[test]
fn nested_def_excluded_from_halstead() {
    // Inner function tokens should NOT inflate outer Halstead volume
    let outer_only = "\
def simple():
    return 1
";
    let with_inner = "\
def simple():
    def complex_inner(a, b, c, d, e):
        x = a + b * c - d / e
        y = x ** 2
        return y
    return 1
";
    let module_simple = parse_python(outer_only);
    let module_with_inner = parse_python(with_inner);
    let vol_simple = module_simple.functions[0].halstead_volume;
    let vol_with_inner = module_with_inner.functions[0].halstead_volume;
    assert_eq!(
        vol_simple, vol_with_inner,
        "Halstead volume should be same with or without inner function: {} vs {}",
        vol_simple, vol_with_inner
    );
}
