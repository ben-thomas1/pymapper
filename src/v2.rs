use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

use serde::Serialize;
use tree_sitter::{Node, Parser};

use crate::complexity::{
    compute_cognitive_complexity, compute_complexity, compute_halstead,
    compute_maintainability_index, compute_max_nesting,
};
use crate::loader::{compute_base_path, module_prefix_from_path, try_collect_py_files};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    Table,
    Json,
    Dot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    General,
    Pytest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Confidence {
    Certain,
    Likely,
    Possible,
    Unresolved,
}

impl Confidence {
    pub fn includes(self, edge: Confidence) -> bool {
        edge <= self
    }

    fn label(self) -> &'static str {
        match self {
            Self::Certain => "certain",
            Self::Likely => "likely",
            Self::Possible => "possible",
            Self::Unresolved => "unresolved",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SymbolKind {
    Function,
    Method,
    NestedFunction,
    Class,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EdgeKind {
    Call,
    Decorator,
    Import,
    Inheritance,
    PytestEntrypoint,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnalysisBundle {
    pub schema_version: u32,
    pub project: ProjectSummary,
    pub modules: Vec<ModuleRecord>,
    pub symbols: Vec<SymbolRecord>,
    pub imports: Vec<ImportRecord>,
    pub edges: Vec<EdgeRecord>,
    pub metrics: Vec<MetricRecord>,
    pub findings: Vec<FindingRecord>,
    pub diagnostics: Vec<DiagnosticRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    pub root: String,
    pub file_count: usize,
    pub profile: Profile,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModuleRecord {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolRecord {
    pub id: String,
    pub kind: SymbolKind,
    pub name: String,
    pub qualified_name: String,
    pub module: String,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub parent_symbol: Option<String>,
    pub parent_class: Option<String>,
    pub decorators: Vec<String>,
    pub bases: Vec<String>,
    pub local_bindings: Vec<String>,
    pub complexity: usize,
    pub cognitive_complexity: usize,
    pub max_nesting: usize,
    pub loc: usize,
    pub maintainability_index: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportRecord {
    pub scope: Option<String>,
    pub module: String,
    pub local_name: String,
    pub source_module: String,
    pub imported_name: Option<String>,
    pub level: usize,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeRecord {
    pub from: String,
    pub to: Option<String>,
    pub target_name: String,
    pub kind: EdgeKind,
    pub confidence: Confidence,
    pub reason: String,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricRecord {
    pub symbol: String,
    pub fan_in: usize,
    pub fan_out: usize,
    pub reachable: usize,
    pub call_depth: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindingRecord {
    pub kind: String,
    pub severity: String,
    pub symbol: Option<String>,
    pub message: String,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticRecord {
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone)]
struct RawCall {
    name: String,
    receiver: Option<String>,
    line: usize,
}

#[derive(Debug, Clone)]
struct FileParse {
    module: ModuleRecord,
    symbols: Vec<SymbolRecord>,
    imports: Vec<ImportRecord>,
    calls: HashMap<String, Vec<RawCall>>,
    assignments: HashMap<String, HashMap<String, String>>,
}

pub fn analyze_paths(paths: &[String], profile: Profile) -> Result<AnalysisBundle, String> {
    let files = try_collect_py_files(paths)?;
    if files.is_empty() {
        return Err("no .py files found".to_string());
    }

    let base = compute_base_path(paths);
    let diagnostics = Vec::new();
    let mut parses = Vec::new();

    for path in files {
        match parse_file(&path, &base) {
            Ok(parsed) => parses.push(parsed),
            Err(message) => return Err(format!("{}: {}", path.display(), message)),
        }
    }

    if parses.is_empty() {
        return Err("no readable .py files found".to_string());
    }

    parses.sort_by(|a, b| {
        a.module
            .name
            .cmp(&b.module.name)
            .then(a.module.path.cmp(&b.module.path))
    });

    let modules: Vec<ModuleRecord> = parses.iter().map(|p| p.module.clone()).collect();
    let imports: Vec<ImportRecord> = parses.iter().flat_map(|p| p.imports.clone()).collect();
    let symbols: Vec<SymbolRecord> = parses.iter().flat_map(|p| p.symbols.clone()).collect();
    let calls: HashMap<String, Vec<RawCall>> =
        parses.iter().flat_map(|p| p.calls.clone()).collect();
    let assignments: HashMap<String, HashMap<String, String>> =
        parses.iter().flat_map(|p| p.assignments.clone()).collect();

    let resolver = Resolver::new(&modules, &symbols, &imports);
    let mut edges = build_edges(&symbols, &imports, &calls, &assignments, &resolver, profile);
    edges.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then(a.target_name.cmp(&b.target_name))
            .then(a.line.cmp(&b.line))
    });

    let metrics = compute_metrics(&symbols, &edges);
    let findings = build_findings(&symbols, &edges, &metrics, profile);

    Ok(AnalysisBundle {
        schema_version: 2,
        project: ProjectSummary {
            root: base.display().to_string(),
            file_count: modules.len(),
            profile,
        },
        modules,
        symbols,
        imports,
        edges,
        metrics,
        findings,
        diagnostics,
    })
}

fn parse_file(path: &Path, base: &Path) -> Result<FileParse, String> {
    let source = fs::read_to_string(path).map_err(|e| format!("failed to read file: {}", e))?;
    let module_name = module_prefix_from_path(path, base);
    let file = path.display().to_string();
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .map_err(|e| format!("failed to load Python grammar: {:?}", e))?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| "tree-sitter failed to parse source".to_string())?;
    let root = tree.root_node();
    crate::parser::validate_tree(root)?;
    let src = source.as_bytes();

    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    let mut calls = HashMap::new();
    let mut assignments = HashMap::new();
    collect_module_items(
        root,
        src,
        &file,
        &module_name,
        None,
        None,
        String::new(),
        &mut symbols,
        &mut imports,
        &mut calls,
        &mut assignments,
    );

    Ok(FileParse {
        module: ModuleRecord {
            name: module_name,
            path: file,
        },
        symbols,
        imports,
        calls,
        assignments,
    })
}

#[allow(clippy::too_many_arguments)]
fn collect_module_items(
    node: Node,
    src: &[u8],
    file: &str,
    module: &str,
    parent_symbol: Option<String>,
    parent_class: Option<String>,
    qual_prefix: String,
    symbols: &mut Vec<SymbolRecord>,
    imports: &mut Vec<ImportRecord>,
    calls: &mut HashMap<String, Vec<RawCall>>,
    assignments: &mut HashMap<String, HashMap<String, String>>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_statement" | "import_from_statement" => {
                let records = if child.kind() == "import_statement" {
                    extract_imports(child, src, module)
                } else {
                    extract_from_imports(child, src, module, file)
                };
                imports.extend(records.into_iter().map(|mut record| {
                    record.scope = parent_symbol.clone();
                    record
                }));
            }
            "function_definition" => {
                collect_function(
                    child,
                    src,
                    file,
                    module,
                    parent_symbol.clone(),
                    parent_class.clone(),
                    &qual_prefix,
                    Vec::new(),
                    symbols,
                    imports,
                    calls,
                    assignments,
                );
            }
            "class_definition" => {
                collect_class(
                    child,
                    src,
                    file,
                    module,
                    parent_symbol.clone(),
                    &qual_prefix,
                    Vec::new(),
                    symbols,
                    imports,
                    calls,
                    assignments,
                );
            }
            "decorated_definition" => {
                let decorators = extract_decorators(child, src);
                if let Some(inner) = find_decorated_inner(child) {
                    match inner.kind() {
                        "function_definition" => collect_function(
                            inner,
                            src,
                            file,
                            module,
                            parent_symbol.clone(),
                            parent_class.clone(),
                            &qual_prefix,
                            decorators,
                            symbols,
                            imports,
                            calls,
                            assignments,
                        ),
                        "class_definition" => collect_class(
                            inner,
                            src,
                            file,
                            module,
                            parent_symbol.clone(),
                            &qual_prefix,
                            decorators,
                            symbols,
                            imports,
                            calls,
                            assignments,
                        ),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_function(
    node: Node,
    src: &[u8],
    file: &str,
    module: &str,
    parent_symbol: Option<String>,
    parent_class: Option<String>,
    qual_prefix: &str,
    decorators: Vec<String>,
    symbols: &mut Vec<SymbolRecord>,
    imports: &mut Vec<ImportRecord>,
    calls: &mut HashMap<String, Vec<RawCall>>,
    assignments: &mut HashMap<String, HashMap<String, String>>,
) {
    let name = node_text_field(node, "name", src).unwrap_or_default();
    let local_qual = if qual_prefix.is_empty() {
        name.clone()
    } else {
        format!("{}.{}", qual_prefix, name)
    };
    let body = node.child_by_field_name("body");
    let loc = node.end_position().row + 1 - node.start_position().row;
    let complexity = body.map(|b| compute_complexity(&b)).unwrap_or(1);
    let volume = body
        .map(|b| compute_halstead(&b, src).volume)
        .unwrap_or(0.0);
    let maintainability_index = compute_maintainability_index(volume, complexity, loc.max(1));
    let kind = if parent_symbol.as_ref().is_some_and(|id| {
        symbols
            .iter()
            .any(|s| s.id == *id && s.kind == SymbolKind::Class)
    }) {
        SymbolKind::Method
    } else if parent_symbol.is_some() {
        SymbolKind::NestedFunction
    } else {
        SymbolKind::Function
    };
    let qualified_name = qualify(module, &local_qual);
    let id = symbol_id(file, &node, kind);

    symbols.push(SymbolRecord {
        id: id.clone(),
        kind,
        name: name.clone(),
        qualified_name,
        module: module.to_string(),
        file: file.to_string(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        byte_start: node.start_byte(),
        byte_end: node.end_byte(),
        parent_symbol: parent_symbol.clone(),
        parent_class: parent_class.clone(),
        decorators,
        bases: Vec::new(),
        local_bindings: local_bindings(node, src),
        complexity,
        cognitive_complexity: body.map(|b| compute_cognitive_complexity(&b)).unwrap_or(0),
        max_nesting: body.map(|b| compute_max_nesting(&b)).unwrap_or(0),
        loc,
        maintainability_index,
    });

    if let Some(body) = body {
        calls.insert(id.clone(), extract_calls(body, src));
        assignments.insert(id.clone(), extract_assignments(body, src));
        collect_module_items(
            body,
            src,
            file,
            module,
            Some(id),
            parent_class,
            local_qual,
            symbols,
            imports,
            calls,
            assignments,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_class(
    node: Node,
    src: &[u8],
    file: &str,
    module: &str,
    parent_symbol: Option<String>,
    qual_prefix: &str,
    decorators: Vec<String>,
    symbols: &mut Vec<SymbolRecord>,
    imports: &mut Vec<ImportRecord>,
    calls: &mut HashMap<String, Vec<RawCall>>,
    assignments: &mut HashMap<String, HashMap<String, String>>,
) {
    let name = node_text_field(node, "name", src).unwrap_or_default();
    let local_qual = if qual_prefix.is_empty() {
        name.clone()
    } else {
        format!("{}.{}", qual_prefix, name)
    };
    let id = symbol_id(file, &node, SymbolKind::Class);
    let bases = extract_bases(node, src);

    symbols.push(SymbolRecord {
        id: id.clone(),
        kind: SymbolKind::Class,
        name: name.clone(),
        qualified_name: qualify(module, &local_qual),
        module: module.to_string(),
        file: file.to_string(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        byte_start: node.start_byte(),
        byte_end: node.end_byte(),
        parent_symbol: parent_symbol.clone(),
        parent_class: None,
        decorators,
        bases,
        local_bindings: Vec::new(),
        complexity: 0,
        cognitive_complexity: 0,
        max_nesting: 0,
        loc: node.end_position().row + 1 - node.start_position().row,
        maintainability_index: 100.0,
    });

    if let Some(body) = node.child_by_field_name("body") {
        collect_module_items(
            body,
            src,
            file,
            module,
            Some(id),
            Some(local_qual.clone()),
            local_qual,
            symbols,
            imports,
            calls,
            assignments,
        );
    }
}

fn extract_imports(node: Node, src: &[u8], module: &str) -> Vec<ImportRecord> {
    let mut imports = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "dotted_name" => {
                let source = child.utf8_text(src).unwrap_or("").to_string();
                let local = source.split('.').next().unwrap_or(&source).to_string();
                imports.push(import_record(
                    module,
                    local,
                    source,
                    None,
                    0,
                    child.start_position().row + 1,
                ));
            }
            "aliased_import" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let source = name_node.utf8_text(src).unwrap_or("").to_string();
                    let local = child
                        .child_by_field_name("alias")
                        .map(|n| n.utf8_text(src).unwrap_or("").to_string())
                        .unwrap_or_else(|| source.split('.').next().unwrap_or(&source).to_string());
                    imports.push(import_record(
                        module,
                        local,
                        source,
                        None,
                        0,
                        child.start_position().row + 1,
                    ));
                }
            }
            _ => {}
        }
    }
    imports
}

fn extract_from_imports(node: Node, src: &[u8], module: &str, file: &str) -> Vec<ImportRecord> {
    let text = node.utf8_text(src).unwrap_or("");
    let level = text
        .strip_prefix("from ")
        .map(|rest| rest.chars().take_while(|c| *c == '.').count())
        .unwrap_or(0);
    let source = node
        .child_by_field_name("module_name")
        .map(|n| n.utf8_text(src).unwrap_or("").to_string())
        .unwrap_or_default();
    let package = if Path::new(file)
        .file_name()
        .is_some_and(|n| n == "__init__.py")
    {
        module
    } else {
        module.rsplit_once('.').map_or("", |(package, _)| package)
    };
    let source = resolve_relative_module(package, &source, level);
    if source.is_empty() && level == 0 {
        return Vec::new();
    }

    let module_name_node_id = node.child_by_field_name("module_name").map(|n| n.id());
    let mut imports = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" | "dotted_name" => {
                if Some(child.id()) == module_name_node_id {
                    continue;
                }
                let name = child.utf8_text(src).unwrap_or("").to_string();
                imports.push(import_record(
                    module,
                    name.clone(),
                    source.clone(),
                    Some(name),
                    level,
                    child.start_position().row + 1,
                ));
            }
            "aliased_import" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let original = name_node.utf8_text(src).unwrap_or("").to_string();
                    let local = child
                        .child_by_field_name("alias")
                        .map(|n| n.utf8_text(src).unwrap_or("").to_string())
                        .unwrap_or_else(|| original.clone());
                    imports.push(import_record(
                        module,
                        local,
                        source.clone(),
                        Some(original),
                        level,
                        child.start_position().row + 1,
                    ));
                }
            }
            _ => {}
        }
    }
    imports
}

fn import_record(
    module: &str,
    local_name: String,
    source_module: String,
    imported_name: Option<String>,
    level: usize,
    line: usize,
) -> ImportRecord {
    ImportRecord {
        scope: None,
        module: module.to_string(),
        local_name,
        source_module,
        imported_name,
        level,
        line,
    }
}

fn resolve_relative_module(current: &str, imported: &str, level: usize) -> String {
    if level == 0 {
        return imported.to_string();
    }
    let mut parts: Vec<&str> = current.split('.').filter(|p| !p.is_empty()).collect();
    for _ in 1..level {
        parts.pop();
    }
    if !imported.is_empty() {
        parts.extend(
            imported
                .trim_start_matches('.')
                .split('.')
                .filter(|p| !p.is_empty()),
        );
    }
    parts.join(".")
}

fn extract_decorators(node: Node, src: &[u8]) -> Vec<String> {
    let mut decorators = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "decorator" {
            continue;
        }
        let mut inner_cursor = child.walk();
        for inner in child.children(&mut inner_cursor) {
            match inner.kind() {
                "identifier" | "attribute" => {
                    decorators.push(inner.utf8_text(src).unwrap_or("").to_string())
                }
                "call" => {
                    if let Some(func) = inner.child_by_field_name("function") {
                        decorators.push(func.utf8_text(src).unwrap_or("").to_string());
                    }
                }
                _ => {}
            }
        }
    }
    decorators
}

fn find_decorated_inner<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == "function_definition" || child.kind() == "class_definition")
}

fn extract_bases(node: Node, src: &[u8]) -> Vec<String> {
    let mut bases = Vec::new();
    if let Some(superclasses) = node.child_by_field_name("superclasses") {
        let mut cursor = superclasses.walk();
        for child in superclasses.children(&mut cursor) {
            match child.kind() {
                "identifier" | "attribute" => {
                    bases.push(child.utf8_text(src).unwrap_or("").to_string())
                }
                "call" => {
                    if let Some(func) = child.child_by_field_name("function") {
                        bases.push(func.utf8_text(src).unwrap_or("").to_string());
                    }
                }
                _ => {}
            }
        }
    }
    bases
}

fn extract_calls(node: Node, src: &[u8]) -> Vec<RawCall> {
    let mut calls = Vec::new();
    collect_calls(node, src, &mut calls);
    calls
}

fn collect_calls(node: Node, src: &[u8], calls: &mut Vec<RawCall>) {
    if node.kind() == "function_definition"
        || node.kind() == "class_definition"
        || node.kind() == "lambda"
    {
        return;
    }
    if node.kind() == "call" {
        if let Some(func) = node.child_by_field_name("function") {
            match func.kind() {
                "identifier" => calls.push(RawCall {
                    name: func.utf8_text(src).unwrap_or("").to_string(),
                    receiver: None,
                    line: node.start_position().row + 1,
                }),
                "attribute" => {
                    let object = func.child_by_field_name("object");
                    let attr = func.child_by_field_name("attribute");
                    if let (Some(object), Some(attr)) = (object, attr) {
                        let receiver = object.utf8_text(src).unwrap_or("").to_string();
                        let attr = attr.utf8_text(src).unwrap_or("").to_string();
                        calls.push(RawCall {
                            name: attr,
                            receiver: Some(receiver),
                            line: node.start_position().row + 1,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_calls(child, src, calls);
    }
}

fn local_bindings(function: Node, src: &[u8]) -> Vec<String> {
    fn names(node: Node, src: &[u8], out: &mut Vec<String>) {
        match node.kind() {
            "identifier" => out.push(node.utf8_text(src).unwrap_or("").to_string()),
            "tuple_pattern"
            | "list_pattern"
            | "pattern_list"
            | "list_splat_pattern"
            | "dictionary_splat_pattern" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    names(child, src, out);
                }
            }
            _ => {}
        }
    }
    fn walk(node: Node, src: &[u8], out: &mut Vec<String>) {
        if matches!(
            node.kind(),
            "function_definition" | "class_definition" | "lambda"
        ) {
            return;
        }
        if matches!(
            node.kind(),
            "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause"
        ) {
            if let Some(left) = node.child_by_field_name("left") {
                names(left, src, out);
            }
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            walk(child, src, out);
        }
    }
    let mut out = Vec::new();
    if let Some(params) = function.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for param in params.named_children(&mut cursor) {
            let binding = param
                .child_by_field_name("name")
                .or_else(|| {
                    if matches!(param.kind(), "typed_parameter" | "typed_default_parameter") {
                        param.named_child(0)
                    } else {
                        None
                    }
                })
                .unwrap_or(param);
            names(binding, src, &mut out);
        }
    }
    if let Some(body) = function.child_by_field_name("body") {
        walk(body, src, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn extract_assignments(node: Node, src: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    collect_assignments(node, src, &mut out);
    out
}

fn collect_assignments(node: Node, src: &[u8], out: &mut HashMap<String, String>) {
    if node.kind() == "function_definition"
        || node.kind() == "class_definition"
        || node.kind() == "lambda"
    {
        return;
    }
    if node.kind() == "assignment" {
        let left = node.child_by_field_name("left");
        let right = node.child_by_field_name("right");
        if let (Some(left), Some(right)) = (left, right) {
            if left.kind() == "identifier" && right.kind() == "call" {
                if let Some(func) = right.child_by_field_name("function") {
                    let local = left.utf8_text(src).unwrap_or("").to_string();
                    let class_name = func.utf8_text(src).unwrap_or("").to_string();
                    out.insert(local, class_name);
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_assignments(child, src, out);
    }
}

struct Resolver<'a> {
    symbols: &'a [SymbolRecord],
    module_names: HashSet<String>,
    imports_by_module: HashMap<String, Vec<&'a ImportRecord>>,
    class_by_module_name: HashMap<(String, String), &'a SymbolRecord>,
    methods_by_class: HashMap<String, Vec<&'a SymbolRecord>>,
}

impl<'a> Resolver<'a> {
    fn new(
        modules: &'a [ModuleRecord],
        symbols: &'a [SymbolRecord],
        imports: &'a [ImportRecord],
    ) -> Self {
        let module_names = modules.iter().map(|m| m.name.clone()).collect();
        let mut imports_by_module: HashMap<String, Vec<&ImportRecord>> = HashMap::new();
        for imp in imports {
            imports_by_module
                .entry(imp.module.clone())
                .or_default()
                .push(imp);
        }
        let mut class_by_module_name = HashMap::new();
        let mut methods_by_class: HashMap<String, Vec<&SymbolRecord>> = HashMap::new();
        for symbol in symbols {
            if symbol.kind == SymbolKind::Class {
                let local = symbol
                    .qualified_name
                    .rsplit("::")
                    .next()
                    .unwrap_or(&symbol.name);
                class_by_module_name.insert((symbol.module.clone(), local.to_string()), symbol);
            }
            if symbol.kind == SymbolKind::Method {
                if let Some(class) = &symbol.parent_class {
                    methods_by_class
                        .entry(format!("{}::{}", symbol.module, class))
                        .or_default()
                        .push(symbol);
                }
            }
        }
        Self {
            symbols,
            module_names,
            imports_by_module,
            class_by_module_name,
            methods_by_class,
        }
    }

    fn resolve_call(
        &self,
        caller: &SymbolRecord,
        call: &RawCall,
        assignments: Option<&HashMap<String, String>>,
    ) -> (Option<String>, Confidence, String, String) {
        if let Some(receiver) = &call.receiver {
            if receiver == "self" || receiver == "cls" {
                if let Some(class) = &caller.parent_class {
                    if let Some(target) = self.resolve_method(&caller.module, class, &call.name) {
                        return (
                            Some(target.id.clone()),
                            Confidence::Certain,
                            format!("resolved {}.{} in class hierarchy", receiver, call.name),
                            format!("{}.{}", receiver, call.name),
                        );
                    }
                }
                return (
                    None,
                    Confidence::Unresolved,
                    format!("could not resolve {}.{}", receiver, call.name),
                    format!("{}.{}", receiver, call.name),
                );
            }
            if receiver.starts_with("super(") {
                if let Some(class) = &caller.parent_class {
                    if let Some(target) =
                        self.resolve_super_method(&caller.module, class, &call.name)
                    {
                        return (
                            Some(target.id.clone()),
                            Confidence::Certain,
                            format!("resolved super().{} in base class", call.name),
                            format!("super().{}", call.name),
                        );
                    }
                }
                return (
                    None,
                    Confidence::Unresolved,
                    format!("could not resolve super().{}", call.name),
                    format!("super().{}", call.name),
                );
            }
            if let Some(target) =
                self.resolve_visible_import(caller, &format!("{}.{}", receiver, call.name))
            {
                return (
                    Some(target.id.clone()),
                    Confidence::Certain,
                    "resolved through module import".to_string(),
                    format!("{}.{}", receiver, call.name),
                );
            }
            if let Some(assignments) = assignments {
                if let Some(class_name) = assignments.get(receiver) {
                    if let Some(class) = self.resolve_class_name(&caller.module, class_name) {
                        if let Some(target) =
                            self.resolve_method(&class.module, &class.name, &call.name)
                        {
                            return (
                                Some(target.id.clone()),
                                Confidence::Likely,
                                "resolved through local class construction assignment".to_string(),
                                format!("{}.{}", receiver, call.name),
                            );
                        }
                    }
                }
            }
            if let Some(target) = self.find_method_by_name(&call.name) {
                return (
                    Some(target.id.clone()),
                    Confidence::Possible,
                    "method name matched but receiver type is unknown".to_string(),
                    format!("{}.{}", receiver, call.name),
                );
            }
            return (
                None,
                Confidence::Unresolved,
                "receiver type is unknown".to_string(),
                format!("{}.{}", receiver, call.name),
            );
        }

        if self.is_shadowed(caller, &call.name) {
            return (
                None,
                Confidence::Unresolved,
                "name is bound to a local value or parameter".to_string(),
                call.name.clone(),
            );
        }
        if let Some(target) = self.resolve_scoped(caller, &call.name) {
            return (
                Some(target.id.clone()),
                Confidence::Certain,
                "resolved in lexical scope".to_string(),
                call.name.clone(),
            );
        }
        if let Some(target) = self.resolve_visible_import(caller, &call.name) {
            return (
                Some(target.id.clone()),
                Confidence::Certain,
                "resolved through from-import".to_string(),
                call.name.clone(),
            );
        }
        if let Some(target) = self.resolve_builtin_class_constructor(&caller.module, &call.name) {
            return (
                Some(target.id.clone()),
                Confidence::Certain,
                "resolved class constructor".to_string(),
                call.name.clone(),
            );
        }
        (
            None,
            Confidence::Unresolved,
            "no matching symbol found".to_string(),
            call.name.clone(),
        )
    }

    fn resolve_local(&self, module: &str, name: &str) -> Option<&'a SymbolRecord> {
        let qname = qualify(module, name);
        self.symbols
            .iter()
            .rev()
            .find(|s| s.module == module && s.qualified_name == qname && s.parent_symbol.is_none())
    }

    fn resolve_scoped(&self, caller: &SymbolRecord, name: &str) -> Option<&'a SymbolRecord> {
        let mut scope = Some(caller);
        while let Some(current) = scope {
            // A class namespace is not an enclosing lexical scope for methods.
            if current.kind != SymbolKind::Class {
                if let Some(target) = self.symbols.iter().rev().find(|s| {
                    s.module == caller.module
                        && s.name == name
                        && s.parent_symbol.as_deref() == Some(current.id.as_str())
                }) {
                    return Some(target);
                }
            }
            scope = current
                .parent_symbol
                .as_ref()
                .and_then(|id| self.symbols.iter().find(|s| s.id == *id));
        }
        self.resolve_local(&caller.module, name)
    }

    fn lexical_scopes(&self, caller: &'a SymbolRecord) -> Vec<Option<&'a SymbolRecord>> {
        let mut scopes = Vec::new();
        let mut current = Some(caller);
        while let Some(symbol) = current {
            if symbol.kind != SymbolKind::Class {
                scopes.push(Some(symbol));
            }
            current = symbol
                .parent_symbol
                .as_ref()
                .and_then(|id| self.symbols.iter().find(|s| s.id == *id));
        }
        scopes.push(None);
        scopes
    }

    fn is_shadowed(&self, caller: &'a SymbolRecord, name: &str) -> bool {
        self.lexical_scopes(caller)
            .iter()
            .flatten()
            .any(|s| s.local_bindings.iter().any(|n| n == name))
    }

    fn resolve_visible_import(
        &self,
        caller: &'a SymbolRecord,
        name: &str,
    ) -> Option<&'a SymbolRecord> {
        if self.is_shadowed(caller, name.split('.').next()?) {
            return None;
        }
        let imports = self.imports_by_module.get(&caller.module)?;
        for scope in self.lexical_scopes(caller) {
            for imp in imports
                .iter()
                .rev()
                .filter(|i| i.scope.as_deref() == scope.map(|s| s.id.as_str()))
            {
                if let Some(target) = self.import_target(imp, name) {
                    return Some(target);
                }
            }
        }
        None
    }

    fn import_target(&self, imp: &ImportRecord, name: &str) -> Option<&'a SymbolRecord> {
        let suffix = name.strip_prefix(&imp.local_name)?;
        if !suffix.is_empty() && !suffix.starts_with('.') {
            return None;
        }
        let target = if let Some(imported) = &imp.imported_name {
            format!(
                "{}{}{}{}",
                imp.source_module,
                if imp.source_module.is_empty() {
                    ""
                } else {
                    "."
                },
                imported,
                suffix
            )
        } else {
            if suffix.is_empty() {
                return None;
            } // A module itself is not callable.
            let root = imp.source_module.split('.').next().unwrap_or("");
            if imp.local_name == root && name.starts_with(&format!("{}.", imp.source_module)) {
                name.to_string()
            } else if imp.local_name == root && imp.source_module.contains('.') {
                return None;
            } else {
                format!("{}{}", imp.source_module, suffix)
            }
        };
        // Split at each module boundary; the remainder may name a class method.
        for (index, _) in target.match_indices('.').rev() {
            let module = &target[..index];
            let local = &target[index + 1..];
            if self.module_names.contains(module) {
                return self
                    .symbols
                    .iter()
                    .rev()
                    .find(|s| s.qualified_name == qualify(module, local));
            }
        }
        self.resolve_local("", &target)
    }

    fn resolve_import_bare(&self, module: &str, name: &str) -> Option<&'a SymbolRecord> {
        let imports = self.imports_by_module.get(module)?;
        for imp in imports.iter().rev() {
            if imp.scope.is_some() {
                continue;
            }
            if imp.local_name == name {
                let imported = imp.imported_name.as_deref().unwrap_or(&imp.local_name);
                if let Some(symbol) = self.resolve_local(&imp.source_module, imported) {
                    return Some(symbol);
                }
                let nested_module = if imp.source_module.is_empty() {
                    imported.to_string()
                } else {
                    format!("{}.{}", imp.source_module, imported)
                };
                if self.module_names.contains(&nested_module) {
                    return None;
                }
            }
        }
        None
    }

    fn resolve_class_name(&self, module: &str, name: &str) -> Option<&'a SymbolRecord> {
        if let Some(class) = self
            .class_by_module_name
            .get(&(module.to_string(), name.to_string()))
        {
            return Some(class);
        }
        if let Some(imported) = self.resolve_import_bare(module, name) {
            if imported.kind == SymbolKind::Class {
                return Some(imported);
            }
        }
        None
    }

    fn resolve_builtin_class_constructor(
        &self,
        module: &str,
        name: &str,
    ) -> Option<&'a SymbolRecord> {
        self.resolve_class_name(module, name)
    }

    fn resolve_method(&self, module: &str, class: &str, method: &str) -> Option<&'a SymbolRecord> {
        let mut queue = VecDeque::from([(module.to_string(), class.to_string())]);
        let mut seen = HashSet::new();
        while let Some((module, class)) = queue.pop_front() {
            if !seen.insert((module.clone(), class.clone())) {
                continue;
            }
            if let Some(methods) = self.methods_by_class.get(&format!("{}::{}", module, class)) {
                if let Some(found) = methods.iter().find(|m| m.name == method) {
                    return Some(*found);
                }
            }
            if let Some(class_symbol) = self
                .class_by_module_name
                .get(&(module.clone(), class.clone()))
            {
                for base in &class_symbol.bases {
                    if let Some(base_class) = self.resolve_class_name(&module, base) {
                        queue.push_back((base_class.module.clone(), base_class.name.clone()));
                    }
                }
            }
        }
        None
    }

    fn resolve_super_method(
        &self,
        module: &str,
        class: &str,
        method: &str,
    ) -> Option<&'a SymbolRecord> {
        let class_symbol = self
            .class_by_module_name
            .get(&(module.to_string(), class.to_string()))?;
        for base in &class_symbol.bases {
            if let Some(base_class) = self.resolve_class_name(module, base) {
                if let Some(method) =
                    self.resolve_method(&base_class.module, &base_class.name, method)
                {
                    return Some(method);
                }
            }
        }
        None
    }

    fn find_method_by_name(&self, method: &str) -> Option<&'a SymbolRecord> {
        let mut matches = self
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method && s.name == method);
        let first = matches.next()?;
        if matches.next().is_none() {
            Some(first)
        } else {
            None
        }
    }
}

fn build_edges(
    symbols: &[SymbolRecord],
    imports: &[ImportRecord],
    calls: &HashMap<String, Vec<RawCall>>,
    assignments: &HashMap<String, HashMap<String, String>>,
    resolver: &Resolver<'_>,
    profile: Profile,
) -> Vec<EdgeRecord> {
    let mut edges = Vec::new();

    for imp in imports {
        edges.push(EdgeRecord {
            from: format!("module:{}", imp.module),
            to: None,
            target_name: imp.source_module.clone(),
            kind: EdgeKind::Import,
            confidence: Confidence::Certain,
            reason: "declared import".to_string(),
            line: imp.line,
        });
    }

    for symbol in symbols {
        for base in &symbol.bases {
            let resolved = resolver.resolve_class_name(&symbol.module, base);
            edges.push(EdgeRecord {
                from: symbol.id.clone(),
                to: resolved.map(|s| s.id.clone()),
                target_name: base.clone(),
                kind: EdgeKind::Inheritance,
                confidence: if resolved.is_some() {
                    Confidence::Certain
                } else {
                    Confidence::Unresolved
                },
                reason: if resolved.is_some() {
                    "resolved base class".to_string()
                } else {
                    "base class not found in analyzed project".to_string()
                },
                line: symbol.start_line,
            });
        }

        for decorator in &symbol.decorators {
            let call = RawCall {
                name: decorator.clone(),
                receiver: decorator.split_once('.').map(|(r, _)| r.to_string()),
                line: symbol.start_line,
            };
            let call = if let Some((_, attr)) = decorator.split_once('.') {
                RawCall {
                    name: attr.to_string(),
                    ..call
                }
            } else {
                call
            };
            let (to, confidence, reason, target_name) =
                resolver.resolve_call(symbol, &call, assignments.get(&symbol.id));
            edges.push(EdgeRecord {
                from: symbol.id.clone(),
                to,
                target_name,
                kind: EdgeKind::Decorator,
                confidence,
                reason,
                line: symbol.start_line,
            });
        }

        if profile == Profile::Pytest && is_pytest_entrypoint(symbol) {
            edges.push(EdgeRecord {
                from: "pytest:entrypoint".to_string(),
                to: Some(symbol.id.clone()),
                target_name: symbol.qualified_name.clone(),
                kind: EdgeKind::PytestEntrypoint,
                confidence: Confidence::Certain,
                reason: "pytest test or fixture entrypoint".to_string(),
                line: symbol.start_line,
            });
        }

        for call in calls.get(&symbol.id).into_iter().flatten() {
            let (to, confidence, reason, target_name) =
                resolver.resolve_call(symbol, call, assignments.get(&symbol.id));
            edges.push(EdgeRecord {
                from: symbol.id.clone(),
                to,
                target_name,
                kind: EdgeKind::Call,
                confidence,
                reason,
                line: call.line,
            });
        }
    }

    edges
}

fn compute_metrics(symbols: &[SymbolRecord], edges: &[EdgeRecord]) -> Vec<MetricRecord> {
    let symbol_ids: HashSet<&str> = symbols.iter().map(|s| s.id.as_str()).collect();
    let mut outgoing: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut incoming: HashMap<&str, HashSet<&str>> = HashMap::new();

    for edge in edges.iter().filter(|e| {
        e.kind == EdgeKind::Call
            || e.kind == EdgeKind::Decorator
            || e.kind == EdgeKind::PytestEntrypoint
    }) {
        if edge.confidence > Confidence::Possible {
            continue;
        }
        if let Some(to) = edge.to.as_deref() {
            if symbol_ids.contains(to) {
                outgoing.entry(edge.from.as_str()).or_default().push(to);
                incoming.entry(to).or_default().insert(edge.from.as_str());
            }
        }
    }

    symbols
        .iter()
        .filter(|s| s.kind != SymbolKind::Class)
        .map(|symbol| {
            let mut seen = HashSet::new();
            let mut stack = VecDeque::from([(symbol.id.as_str(), 0usize)]);
            let mut max_depth = 0;
            while let Some((id, depth)) = stack.pop_front() {
                if !seen.insert(id) {
                    continue;
                }
                max_depth = max_depth.max(depth);
                for next in outgoing.get(id).into_iter().flatten() {
                    stack.push_back((*next, depth + 1));
                }
            }
            MetricRecord {
                symbol: symbol.id.clone(),
                fan_in: incoming.get(symbol.id.as_str()).map_or(0, HashSet::len),
                fan_out: outgoing
                    .get(symbol.id.as_str())
                    .map_or(0, |v| v.iter().collect::<HashSet<_>>().len()),
                reachable: seen.len(),
                call_depth: max_depth,
            }
        })
        .collect()
}

fn build_findings(
    symbols: &[SymbolRecord],
    edges: &[EdgeRecord],
    metrics: &[MetricRecord],
    profile: Profile,
) -> Vec<FindingRecord> {
    let metric_by_symbol: HashMap<&str, &MetricRecord> =
        metrics.iter().map(|m| (m.symbol.as_str(), m)).collect();
    let mut findings = Vec::new();

    for symbol in symbols.iter().filter(|s| s.kind != SymbolKind::Class) {
        if symbol.complexity >= 10 || symbol.cognitive_complexity >= 15 || symbol.max_nesting >= 5 {
            findings.push(FindingRecord {
                kind: "hotspot".to_string(),
                severity: if symbol.complexity >= 15 || symbol.cognitive_complexity >= 25 {
                    "warning"
                } else {
                    "info"
                }
                .to_string(),
                symbol: Some(symbol.id.clone()),
                message: format!(
                    "{} has cc={}, cognitive={}, nesting={}",
                    symbol.qualified_name,
                    symbol.complexity,
                    symbol.cognitive_complexity,
                    symbol.max_nesting
                ),
                confidence: Confidence::Certain,
            });
        }

        let fan_in = metric_by_symbol
            .get(symbol.id.as_str())
            .map_or(0, |m| m.fan_in);
        if fan_in == 0
            && !is_known_entrypoint(symbol, profile)
            && !is_dunder(&symbol.name)
            && symbol.name != "main"
        {
            findings.push(FindingRecord {
                kind: "dead-code-candidate".to_string(),
                severity: "info".to_string(),
                symbol: Some(symbol.id.clone()),
                message: format!("{} has no resolved callers", symbol.qualified_name),
                confidence: Confidence::Possible,
            });
        }
    }

    let symbol_by_id: HashMap<&str, &SymbolRecord> =
        symbols.iter().map(|s| (s.id.as_str(), s)).collect();
    for metric in metrics {
        if metric.fan_in + metric.fan_out < 10 {
            continue;
        }
        let Some(symbol) = symbol_by_id.get(metric.symbol.as_str()) else {
            continue;
        };
        let severity = if metric.fan_in >= 10 && metric.fan_out >= 5 {
            "warning"
        } else {
            "info"
        };
        findings.push(FindingRecord {
            kind: "coupling-risk".to_string(),
            severity: severity.to_string(),
            symbol: Some(metric.symbol.clone()),
            message: format!(
                "{} has fan_in={}, fan_out={}",
                symbol.qualified_name, metric.fan_in, metric.fan_out
            ),
            confidence: Confidence::Certain,
        });
    }

    for edge in edges
        .iter()
        .filter(|e| e.confidence == Confidence::Unresolved && e.kind == EdgeKind::Call)
    {
        findings.push(FindingRecord {
            kind: "unresolved-call".to_string(),
            severity: "info".to_string(),
            symbol: Some(edge.from.clone()),
            message: format!("could not resolve call to {}", edge.target_name),
            confidence: Confidence::Unresolved,
        });
    }

    findings
}

pub fn print_map(bundle: &AnalysisBundle, format: OutputFormat, threshold: Confidence) {
    match format {
        OutputFormat::Json => print_json(bundle, threshold),
        OutputFormat::Dot => print_dot(bundle, threshold),
        OutputFormat::Table => {
            println!("PyMapper v2 map");
            println!(
                "Modules: {}  Symbols: {}  Edges: {}",
                bundle.modules.len(),
                bundle.symbols.len(),
                bundle.edges.len()
            );
            for edge in bundle
                .edges
                .iter()
                .filter(|e| threshold.includes(e.confidence))
            {
                println!(
                    "{:<10} {:<10} {} -> {} ({})",
                    edge.kind_label(),
                    edge.confidence.label(),
                    display_node(bundle, &edge.from),
                    edge.to
                        .as_deref()
                        .map(|id| display_node(bundle, id))
                        .unwrap_or_else(|| edge.target_name.clone()),
                    edge.reason
                );
            }
        }
    }
}

pub fn print_scan(bundle: &AnalysisBundle, format: OutputFormat, threshold: Confidence) {
    match format {
        OutputFormat::Json => print_json(bundle, threshold),
        OutputFormat::Dot => print_dot(bundle, threshold),
        OutputFormat::Table => print_scan_table(bundle, threshold),
    }
}

fn print_scan_table(bundle: &AnalysisBundle, threshold: Confidence) {
    let visible: Vec<&FindingRecord> = bundle
        .findings
        .iter()
        .filter(|f| threshold.includes(f.confidence))
        .collect();

    let profile_label = match bundle.project.profile {
        Profile::General => "general",
        Profile::Pytest => "pytest",
    };
    println!(
        "PyMapper v2 scan  (root: {}, profile: {})",
        bundle.project.root, profile_label
    );
    println!(
        "  modules={}  symbols={}  edges={}  findings={}",
        bundle.modules.len(),
        bundle.symbols.len(),
        bundle.edges.len(),
        visible.len(),
    );

    let symbol_by_id: HashMap<&str, &SymbolRecord> =
        bundle.symbols.iter().map(|s| (s.id.as_str(), s)).collect();
    let metric_by_symbol: HashMap<&str, &MetricRecord> = bundle
        .metrics
        .iter()
        .map(|m| (m.symbol.as_str(), m))
        .collect();

    let qname = |finding: &FindingRecord| {
        finding
            .symbol
            .as_deref()
            .map(|id| display_node(bundle, id))
            .unwrap_or_else(|| "-".to_string())
    };
    let sort_findings = |rows: &mut Vec<&FindingRecord>| {
        rows.sort_by(|a, b| {
            severity_rank(&a.severity)
                .cmp(&severity_rank(&b.severity))
                .then_with(|| qname(a).cmp(&qname(b)))
        });
    };

    let mut hotspots: Vec<&FindingRecord> = visible
        .iter()
        .copied()
        .filter(|f| f.kind == "hotspot")
        .collect();
    if !hotspots.is_empty() {
        sort_findings(&mut hotspots);
        println!();
        println!("Hotspots ({})", hotspots.len());
        for finding in hotspots {
            let symbol = finding
                .symbol
                .as_deref()
                .and_then(|id| symbol_by_id.get(id).copied());
            match symbol {
                Some(s) => println!(
                    "  {:<8}  {:<40}  cc={}  cog={}  nest={}",
                    finding.severity,
                    qname(finding),
                    s.complexity,
                    s.cognitive_complexity,
                    s.max_nesting
                ),
                None => println!("  {:<8}  {}", finding.severity, finding.message),
            }
        }
    }

    let mut coupling: Vec<&FindingRecord> = visible
        .iter()
        .copied()
        .filter(|f| f.kind == "coupling-risk")
        .collect();
    if !coupling.is_empty() {
        sort_findings(&mut coupling);
        println!();
        println!("Coupling ({})", coupling.len());
        for finding in coupling {
            let metric = finding
                .symbol
                .as_deref()
                .and_then(|id| metric_by_symbol.get(id).copied());
            match metric {
                Some(m) => println!(
                    "  {:<8}  {:<40}  fan_in={}  fan_out={}",
                    finding.severity,
                    qname(finding),
                    m.fan_in,
                    m.fan_out
                ),
                None => println!("  {:<8}  {}", finding.severity, finding.message),
            }
        }
    }

    let mut dead: Vec<&FindingRecord> = visible
        .iter()
        .copied()
        .filter(|f| f.kind == "dead-code-candidate")
        .collect();
    if !dead.is_empty() {
        sort_findings(&mut dead);
        println!();
        println!("Dead-code candidates ({})", dead.len());
        for finding in dead {
            println!(
                "  {:<8}  {:<40}  no resolved callers",
                finding.severity,
                qname(finding)
            );
        }
    }

    if threshold.includes(Confidence::Unresolved) {
        let mut unresolved: Vec<&EdgeRecord> = bundle
            .edges
            .iter()
            .filter(|e| e.confidence == Confidence::Unresolved && e.kind == EdgeKind::Call)
            .collect();
        if !unresolved.is_empty() {
            unresolved.sort_by(|a, b| {
                let af = display_node(bundle, &a.from);
                let bf = display_node(bundle, &b.from);
                af.cmp(&bf)
                    .then_with(|| a.target_name.cmp(&b.target_name))
                    .then_with(|| a.line.cmp(&b.line))
            });
            println!();
            println!("Unresolved calls ({})", unresolved.len());
            for edge in unresolved {
                println!(
                    "  {:<8}  {}  ->  {}  (line {})",
                    "info",
                    display_node(bundle, &edge.from),
                    edge.target_name,
                    edge.line
                );
            }
        }
    }
}

fn severity_rank(s: &str) -> u8 {
    match s {
        "warning" => 0,
        "info" => 1,
        _ => 2,
    }
}

pub fn print_explain(
    bundle: &AnalysisBundle,
    query: &str,
    format: OutputFormat,
    threshold: Confidence,
) -> Result<(), String> {
    let matches: Vec<&SymbolRecord> = bundle
        .symbols
        .iter()
        .filter(|s| {
            s.id == query
                || s.qualified_name.contains(query)
                || s.file.ends_with(query)
                || s.name == query
        })
        .collect();
    if matches.is_empty() {
        return Err(format!("no symbol matched '{}'", query));
    }
    if format == OutputFormat::Json {
        let ids: HashSet<&str> = matches.iter().map(|s| s.id.as_str()).collect();
        let edges: Vec<&EdgeRecord> = bundle
            .edges
            .iter()
            .filter(|e| threshold.includes(e.confidence))
            .filter(|e| {
                ids.contains(e.from.as_str()) || e.to.as_deref().is_some_and(|id| ids.contains(id))
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "matches": matches, "edges": edges })
        );
        return Ok(());
    }
    if matches.is_empty() {
        return Err(format!("no symbol matched '{}'", query));
    }
    for symbol in matches {
        println!("{} ({:?})", symbol.qualified_name, symbol.kind);
        println!("  id: {}", symbol.id);
        println!(
            "  file: {}:{}-{}",
            symbol.file, symbol.start_line, symbol.end_line
        );
        println!(
            "  metrics: cc={} cognitive={} nesting={} loc={}",
            symbol.complexity, symbol.cognitive_complexity, symbol.max_nesting, symbol.loc
        );
        let outgoing: Vec<&EdgeRecord> = bundle
            .edges
            .iter()
            .filter(|e| e.from == symbol.id && threshold.includes(e.confidence))
            .collect();
        let incoming: Vec<&EdgeRecord> = bundle
            .edges
            .iter()
            .filter(|e| {
                e.to.as_deref() == Some(symbol.id.as_str()) && threshold.includes(e.confidence)
            })
            .collect();
        println!("  outgoing:");
        for edge in outgoing {
            println!(
                "    {:<10} {:<10} -> {} ({})",
                edge.kind_label(),
                edge.confidence.label(),
                edge.to
                    .as_deref()
                    .map(|id| display_node(bundle, id))
                    .unwrap_or_else(|| edge.target_name.clone()),
                edge.reason
            );
        }
        println!("  incoming:");
        for edge in incoming {
            println!(
                "    {:<10} {:<10} <- {} ({})",
                edge.kind_label(),
                edge.confidence.label(),
                display_node(bundle, &edge.from),
                edge.reason
            );
        }
    }
    Ok(())
}

fn print_json(bundle: &AnalysisBundle, threshold: Confidence) {
    let mut visible = bundle.clone();
    visible.edges.retain(|e| threshold.includes(e.confidence));
    visible
        .findings
        .retain(|f| threshold.includes(f.confidence));
    println!("{}", serde_json::to_string_pretty(&visible).unwrap());
}

fn print_dot(bundle: &AnalysisBundle, threshold: Confidence) {
    println!("digraph PyMapperV2 {{");
    println!("  rankdir=LR;");
    println!("  node [shape=box];");
    for symbol in &bundle.symbols {
        println!(
            "  \"{}\" [label=\"{}\"];",
            dot_escape(&symbol.id),
            dot_escape(&symbol.qualified_name)
        );
    }
    for edge in bundle
        .edges
        .iter()
        .filter(|e| threshold.includes(e.confidence))
    {
        if let Some(to) = &edge.to {
            let style = match edge.confidence {
                Confidence::Certain => "solid",
                Confidence::Likely => "dashed",
                Confidence::Possible => "dotted",
                Confidence::Unresolved => "dotted",
            };
            println!(
                "  \"{}\" -> \"{}\" [style={}, label=\"{}:{}\"];",
                dot_escape(&edge.from),
                dot_escape(to),
                style,
                edge.kind_label(),
                edge.confidence.label()
            );
        }
    }
    println!("}}");
}

fn display_node(bundle: &AnalysisBundle, id: &str) -> String {
    bundle
        .symbols
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.qualified_name.clone())
        .unwrap_or_else(|| id.to_string())
}

impl EdgeRecord {
    fn kind_label(&self) -> &'static str {
        match self.kind {
            EdgeKind::Call => "call",
            EdgeKind::Decorator => "decorator",
            EdgeKind::Import => "import",
            EdgeKind::Inheritance => "inherits",
            EdgeKind::PytestEntrypoint => "pytest",
        }
    }
}

fn is_pytest_entrypoint(symbol: &SymbolRecord) -> bool {
    symbol.name.starts_with("test_")
        || symbol
            .decorators
            .iter()
            .any(|d| d == "pytest.fixture" || d == "fixture" || d.ends_with(".fixture"))
}

fn is_known_entrypoint(symbol: &SymbolRecord, profile: Profile) -> bool {
    profile == Profile::Pytest && is_pytest_entrypoint(symbol)
}

fn is_dunder(name: &str) -> bool {
    name.starts_with("__") && name.ends_with("__")
}

fn qualify(module: &str, local: &str) -> String {
    if module.is_empty() {
        local.to_string()
    } else {
        format!("{}::{}", module, local)
    }
}

fn symbol_id(file: &str, node: &Node, kind: SymbolKind) -> String {
    format!(
        "{}:{}-{}:{:?}",
        file,
        node.start_byte(),
        node.end_byte(),
        kind
    )
}

fn node_text_field(node: Node, field: &str, src: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .map(|n| n.utf8_text(src).unwrap_or("").to_string())
}

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn parse_format(format: &str) -> Option<OutputFormat> {
    match format {
        "table" => Some(OutputFormat::Table),
        "json" => Some(OutputFormat::Json),
        "dot" => Some(OutputFormat::Dot),
        _ => None,
    }
}

pub fn parse_profile(profile: &str) -> Option<Profile> {
    match profile {
        "general" => Some(Profile::General),
        "pytest" => Some(Profile::Pytest),
        _ => None,
    }
}

pub fn parse_confidence(confidence: &str) -> Option<Confidence> {
    match confidence {
        "certain" => Some(Confidence::Certain),
        "likely" => Some(Confidence::Likely),
        "possible" => Some(Confidence::Possible),
        "unresolved" => Some(Confidence::Unresolved),
        _ => None,
    }
}
