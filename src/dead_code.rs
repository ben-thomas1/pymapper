use std::collections::HashMap;

use serde::Serialize;

use crate::metrics::GraphMetrics;
use crate::types::{ClassInfo, FunctionInfo, OutputFormat, digit_width};

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum DeadCodeClass {
    TrulyDead,
    EntryPoint,
    Override,
    Inherited,
    TestFixture,
}

impl DeadCodeClass {
    fn label(self) -> &'static str {
        match self {
            Self::TrulyDead => "candidate",
            Self::EntryPoint => "entry-point",
            Self::Override => "override",
            Self::Inherited => "inherited",
            Self::TestFixture => "test-fixture",
        }
    }
}

/// Options for filtering/classifying dead code.
#[derive(Default)]
pub struct DeadCodeOptions {
    pub exclude_decorated: Vec<String>,
    pub exclude_overrides: bool,
    pub classify: bool,
    pub profile: Vec<String>,
}

/// A dead code result with optional classification.
pub struct DeadCodeEntry<'a> {
    pub func: &'a FunctionInfo,
    pub classification: DeadCodeClass,
}

#[derive(Serialize)]
struct DeadCodeRow {
    #[serde(rename = "Function")]
    name: String,
    #[serde(rename = "LOC")]
    loc: usize,
    #[serde(rename = "Lines")]
    lines: String,
    #[serde(rename = "Class", skip_serializing_if = "Option::is_none")]
    classification: Option<String>,
}

#[derive(Serialize)]
pub struct DeadCodeSummaryRow {
    #[serde(rename = "Module")]
    module: String,
    #[serde(rename = "Dead")]
    dead_count: usize,
    #[serde(rename = "Dead LOC")]
    dead_loc: usize,
    #[serde(rename = "By Class", skip_serializing_if = "Option::is_none")]
    by_class: Option<HashMap<String, usize>>,
}

const CLI_DECORATORS: &[&str] = &[
    "click.command",
    "click.group",
    "typer.command",
    "app.command",
    "cli.command",
    "main.command",
];

const WEB_DECORATORS: &[&str] = &[
    "app.route",
    "app.get",
    "app.post",
    "app.put",
    "app.delete",
    "app.patch",
    "router.get",
    "router.post",
    "router.put",
    "router.delete",
    "router.patch",
    "api_view",
    "action",
    "app.websocket",
];

const PYDANTIC_DECORATORS: &[&str] = &[
    "validator",
    "field_validator",
    "root_validator",
    "serializer",
    "field_serializer",
    "model_validator",
];

const TEST_DECORATORS: &[&str] = &["pytest.fixture", "fixture", "pytest.mark.parametrize"];

fn profile_patterns(profile: &str) -> &'static [&'static str] {
    match profile {
        "cli" => CLI_DECORATORS,
        "web" => WEB_DECORATORS,
        "pydantic" => PYDANTIC_DECORATORS,
        _ => &[],
    }
}

fn classify_function(
    func: &FunctionInfo,
    class_map: &HashMap<&str, &ClassInfo>,
    subclass_map: &HashMap<&str, Vec<&ClassInfo>>,
    exclude_patterns: &[String],
) -> DeadCodeClass {
    // Check test fixtures
    if func.name.starts_with("test_") {
        return DeadCodeClass::TestFixture;
    }
    for dec in &func.decorators {
        for test_dec in TEST_DECORATORS {
            if dec == test_dec || dec.ends_with(test_dec) {
                return DeadCodeClass::TestFixture;
            }
        }
    }

    // Check entry point decorators
    let entry_point_sets: &[&[&str]] = &[CLI_DECORATORS, WEB_DECORATORS, PYDANTIC_DECORATORS];
    for dec in &func.decorators {
        for set in entry_point_sets {
            for pattern in *set {
                if dec == pattern || dec.ends_with(pattern) {
                    return DeadCodeClass::EntryPoint;
                }
            }
        }
        // Check user-provided patterns
        for pattern in exclude_patterns {
            if dec == pattern || dec.ends_with(pattern) {
                return DeadCodeClass::EntryPoint;
            }
        }
    }

    // Check overrides — method name exists in a base class
    if let Some(ref parent) = func.parent_class {
        if let Some(class) = class_map.get(parent.as_str()) {
            for base_name in &class.base_classes {
                if let Some(base_class) = class_map.get(base_name.as_str()) {
                    if base_class.method_names.contains(&func.name) {
                        return DeadCodeClass::Override;
                    }
                }
            }
        }

        // Check inherited — a subclass inherits this method without overriding
        if let Some(subclasses) = subclass_map.get(parent.as_str()) {
            for sub in subclasses {
                if !sub.method_names.contains(&func.name) {
                    return DeadCodeClass::Inherited;
                }
            }
        }
    }

    DeadCodeClass::TrulyDead
}

/// Returns functions with zero callers that aren't dunder methods, excluding inherited methods.
pub fn find_dead_code<'a>(
    functions: &'a [FunctionInfo],
    classes: &[ClassInfo],
    metrics: &HashMap<String, GraphMetrics>,
) -> Vec<&'a FunctionInfo> {
    let class_map: HashMap<&str, &ClassInfo> =
        classes.iter().map(|c| (c.name.as_str(), c)).collect();
    let mut subclass_map: HashMap<&str, Vec<&ClassInfo>> = HashMap::new();
    for class in classes {
        for base_name in &class.base_classes {
            subclass_map
                .entry(base_name.as_str())
                .or_default()
                .push(class);
        }
    }

    functions
        .iter()
        .filter(|f| {
            let qname = f.qualified_name();
            let fan_in = metrics.get(&qname).map_or(0, |m| m.fan_in);
            let is_dunder = f.name.starts_with("__") && f.name.ends_with("__");
            let is_main = f.name == "main";
            if fan_in != 0 || is_dunder || is_main {
                return false;
            }
            let class = classify_function(f, &class_map, &subclass_map, &[]);
            class != DeadCodeClass::Inherited
        })
        .collect()
}

/// Returns classified dead code entries with optional filtering.
pub fn find_dead_code_classified<'a>(
    functions: &'a [FunctionInfo],
    classes: &[ClassInfo],
    metrics: &HashMap<String, GraphMetrics>,
    options: &DeadCodeOptions,
) -> Vec<DeadCodeEntry<'a>> {
    // Build combined exclude patterns from profiles + explicit patterns
    let mut exclude_patterns: Vec<String> = options.exclude_decorated.clone();
    for profile in &options.profile {
        for pattern in profile_patterns(profile) {
            exclude_patterns.push((*pattern).to_string());
        }
    }

    let class_map: HashMap<&str, &ClassInfo> =
        classes.iter().map(|c| (c.name.as_str(), c)).collect();

    // Build reverse map: base_class_name → [subclasses that extend it]
    let mut subclass_map: HashMap<&str, Vec<&ClassInfo>> = HashMap::new();
    for class in classes {
        for base_name in &class.base_classes {
            subclass_map
                .entry(base_name.as_str())
                .or_default()
                .push(class);
        }
    }

    let dead = find_dead_code(functions, classes, metrics);

    let mut entries: Vec<DeadCodeEntry> = dead
        .into_iter()
        .map(|func| {
            let classification =
                classify_function(func, &class_map, &subclass_map, &exclude_patterns);
            DeadCodeEntry {
                func,
                classification,
            }
        })
        .collect();

    // Always exclude inherited methods (known false positives from unresolved attribute calls)
    entries.retain(|e| e.classification != DeadCodeClass::Inherited);

    // If not classifying, filter out non-dead entries based on options
    if !options.classify {
        entries.retain(|e| {
            if options.exclude_overrides && e.classification == DeadCodeClass::Override {
                return false;
            }
            // If any exclude patterns or profiles matched, filter entry points
            if (!exclude_patterns.is_empty()) && e.classification == DeadCodeClass::EntryPoint {
                return false;
            }
            // Filter test fixtures when profiles are active
            if !options.profile.is_empty() && e.classification == DeadCodeClass::TestFixture {
                return false;
            }
            true
        });
    }

    entries
}

/// Prints dead code results.
pub fn print_dead_code(dead: &[&FunctionInfo], format: OutputFormat) {
    let rows: Vec<DeadCodeRow> = dead
        .iter()
        .map(|f| DeadCodeRow {
            name: f.qualified_name(),
            loc: f.line_count,
            lines: format!("{}-{}", f.start_line, f.end_line),
            classification: None,
        })
        .collect();

    print_dead_code_rows(&rows, format);
}

/// Prints classified dead code results.
pub fn print_dead_code_classified(entries: &[DeadCodeEntry], format: OutputFormat) {
    let rows: Vec<DeadCodeRow> = entries
        .iter()
        .map(|e| DeadCodeRow {
            name: e.func.qualified_name(),
            loc: e.func.line_count,
            lines: format!("{}-{}", e.func.start_line, e.func.end_line),
            classification: Some(e.classification.label().to_string()),
        })
        .collect();

    print_dead_code_rows(&rows, format);
}

/// Builds module-level dead code summary from plain dead functions.
pub fn build_dead_code_summary(dead: &[&FunctionInfo]) -> Vec<DeadCodeSummaryRow> {
    let mut by_module: HashMap<String, (usize, usize)> = HashMap::new();
    for f in dead {
        let entry = by_module.entry(f.source_module.clone()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += f.line_count;
    }
    let mut rows: Vec<DeadCodeSummaryRow> = by_module
        .into_iter()
        .map(|(module, (count, loc))| DeadCodeSummaryRow {
            module,
            dead_count: count,
            dead_loc: loc,
            by_class: None,
        })
        .collect();
    rows.sort_by(|a, b| b.dead_count.cmp(&a.dead_count));
    rows
}

/// Builds module-level dead code summary with per-classification breakdown.
pub fn build_dead_code_summary_classified(entries: &[DeadCodeEntry]) -> Vec<DeadCodeSummaryRow> {
    let mut by_module: HashMap<String, (usize, usize, HashMap<String, usize>)> = HashMap::new();
    for e in entries {
        let entry = by_module
            .entry(e.func.source_module.clone())
            .or_insert_with(|| (0, 0, HashMap::new()));
        entry.0 += 1;
        entry.1 += e.func.line_count;
        *entry
            .2
            .entry(e.classification.label().to_string())
            .or_insert(0) += 1;
    }
    let mut rows: Vec<DeadCodeSummaryRow> = by_module
        .into_iter()
        .map(|(module, (count, loc, classes))| DeadCodeSummaryRow {
            module,
            dead_count: count,
            dead_loc: loc,
            by_class: Some(classes),
        })
        .collect();
    rows.sort_by(|a, b| b.dead_count.cmp(&a.dead_count));
    rows
}

/// Prints module-level dead code summary.
pub fn print_dead_code_summary(rows: &[DeadCodeSummaryRow], format: OutputFormat) {
    let has_class = rows.iter().any(|r| r.by_class.is_some());

    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&rows).unwrap()),
        OutputFormat::Csv => {
            if has_class {
                println!("Module,Dead,\"Dead LOC\",\"By Class\"");
                for r in rows {
                    let class_str = r
                        .by_class
                        .as_ref()
                        .map(|m| {
                            m.iter()
                                .map(|(k, v)| format!("{}={}", k, v))
                                .collect::<Vec<_>>()
                                .join(";")
                        })
                        .unwrap_or_default();
                    println!(
                        "\"{}\",{},{},\"{}\"",
                        r.module, r.dead_count, r.dead_loc, class_str
                    );
                }
            } else {
                println!("Module,Dead,\"Dead LOC\"");
                for r in rows {
                    println!("\"{}\",{},{}", r.module, r.dead_count, r.dead_loc);
                }
            }
        }
        OutputFormat::Markdown => {
            if rows.is_empty() {
                println!("No dead code found.");
                return;
            }
            if has_class {
                println!("| Module | Dead | Dead LOC | By Class |");
                println!("|--------|------|----------|----------|");
                for r in rows {
                    let class_str = r
                        .by_class
                        .as_ref()
                        .map(|m| {
                            m.iter()
                                .map(|(k, v)| format!("{}={}", k, v))
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    println!(
                        "| {} | {} | {} | {} |",
                        r.module, r.dead_count, r.dead_loc, class_str
                    );
                }
            } else {
                println!("| Module | Dead | Dead LOC |");
                println!("|--------|------|----------|");
                for r in rows {
                    println!("| {} | {} | {} |", r.module, r.dead_count, r.dead_loc);
                }
            }
            let total_dead: usize = rows.iter().map(|r| r.dead_count).sum();
            let total_loc: usize = rows.iter().map(|r| r.dead_loc).sum();
            println!(
                "\n{} module(s), {} dead function(s), {} dead LOC",
                rows.len(),
                total_dead,
                total_loc
            );
        }
        OutputFormat::Table => {
            if rows.is_empty() {
                println!("No dead code found.");
                return;
            }
            if has_class {
                let headers = ["Module", "Dead", "Dead LOC", "By Class"];
                let class_strs: Vec<String> = rows
                    .iter()
                    .map(|r| {
                        r.by_class
                            .as_ref()
                            .map(|m| {
                                m.iter()
                                    .map(|(k, v)| format!("{}={}", k, v))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default()
                    })
                    .collect();
                let widths = [
                    rows.iter()
                        .map(|r| r.module.len())
                        .max()
                        .unwrap()
                        .max(headers[0].len()),
                    rows.iter()
                        .map(|r| digit_width(r.dead_count))
                        .max()
                        .unwrap()
                        .max(headers[1].len()),
                    rows.iter()
                        .map(|r| digit_width(r.dead_loc))
                        .max()
                        .unwrap()
                        .max(headers[2].len()),
                    class_strs
                        .iter()
                        .map(|s| s.len())
                        .max()
                        .unwrap_or(0)
                        .max(headers[3].len()),
                ];
                println!(
                    "{:<w0$}  {:>w1$}  {:>w2$}  {:<w3$}",
                    headers[0],
                    headers[1],
                    headers[2],
                    headers[3],
                    w0 = widths[0],
                    w1 = widths[1],
                    w2 = widths[2],
                    w3 = widths[3],
                );
                let sep: String = widths
                    .iter()
                    .map(|&w| "-".repeat(w))
                    .collect::<Vec<_>>()
                    .join("  ");
                println!("{}", sep);
                for (r, cs) in rows.iter().zip(class_strs.iter()) {
                    println!(
                        "{:<w0$}  {:>w1$}  {:>w2$}  {:<w3$}",
                        r.module,
                        r.dead_count,
                        r.dead_loc,
                        cs,
                        w0 = widths[0],
                        w1 = widths[1],
                        w2 = widths[2],
                        w3 = widths[3],
                    );
                }
            } else {
                let headers = ["Module", "Dead", "Dead LOC"];
                let widths = [
                    rows.iter()
                        .map(|r| r.module.len())
                        .max()
                        .unwrap()
                        .max(headers[0].len()),
                    rows.iter()
                        .map(|r| digit_width(r.dead_count))
                        .max()
                        .unwrap()
                        .max(headers[1].len()),
                    rows.iter()
                        .map(|r| digit_width(r.dead_loc))
                        .max()
                        .unwrap()
                        .max(headers[2].len()),
                ];
                println!(
                    "{:<w0$}  {:>w1$}  {:>w2$}",
                    headers[0],
                    headers[1],
                    headers[2],
                    w0 = widths[0],
                    w1 = widths[1],
                    w2 = widths[2],
                );
                let sep: String = widths
                    .iter()
                    .map(|&w| "-".repeat(w))
                    .collect::<Vec<_>>()
                    .join("  ");
                println!("{}", sep);
                for r in rows {
                    println!(
                        "{:<w0$}  {:>w1$}  {:>w2$}",
                        r.module,
                        r.dead_count,
                        r.dead_loc,
                        w0 = widths[0],
                        w1 = widths[1],
                        w2 = widths[2],
                    );
                }
            }
            let total_dead: usize = rows.iter().map(|r| r.dead_count).sum();
            let total_loc: usize = rows.iter().map(|r| r.dead_loc).sum();
            println!(
                "\n{} module(s), {} dead function(s), {} dead LOC",
                rows.len(),
                total_dead,
                total_loc
            );
        }
    }
}

fn print_dead_code_rows(rows: &[DeadCodeRow], format: OutputFormat) {
    let has_class = rows.iter().any(|r| r.classification.is_some());

    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&rows).unwrap()),
        OutputFormat::Csv => {
            if has_class {
                println!("\"Function\",LOC,Lines,Class");
            } else {
                println!("\"Function\",LOC,Lines");
            }
            for r in rows {
                if has_class {
                    println!(
                        "\"{}\",{},{},{}",
                        r.name,
                        r.loc,
                        r.lines,
                        r.classification.as_deref().unwrap_or("")
                    );
                } else {
                    println!("\"{}\",{},{}", r.name, r.loc, r.lines);
                }
            }
        }
        OutputFormat::Markdown => {
            if has_class {
                println!("| Function | LOC | Lines | Class |");
                println!("|----------|-----|-------|-------|");
                for r in rows {
                    println!(
                        "| {} | {} | {} | {} |",
                        r.name,
                        r.loc,
                        r.lines,
                        r.classification.as_deref().unwrap_or("")
                    );
                }
            } else {
                println!("| Function | LOC | Lines |");
                println!("|----------|-----|-------|");
                for r in rows {
                    println!("| {} | {} | {} |", r.name, r.loc, r.lines);
                }
            }
            println!("\n{} dead function(s) found", rows.len());
        }
        OutputFormat::Table => {
            if rows.is_empty() {
                println!("No dead code found.");
                return;
            }
            if has_class {
                let headers = ["Function", "LOC", "Lines", "Class"];
                let widths = [
                    rows.iter()
                        .map(|r| r.name.len())
                        .max()
                        .unwrap()
                        .max(headers[0].len()),
                    rows.iter()
                        .map(|r| digit_width(r.loc))
                        .max()
                        .unwrap()
                        .max(headers[1].len()),
                    rows.iter()
                        .map(|r| r.lines.len())
                        .max()
                        .unwrap()
                        .max(headers[2].len()),
                    rows.iter()
                        .map(|r| r.classification.as_ref().map_or(0, |c| c.len()))
                        .max()
                        .unwrap()
                        .max(headers[3].len()),
                ];
                println!(
                    "{:<w0$}  {:>w1$}  {:>w2$}  {:<w3$}",
                    headers[0],
                    headers[1],
                    headers[2],
                    headers[3],
                    w0 = widths[0],
                    w1 = widths[1],
                    w2 = widths[2],
                    w3 = widths[3],
                );
                let sep: String = widths
                    .iter()
                    .map(|&w| "-".repeat(w))
                    .collect::<Vec<_>>()
                    .join("  ");
                println!("{}", sep);
                for r in rows {
                    println!(
                        "{:<w0$}  {:>w1$}  {:>w2$}  {:<w3$}",
                        r.name,
                        r.loc,
                        r.lines,
                        r.classification.as_deref().unwrap_or(""),
                        w0 = widths[0],
                        w1 = widths[1],
                        w2 = widths[2],
                        w3 = widths[3],
                    );
                }
            } else {
                let headers = ["Function", "LOC", "Lines"];
                let widths = [
                    rows.iter()
                        .map(|r| r.name.len())
                        .max()
                        .unwrap()
                        .max(headers[0].len()),
                    rows.iter()
                        .map(|r| digit_width(r.loc))
                        .max()
                        .unwrap()
                        .max(headers[1].len()),
                    rows.iter()
                        .map(|r| r.lines.len())
                        .max()
                        .unwrap()
                        .max(headers[2].len()),
                ];
                println!(
                    "{:<w0$}  {:>w1$}  {:>w2$}",
                    headers[0],
                    headers[1],
                    headers[2],
                    w0 = widths[0],
                    w1 = widths[1],
                    w2 = widths[2],
                );
                let sep: String = widths
                    .iter()
                    .map(|&w| "-".repeat(w))
                    .collect::<Vec<_>>()
                    .join("  ");
                println!("{}", sep);
                for r in rows {
                    println!(
                        "{:<w0$}  {:>w1$}  {:>w2$}",
                        r.name,
                        r.loc,
                        r.lines,
                        w0 = widths[0],
                        w1 = widths[1],
                        w2 = widths[2],
                    );
                }
            }
            println!("\n{} dead function(s) found", rows.len());
        }
    }
}
