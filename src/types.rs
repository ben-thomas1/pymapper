#[derive(Debug, Clone)]
pub struct ImportEntry {
    pub local_name: String,
    pub source_module: String,
    pub imported_name: Option<String>,
    pub owning_module: String,
}

#[derive(Debug, Clone)]
pub struct CallTarget {
    pub name: String,
    pub is_self: bool,
}

#[derive(Debug, Clone)]
pub struct FunctionInfo {
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
    pub complexity: usize,
    pub cognitive_complexity: usize,
    pub max_nesting: usize,
    pub line_count: usize,
    pub param_count: usize,
    pub parent_class: Option<String>,
    pub return_count: usize,
    pub bool_expr_count: usize,
    #[allow(dead_code)]
    pub halstead_volume: f64,
    pub maintainability_index: f64,
    pub calls: Vec<CallTarget>,
    pub decorators: Vec<String>,
    pub source_module: String,
    pub source_file: String,
}

impl FunctionInfo {
    /// Returns the qualified name (e.g. "module::MyClass.method" or "module::func").
    pub fn qualified_name(&self) -> String {
        let local = match &self.parent_class {
            Some(class) => format!("{}.{}", class, self.name),
            None => self.name.clone(),
        };
        if self.source_module.is_empty() {
            local
        } else {
            format!("{}::{}", self.source_module, local)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub name: String,
    #[allow(dead_code)]
    pub start_line: usize,
    #[allow(dead_code)]
    pub end_line: usize,
    pub base_classes: Vec<String>,
    pub method_names: Vec<String>,
    pub source_module: String,
}

#[derive(Debug, Default)]
pub struct ModuleInfo {
    pub module_prefix: String,
    pub functions: Vec<FunctionInfo>,
    pub classes: Vec<ClassInfo>,
    pub imports: Vec<ImportEntry>,
}

impl ModuleInfo {
    /// Merges another module's functions, classes, and imports into this one.
    pub fn merge(&mut self, other: ModuleInfo) {
        self.functions.extend(other.functions);
        self.classes.extend(other.classes);
        self.imports.extend(other.imports);
    }
}

/// Returns the number of decimal digits needed to display `n`.
pub fn digit_width(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    ((n as f64).log10().floor() as usize) + 1
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
    Csv,
    Markdown,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Splines {
    Ortho,
    Polyline,
    Curved,
    Spline,
    Line,
}

impl Splines {
    /// Returns the DOT attribute value.
    pub fn as_dot_str(self) -> &'static str {
        match self {
            Splines::Ortho => "ortho",
            Splines::Polyline => "polyline",
            Splines::Curved => "curved",
            Splines::Spline => "spline",
            Splines::Line => "line",
        }
    }
}
