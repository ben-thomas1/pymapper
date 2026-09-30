use tree_sitter::{Node, Parser};

use crate::complexity::{
    compute_cognitive_complexity, compute_complexity, compute_halstead,
    compute_maintainability_index, compute_max_nesting,
};
use crate::types::{CallTarget, ClassInfo, FunctionInfo, ImportEntry, ModuleInfo};

/// Parses a Python source string and extracts module structure.
pub fn parse_python(source: &str) -> ModuleInfo {
    parse_python_checked(source).expect("invalid Python source")
}

/// Parse source, reporting syntax errors before extracting any results.
pub fn parse_python_checked(source: &str) -> Result<ModuleInfo, String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .map_err(|e| format!("failed to load Python grammar: {}", e))?;

    let tree = parser.parse(source, None).ok_or("failed to parse source")?;
    let root = tree.root_node();
    validate_tree(root)?;
    let src = source.as_bytes();

    let mut module = ModuleInfo::default();

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                let func = extract_function(&child, src, None);
                module.functions.push(func);
            }
            "class_definition" => {
                let (class_info, methods) = extract_class(&child, src);
                module.classes.push(class_info);
                module.functions.extend(methods);
            }
            "decorated_definition" => {
                let decorators = extract_decorators(&child, src);
                let inner =
                    find_decorated_inner(&child).expect("decorated_definition must have inner");
                match inner.kind() {
                    "function_definition" => {
                        let mut func = extract_function(&inner, src, None);
                        func.decorators = decorators;
                        module.functions.push(func);
                    }
                    "class_definition" => {
                        let (class_info, methods) = extract_class(&inner, src);
                        module.classes.push(class_info);
                        module.functions.extend(methods);
                    }
                    _ => {}
                }
            }
            "import_statement" => {
                module.imports.extend(extract_import_statement(&child, src));
            }
            "import_from_statement" => {
                module
                    .imports
                    .extend(extract_import_from_statement(&child, src));
            }
            _ => {}
        }
    }

    Ok(module)
}

/// Bound recursive analysis and reject tree-sitter error recovery trees.
pub(crate) fn validate_tree(root: Node) -> Result<(), String> {
    if root.has_error() {
        return Err("Python syntax error".to_string());
    }
    let mut cursor = root.walk();
    let mut depth = 0;
    loop {
        if cursor.goto_first_child() {
            depth += 1;
            if depth > 256 {
                return Err("syntax nesting exceeds the analysis limit of 256".to_string());
            }
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return Ok(());
            }
            depth -= 1;
        }
    }
}

/// Extracts decorator names from a `decorated_definition` node.
fn extract_decorators(node: &Node, src: &[u8]) -> Vec<String> {
    let mut decorators = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "decorator" {
            // The decorator node contains '@' followed by the expression.
            // Walk children to find the name (identifier, attribute, or call).
            let mut inner_cursor = child.walk();
            for inner in child.children(&mut inner_cursor) {
                match inner.kind() {
                    "identifier" | "attribute" => {
                        decorators.push(inner.utf8_text(src).unwrap_or("").to_string());
                    }
                    "call" => {
                        // Extract just the function name, not the arguments
                        if let Some(func_node) = inner.child_by_field_name("function") {
                            decorators.push(func_node.utf8_text(src).unwrap_or("").to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    decorators
}

fn find_decorated_inner<'a>(node: &'a Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" | "class_definition" => return Some(child),
            _ => {}
        }
    }
    None
}

fn extract_function(node: &Node, src: &[u8], parent_class: Option<&str>) -> FunctionInfo {
    let name = node
        .child_by_field_name("name")
        .map(|n| n.utf8_text(src).unwrap().to_string())
        .unwrap_or_default();

    let body = node.child_by_field_name("body");
    let calls = body.map(|b| extract_calls(&b, src)).unwrap_or_default();
    let complexity = body.map(|b| compute_complexity(&b)).unwrap_or(1);
    let cognitive_complexity = body.map(|b| compute_cognitive_complexity(&b)).unwrap_or(0);
    let max_nesting = body.map(|b| compute_max_nesting(&b)).unwrap_or(0);

    let param_count = count_params(node, src);
    let return_count = body
        .map(|b| count_node_kind(&b, "return_statement"))
        .unwrap_or(0);
    let bool_expr_count = body
        .map(|b| count_node_kind(&b, "boolean_operator"))
        .unwrap_or(0);
    let halstead =
        body.map(|b| compute_halstead(&b, src))
            .unwrap_or(crate::complexity::HalsteadMetrics {
                volume: 0.0,
                difficulty: 0.0,
                effort: 0.0,
            });

    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;
    let line_count = end_line - start_line + 1;
    let mi = compute_maintainability_index(halstead.volume, complexity, line_count);

    FunctionInfo {
        name,
        start_line,
        end_line,
        complexity,
        cognitive_complexity,
        max_nesting,
        line_count,
        param_count,
        return_count,
        bool_expr_count,
        halstead_volume: halstead.volume,
        maintainability_index: mi,
        parent_class: parent_class.map(String::from),
        calls,
        decorators: Vec::new(),
        source_module: String::new(),
        source_file: String::new(),
    }
}

fn count_params(node: &Node, src: &[u8]) -> usize {
    let params = node
        .child_by_field_name("parameters")
        .expect("function_definition must have parameters");
    let mut count = 0;
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        match child.kind() {
            "identifier"
            | "default_parameter"
            | "typed_parameter"
            | "typed_default_parameter"
            | "list_splat_pattern"
            | "dictionary_splat_pattern" => {
                let text = child.utf8_text(src).unwrap_or("");
                if text != "self"
                    && !text.starts_with("self:")
                    && !text.starts_with("self=")
                    && text != "cls"
                    && !text.starts_with("cls:")
                    && !text.starts_with("cls=")
                {
                    count += 1;
                }
            }
            _ => {}
        }
    }
    count
}

fn extract_class(node: &Node, src: &[u8]) -> (ClassInfo, Vec<FunctionInfo>) {
    let name = node
        .child_by_field_name("name")
        .map(|n| n.utf8_text(src).unwrap().to_string())
        .unwrap_or_default();

    let base_classes = extract_base_classes(node, src);

    let body = node.child_by_field_name("body");
    let mut methods = Vec::new();
    let mut method_names = Vec::new();

    if let Some(body) = body {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            match child.kind() {
                "function_definition" => {
                    let func = extract_function(&child, src, Some(&name));
                    method_names.push(func.name.clone());
                    methods.push(func);
                }
                "decorated_definition" => {
                    let decorators = extract_decorators(&child, src);
                    let inner =
                        find_decorated_inner(&child).expect("decorated_definition must have inner");
                    if inner.kind() == "function_definition" {
                        let mut func = extract_function(&inner, src, Some(&name));
                        func.decorators = decorators;
                        method_names.push(func.name.clone());
                        methods.push(func);
                    }
                }
                _ => {}
            }
        }
    }

    let class_info = ClassInfo {
        name,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        base_classes,
        method_names,
        source_module: String::new(),
    };

    (class_info, methods)
}

fn extract_base_classes(node: &Node, src: &[u8]) -> Vec<String> {
    let mut bases = Vec::new();
    if let Some(superclasses) = node.child_by_field_name("superclasses") {
        let mut cursor = superclasses.walk();
        for child in superclasses.children(&mut cursor) {
            match child.kind() {
                "identifier" | "attribute" => {
                    bases.push(child.utf8_text(src).unwrap().to_string());
                }
                _ => {}
            }
        }
    }
    bases
}

/// Counts occurrences of a node kind within a subtree.
fn count_node_kind(node: &Node, kind: &str) -> usize {
    if node.kind() == "function_definition" || node.kind() == "lambda" {
        return 0;
    }
    let mut count = if node.kind() == kind { 1 } else { 0 };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count += count_node_kind(&child, kind);
    }
    count
}

fn extract_calls(node: &Node, src: &[u8]) -> Vec<CallTarget> {
    let mut calls = Vec::new();
    collect_calls(node, src, &mut calls);
    calls
}

fn collect_calls(node: &Node, src: &[u8], calls: &mut Vec<CallTarget>) {
    if node.kind() == "call" {
        if let Some(func_node) = node.child_by_field_name("function") {
            match func_node.kind() {
                "identifier" => {
                    let name = func_node.utf8_text(src).unwrap().to_string();
                    calls.push(CallTarget {
                        name,
                        is_self: false,
                    });
                }
                "attribute" => {
                    let obj = func_node
                        .child_by_field_name("object")
                        .expect("attribute must have object");
                    let attr = func_node
                        .child_by_field_name("attribute")
                        .expect("attribute must have attribute");
                    let obj_text = obj.utf8_text(src).unwrap();
                    let attr_text = attr.utf8_text(src).unwrap().to_string();
                    if obj_text == "self" {
                        calls.push(CallTarget {
                            name: attr_text,
                            is_self: true,
                        });
                    } else {
                        calls.push(CallTarget {
                            name: format!("{}.{}", obj_text, attr_text),
                            is_self: false,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "function_definition" || child.kind() == "lambda" {
            continue;
        }
        collect_calls(&child, src, calls);
    }
}

/// Extracts imports from an `import_statement` node (e.g., `import utils`, `import utils as u`).
fn extract_import_statement(node: &Node, src: &[u8]) -> Vec<ImportEntry> {
    let mut imports = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "dotted_name" => {
                let name = child.utf8_text(src).unwrap_or("").to_string();
                imports.push(ImportEntry {
                    local_name: name.clone(),
                    source_module: name,
                    imported_name: None,
                    owning_module: String::new(),
                });
            }
            "aliased_import" => {
                let name_node = child.child_by_field_name("name");
                let alias_node = child.child_by_field_name("alias");
                if let Some(name_node) = name_node {
                    let source = name_node.utf8_text(src).unwrap_or("").to_string();
                    let local = alias_node
                        .map(|a| a.utf8_text(src).unwrap_or("").to_string())
                        .unwrap_or_else(|| source.clone());
                    imports.push(ImportEntry {
                        local_name: local,
                        source_module: source,
                        imported_name: None,
                        owning_module: String::new(),
                    });
                }
            }
            _ => {}
        }
    }
    imports
}

/// Extracts imports from an `import_from_statement` node (e.g., `from utils import helper`).
fn extract_import_from_statement(node: &Node, src: &[u8]) -> Vec<ImportEntry> {
    let mut imports = Vec::new();

    let module_name = node
        .child_by_field_name("module_name")
        .map(|n| n.utf8_text(src).unwrap_or("").to_string())
        .unwrap_or_default();

    if module_name.is_empty() {
        return imports;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "dotted_name" | "identifier" => {
                // Skip the module_name node itself (already captured via field)
                if child.id()
                    == node
                        .child_by_field_name("module_name")
                        .map(|n| n.id())
                        .unwrap_or(0)
                {
                    continue;
                }
                let name = child.utf8_text(src).unwrap_or("").to_string();
                imports.push(ImportEntry {
                    local_name: name.clone(),
                    source_module: module_name.clone(),
                    imported_name: Some(name),
                    owning_module: String::new(),
                });
            }
            "aliased_import" => {
                let name_node = child.child_by_field_name("name");
                let alias_node = child.child_by_field_name("alias");
                if let Some(name_node) = name_node {
                    let original = name_node.utf8_text(src).unwrap_or("").to_string();
                    let local = alias_node
                        .map(|a| a.utf8_text(src).unwrap_or("").to_string())
                        .unwrap_or_else(|| original.clone());
                    imports.push(ImportEntry {
                        local_name: local,
                        source_module: module_name.clone(),
                        imported_name: Some(original),
                        owning_module: String::new(),
                    });
                }
            }
            "wildcard_import" => {
                // Skip `from X import *`
            }
            _ => {}
        }
    }
    imports
}
