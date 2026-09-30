use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::metrics::GraphMetrics;
use crate::types::{ModuleInfo, OutputFormat, digit_width};

#[derive(Serialize, Deserialize)]
pub struct ReportRow {
    #[serde(rename = "Function")]
    pub name: String,
    #[serde(rename = "McC")]
    pub cc: usize,
    #[serde(rename = "Cognitive")]
    pub cognitive: usize,
    #[serde(rename = "Nesting")]
    pub nesting: usize,
    #[serde(rename = "LOC")]
    pub loc: usize,
    #[serde(rename = "Params")]
    pub params: usize,
    #[serde(rename = "FI")]
    pub fan_in: usize,
    #[serde(rename = "FO")]
    pub fan_out: usize,
    #[serde(rename = "RFC")]
    pub rfc: usize,
    #[serde(rename = "Depth")]
    pub depth: usize,
    #[serde(rename = "T-LOC")]
    pub t_loc: usize,
    #[serde(rename = "T-McC")]
    pub t_cc: usize,
    #[serde(rename = "MI")]
    pub mi: f64,
    #[serde(rename = "Returns")]
    pub returns: usize,
    #[serde(rename = "Bool-Expr")]
    pub bool_expr: usize,
    #[serde(rename = "Lines")]
    pub lines: String,
}

pub struct Thresholds {
    pub cc: Option<usize>,
    pub cognitive: Option<usize>,
    pub nesting: Option<usize>,
    pub loc: Option<usize>,
    pub params: Option<usize>,
}

pub struct Violation {
    pub func_name: String,
    pub metric_name: String,
    pub actual: usize,
    pub limit: usize,
}

/// Parses a threshold string like "cc=10,cognitive=15,nesting=5".
pub fn parse_thresholds(s: &str) -> Result<Thresholds, String> {
    let mut t = Thresholds {
        cc: None,
        cognitive: None,
        nesting: None,
        loc: None,
        params: None,
    };
    for part in s.split(',') {
        let (key, val) = part
            .trim()
            .split_once('=')
            .ok_or_else(|| format!("invalid threshold: {}", part))?;
        let value = val
            .trim()
            .parse::<usize>()
            .map_err(|_| format!("invalid threshold value: {}", val))?;
        let slot = match key.trim() {
            "cc" => &mut t.cc,
            "cognitive" => &mut t.cognitive,
            "nesting" => &mut t.nesting,
            "loc" => &mut t.loc,
            "params" => &mut t.params,
            _ => return Err(format!("unknown threshold key: {}", key)),
        };
        if slot.replace(value).is_some() {
            return Err(format!("duplicate threshold key: {}", key));
        }
    }
    Ok(t)
}

/// Builds report rows sorted by McC descending, with optional filtering.
pub fn build_report_rows(
    module: &ModuleInfo,
    metrics: &HashMap<String, GraphMetrics>,
    min_cc: Option<usize>,
    top: Option<usize>,
) -> (Vec<ReportRow>, usize) {
    build_report_rows_inner(module, metrics, min_cc, top, false)
}

/// Builds report rows, optionally skipping the default McC sort.
pub fn build_report_rows_unsorted(
    module: &ModuleInfo,
    metrics: &HashMap<String, GraphMetrics>,
    min_cc: Option<usize>,
    top: Option<usize>,
) -> (Vec<ReportRow>, usize) {
    build_report_rows_inner(module, metrics, min_cc, top, true)
}

fn build_report_rows_inner(
    module: &ModuleInfo,
    metrics: &HashMap<String, GraphMetrics>,
    min_cc: Option<usize>,
    top: Option<usize>,
    skip_sort: bool,
) -> (Vec<ReportRow>, usize) {
    let total_count = module.functions.len();

    let mut rows: Vec<ReportRow> = module
        .functions
        .iter()
        .map(|f| {
            let qname = f.qualified_name();
            let gm = metrics.get(&qname);
            ReportRow {
                name: qname,
                cc: f.complexity,
                cognitive: f.cognitive_complexity,
                nesting: f.max_nesting,
                loc: f.line_count,
                params: f.param_count,
                fan_in: gm.map_or(0, |m| m.fan_in),
                fan_out: gm.map_or(0, |m| m.fan_out),
                rfc: gm.map_or(0, |m| m.rfc),
                depth: gm.map_or(0, |m| m.call_depth),
                t_loc: gm.map_or(0, |m| m.t_loc),
                t_cc: gm.map_or(0, |m| m.t_cc),
                mi: (f.maintainability_index * 100.0).round() / 100.0,
                returns: f.return_count,
                bool_expr: f.bool_expr_count,
                lines: format!("{}-{}", f.start_line, f.end_line),
            }
        })
        .collect();

    if !skip_sort {
        rows.sort_by(|a, b| b.cc.cmp(&a.cc).then(a.name.cmp(&b.name)));
    }

    if let Some(min) = min_cc {
        rows.retain(|r| r.cc >= min);
    }
    if let Some(n) = top {
        rows.truncate(n);
    }

    (rows, total_count)
}

/// Prints a complexity report table sorted by cyclomatic complexity descending.
pub fn print_report(
    module: &ModuleInfo,
    metrics: &HashMap<String, GraphMetrics>,
    min_cc: Option<usize>,
    top: Option<usize>,
    format: OutputFormat,
) {
    let (rows, total_count) = build_report_rows(module, metrics, min_cc, top);
    let filtered = min_cc.is_some() || top.is_some();

    match format {
        OutputFormat::Json => print_report_json(&rows),
        OutputFormat::Csv => print_report_csv(&rows),
        OutputFormat::Table => print_report_table(&rows, total_count, filtered),
        OutputFormat::Markdown => print_report_markdown(&rows, total_count, filtered),
    }
}

/// Prints a report from pre-sorted rows.
pub fn print_sorted_report(
    rows: &[ReportRow],
    total_count: usize,
    filtered: bool,
    format: OutputFormat,
) {
    match format {
        OutputFormat::Json => print_report_json(rows),
        OutputFormat::Csv => print_report_csv(rows),
        OutputFormat::Table => print_report_table(rows, total_count, filtered),
        OutputFormat::Markdown => print_report_markdown(rows, total_count, filtered),
    }
}

fn print_report_json(rows: &[ReportRow]) {
    println!("{}", serde_json::to_string_pretty(rows).unwrap());
}

fn print_report_csv(rows: &[ReportRow]) {
    println!(
        "\"Function\",McC,Cognitive,Nesting,LOC,Params,FI,FO,RFC,Depth,T-LOC,T-McC,MI,Returns,Bool-Expr,Lines"
    );
    for r in rows {
        println!(
            "\"{}\",{},{},{},{},{},{},{},{},{},{},{},{:.1},{},{},{}",
            r.name,
            r.cc,
            r.cognitive,
            r.nesting,
            r.loc,
            r.params,
            r.fan_in,
            r.fan_out,
            r.rfc,
            r.depth,
            r.t_loc,
            r.t_cc,
            r.mi,
            r.returns,
            r.bool_expr,
            r.lines
        );
    }
}

fn print_report_table(rows: &[ReportRow], total_count: usize, filtered: bool) {
    if rows.is_empty() {
        println!("No functions found.");
        return;
    }

    let headers = [
        "Function",
        "McC",
        "Cognitive",
        "Nesting",
        "LOC",
        "Params",
        "FI",
        "FO",
        "RFC",
        "Depth",
        "T-LOC",
        "T-McC",
        "MI",
        "Ret",
        "Bool",
        "Lines",
    ];

    let widths = [
        rows.iter()
            .map(|r| r.name.len())
            .max()
            .unwrap()
            .max(headers[0].len()),
        rows.iter()
            .map(|r| digit_width(r.cc))
            .max()
            .unwrap()
            .max(headers[1].len()),
        rows.iter()
            .map(|r| digit_width(r.cognitive))
            .max()
            .unwrap()
            .max(headers[2].len()),
        rows.iter()
            .map(|r| digit_width(r.nesting))
            .max()
            .unwrap()
            .max(headers[3].len()),
        rows.iter()
            .map(|r| digit_width(r.loc))
            .max()
            .unwrap()
            .max(headers[4].len()),
        rows.iter()
            .map(|r| digit_width(r.params))
            .max()
            .unwrap()
            .max(headers[5].len()),
        rows.iter()
            .map(|r| digit_width(r.fan_in))
            .max()
            .unwrap()
            .max(headers[6].len()),
        rows.iter()
            .map(|r| digit_width(r.fan_out))
            .max()
            .unwrap()
            .max(headers[7].len()),
        rows.iter()
            .map(|r| digit_width(r.rfc))
            .max()
            .unwrap()
            .max(headers[8].len()),
        rows.iter()
            .map(|r| digit_width(r.depth))
            .max()
            .unwrap()
            .max(headers[9].len()),
        rows.iter()
            .map(|r| digit_width(r.t_loc))
            .max()
            .unwrap()
            .max(headers[10].len()),
        rows.iter()
            .map(|r| digit_width(r.t_cc))
            .max()
            .unwrap()
            .max(headers[11].len()),
        rows.iter()
            .map(|r| format!("{:.1}", r.mi).len())
            .max()
            .unwrap()
            .max(headers[12].len()),
        rows.iter()
            .map(|r| digit_width(r.returns))
            .max()
            .unwrap()
            .max(headers[13].len()),
        rows.iter()
            .map(|r| digit_width(r.bool_expr))
            .max()
            .unwrap()
            .max(headers[14].len()),
        rows.iter()
            .map(|r| r.lines.len())
            .max()
            .unwrap()
            .max(headers[15].len()),
    ];

    println!(
        "{:<w0$}  {:>w1$}  {:>w2$}  {:>w3$}  {:>w4$}  {:>w5$}  {:>w6$}  {:>w7$}  {:>w8$}  {:>w9$}  {:>w10$}  {:>w11$}  {:>w12$}  {:>w13$}  {:>w14$}  {:>w15$}",
        headers[0],
        headers[1],
        headers[2],
        headers[3],
        headers[4],
        headers[5],
        headers[6],
        headers[7],
        headers[8],
        headers[9],
        headers[10],
        headers[11],
        headers[12],
        headers[13],
        headers[14],
        headers[15],
        w0 = widths[0],
        w1 = widths[1],
        w2 = widths[2],
        w3 = widths[3],
        w4 = widths[4],
        w5 = widths[5],
        w6 = widths[6],
        w7 = widths[7],
        w8 = widths[8],
        w9 = widths[9],
        w10 = widths[10],
        w11 = widths[11],
        w12 = widths[12],
        w13 = widths[13],
        w14 = widths[14],
        w15 = widths[15],
    );

    let sep: String = widths
        .iter()
        .map(|&w| "-".repeat(w))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{}", sep);

    for r in rows {
        println!(
            "{:<w0$}  {:>w1$}  {:>w2$}  {:>w3$}  {:>w4$}  {:>w5$}  {:>w6$}  {:>w7$}  {:>w8$}  {:>w9$}  {:>w10$}  {:>w11$}  {:>w12$.1}  {:>w13$}  {:>w14$}  {:>w15$}",
            r.name,
            r.cc,
            r.cognitive,
            r.nesting,
            r.loc,
            r.params,
            r.fan_in,
            r.fan_out,
            r.rfc,
            r.depth,
            r.t_loc,
            r.t_cc,
            r.mi,
            r.returns,
            r.bool_expr,
            r.lines,
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
            w4 = widths[4],
            w5 = widths[5],
            w6 = widths[6],
            w7 = widths[7],
            w8 = widths[8],
            w9 = widths[9],
            w10 = widths[10],
            w11 = widths[11],
            w12 = widths[12],
            w13 = widths[13],
            w14 = widths[14],
            w15 = widths[15],
        );
    }

    if filtered {
        println!("\n{} functions shown ({} total)", rows.len(), total_count);
    } else {
        println!("\n{} functions total", rows.len());
    }
}

fn print_report_markdown(rows: &[ReportRow], total_count: usize, filtered: bool) {
    if rows.is_empty() {
        println!("No functions found.");
        return;
    }
    println!(
        "| Function | McC | Cognitive | Nesting | LOC | Params | FI | FO | RFC | Depth | T-LOC | T-McC | MI | Ret | Bool | Lines |"
    );
    println!(
        "|----------|-----|-----------|---------|-----|--------|----|----|-----|-------|-------|-------|----|-----|------|-------|"
    );
    for r in rows {
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {:.1} | {} | {} | {} |",
            r.name,
            r.cc,
            r.cognitive,
            r.nesting,
            r.loc,
            r.params,
            r.fan_in,
            r.fan_out,
            r.rfc,
            r.depth,
            r.t_loc,
            r.t_cc,
            r.mi,
            r.returns,
            r.bool_expr,
            r.lines
        );
    }
    if filtered {
        println!("\n{} functions shown ({} total)", rows.len(), total_count);
    } else {
        println!("\n{} functions total", rows.len());
    }
}

/// Checks rows against thresholds, returns violations.
pub fn check_thresholds(module: &ModuleInfo, t: &Thresholds) -> Vec<Violation> {
    let mut violations = Vec::new();
    for f in &module.functions {
        let qname = f.qualified_name();
        let checks: [(Option<usize>, usize, &str); 5] = [
            (t.cc, f.complexity, "cc"),
            (t.cognitive, f.cognitive_complexity, "cognitive"),
            (t.nesting, f.max_nesting, "nesting"),
            (t.loc, f.line_count, "loc"),
            (t.params, f.param_count, "params"),
        ];
        for (limit, actual, name) in checks {
            if let Some(limit) = limit {
                if actual > limit {
                    violations.push(Violation {
                        func_name: qname.clone(),
                        metric_name: name.to_string(),
                        actual,
                        limit,
                    });
                }
            }
        }
    }
    violations
}

/// Prints violations to stderr and returns true if any exist.
pub fn print_violations(violations: &[Violation]) -> bool {
    if violations.is_empty() {
        return false;
    }

    let name_w = violations
        .iter()
        .map(|v| v.func_name.len())
        .max()
        .unwrap()
        .max(8);
    let metric_w = violations
        .iter()
        .map(|v| v.metric_name.len())
        .max()
        .unwrap()
        .max(6);

    eprintln!(
        "{:<nw$}  {:<mw$}  {:>6}  {:>5}",
        "Function",
        "Metric",
        "Actual",
        "Limit",
        nw = name_w,
        mw = metric_w,
    );
    let sep = format!(
        "{}  {}  {}  {}",
        "-".repeat(name_w),
        "-".repeat(metric_w),
        "-".repeat(6),
        "-".repeat(5),
    );
    eprintln!("{}", sep);
    for v in violations {
        eprintln!(
            "{:<nw$}  {:<mw$}  {:>6}  {:>5}",
            v.func_name,
            v.metric_name,
            v.actual,
            v.limit,
            nw = name_w,
            mw = metric_w,
        );
    }
    eprintln!(
        "\nFAIL: {} violation(s) exceed thresholds",
        violations.len()
    );
    true
}

#[derive(Serialize)]
pub struct ModuleSummaryRow {
    #[serde(rename = "Module")]
    pub module: String,
    #[serde(rename = "Functions")]
    pub func_count: usize,
    #[serde(rename = "Total LOC")]
    pub total_loc: usize,
    #[serde(rename = "Avg McC")]
    pub avg_cc: f64,
    #[serde(rename = "Max McC")]
    pub max_cc: usize,
    #[serde(rename = "Cross-Module Calls")]
    pub cross_module: usize,
    #[serde(rename = "Dead Functions")]
    pub dead_count: usize,
    #[serde(rename = "Ca")]
    pub ca: usize,
    #[serde(rename = "Ce")]
    pub ce: usize,
    #[serde(rename = "Instability")]
    pub instability: f64,
}

struct ModuleAccum {
    locs: Vec<usize>,
    ccs: Vec<usize>,
    max_cc: usize,
    dead_count: usize,
    cross_module: usize,
    efferent_modules: std::collections::HashSet<String>,
}

/// Builds module-level summary rows sorted by total LOC descending.
pub fn build_module_summary(
    module: &ModuleInfo,
    metrics: &HashMap<String, GraphMetrics>,
    top: Option<usize>,
) -> Vec<ModuleSummaryRow> {
    use std::collections::HashSet;

    // Build a map from function qualified name to its source module
    let func_module_map: HashMap<String, String> = module
        .functions
        .iter()
        .map(|f| (f.qualified_name(), f.source_module.clone()))
        .collect();

    // Build import map keyed by owning module
    let mut import_map: HashMap<&str, Vec<&crate::types::ImportEntry>> = HashMap::new();
    for imp in &module.imports {
        import_map
            .entry(imp.owning_module.as_str())
            .or_default()
            .push(imp);
    }

    let mut accum: HashMap<String, ModuleAccum> = HashMap::new();

    for f in &module.functions {
        let m = &f.source_module;
        let entry = accum.entry(m.clone()).or_insert_with(|| ModuleAccum {
            locs: Vec::new(),
            ccs: Vec::new(),
            max_cc: 0,
            dead_count: 0,
            cross_module: 0,
            efferent_modules: HashSet::new(),
        });
        entry.locs.push(f.line_count);
        entry.ccs.push(f.complexity);
        if f.complexity > entry.max_cc {
            entry.max_cc = f.complexity;
        }

        let qname = f.qualified_name();
        let fan_in = metrics.get(&qname).map_or(0, |g| g.fan_in);
        let is_dunder = f.name.starts_with("__") && f.name.ends_with("__");
        if fan_in == 0 && !is_dunder {
            // Exclude inherited methods from dead count
            let is_inherited = if let Some(ref parent) = f.parent_class {
                module.classes.iter().any(|c| {
                    c.base_classes.iter().any(|base| base == parent)
                        && !c.method_names.contains(&f.name)
                })
            } else {
                false
            };
            if !is_inherited {
                entry.dead_count += 1;
            }
        }

        // Track efferent coupling (distinct modules this module calls into)
        for call in &f.calls {
            if call.is_self {
                continue;
            }
            // Try to resolve the call target to a module
            let prefixed = if !m.is_empty() {
                format!("{}::{}", m, call.name)
            } else {
                call.name.clone()
            };
            let target_module = if let Some(tm) = func_module_map.get(&prefixed) {
                Some(tm.clone())
            } else if let Some(tm) = func_module_map.get(&call.name) {
                Some(tm.clone())
            } else if let Some(imports) = import_map.get(m.as_str()) {
                // Try import-based resolution
                let mut resolved = None;
                // Bare-name lookup
                for imp in imports.iter().rev() {
                    if imp.local_name == call.name {
                        let name = imp.imported_name.as_deref().unwrap_or(&imp.local_name);
                        let qualified = format!("{}::{}", imp.source_module, name);
                        if let Some(tm) = func_module_map.get(&qualified) {
                            resolved = Some(tm.clone());
                            break;
                        }
                    }
                }
                // Attribute-call lookup
                if resolved.is_none() {
                    if let Some((prefix, attr)) = call.name.split_once('.') {
                        for imp in imports.iter().rev() {
                            if imp.local_name == prefix && imp.imported_name.is_none() {
                                let qualified = format!("{}::{}", imp.source_module, attr);
                                if let Some(tm) = func_module_map.get(&qualified) {
                                    resolved = Some(tm.clone());
                                    break;
                                }
                            }
                        }
                    }
                }
                resolved
            } else {
                None
            };
            if let Some(tm) = target_module {
                if tm != *m {
                    entry.cross_module += 1;
                    entry.efferent_modules.insert(tm);
                }
            }
        }
    }

    // Import-based Ce: imports referencing other analyzed modules create coupling edges
    let known_modules: HashSet<String> = accum.keys().cloned().collect();
    for imp in &module.imports {
        let owner = &imp.owning_module;
        if let Some(entry) = accum.get_mut(owner) {
            if known_modules.contains(&imp.source_module) && imp.source_module != *owner {
                entry.efferent_modules.insert(imp.source_module.clone());
            } else {
                for km in &known_modules {
                    if km != owner
                        && (km.starts_with(&format!("{}.", imp.source_module))
                            || imp.source_module.starts_with(&format!("{}.", km)))
                    {
                        entry.efferent_modules.insert(km.clone());
                    }
                }
            }
        }
    }

    // Inheritance-based Ce: base class imports create coupling edges
    for class in &module.classes {
        for base_name in &class.base_classes {
            if let Some(imports) = import_map.get(class.source_module.as_str()) {
                for imp in imports {
                    if imp.local_name == *base_name {
                        let source = &imp.source_module;
                        if known_modules.contains(source)
                            && *source != class.source_module
                            && let Some(entry) = accum.get_mut(&class.source_module)
                        {
                            entry.efferent_modules.insert(source.clone());
                        }
                    }
                }
            }
        }
    }

    // Compute afferent coupling: for each module, count how many distinct modules depend on it
    let efferent_map: HashMap<String, HashSet<String>> = accum
        .iter()
        .map(|(m, a)| {
            let key = if m.is_empty() {
                "(root)".to_string()
            } else {
                m.clone()
            };
            (
                key,
                a.efferent_modules
                    .iter()
                    .map(|e| {
                        if e.is_empty() {
                            "(root)".to_string()
                        } else {
                            e.clone()
                        }
                    })
                    .collect(),
            )
        })
        .collect();

    let mut afferent_count: HashMap<String, usize> = HashMap::new();
    for (_source, targets) in &efferent_map {
        for target in targets {
            *afferent_count.entry(target.clone()).or_insert(0) += 1;
        }
    }

    let mut rows: Vec<ModuleSummaryRow> = accum
        .into_iter()
        .map(|(m, a)| {
            let total_loc: usize = a.locs.iter().sum();
            let avg_cc = if a.ccs.is_empty() {
                0.0
            } else {
                a.ccs.iter().sum::<usize>() as f64 / a.ccs.len() as f64
            };
            let display_module = if m.is_empty() {
                "(root)".to_string()
            } else {
                m
            };
            let ce = a.efferent_modules.len();
            let ca = afferent_count.get(&display_module).copied().unwrap_or(0);
            let instability = if ca + ce == 0 {
                0.0
            } else {
                ce as f64 / (ca + ce) as f64
            };
            ModuleSummaryRow {
                module: display_module,
                func_count: a.locs.len(),
                total_loc,
                avg_cc: (avg_cc * 100.0).round() / 100.0,
                max_cc: a.max_cc,
                cross_module: a.cross_module,
                dead_count: a.dead_count,
                ca,
                ce,
                instability: (instability * 100.0).round() / 100.0,
            }
        })
        .collect();

    rows.sort_by(|a, b| b.total_loc.cmp(&a.total_loc));

    if let Some(n) = top {
        rows.truncate(n);
    }

    rows
}

/// Prints a module-level summary.
pub fn print_module_summary(
    module: &ModuleInfo,
    metrics: &HashMap<String, GraphMetrics>,
    top: Option<usize>,
    format: OutputFormat,
) {
    let rows = build_module_summary(module, metrics, top);

    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&rows).unwrap()),
        OutputFormat::Csv => {
            println!(
                "\"Module\",Functions,\"Total LOC\",\"Avg McC\",\"Max McC\",\"Cross-Module Calls\",\"Dead Functions\",Ca,Ce,Instability"
            );
            for r in &rows {
                println!(
                    "\"{}\",{},{},{:.2},{},{},{},{},{},{:.2}",
                    r.module,
                    r.func_count,
                    r.total_loc,
                    r.avg_cc,
                    r.max_cc,
                    r.cross_module,
                    r.dead_count,
                    r.ca,
                    r.ce,
                    r.instability
                );
            }
        }
        OutputFormat::Markdown => {
            if rows.is_empty() {
                println!("No modules found.");
                return;
            }
            println!(
                "| Module | Functions | Total LOC | Avg McC | Max McC | Cross-Module | Dead | Ca | Ce | I |"
            );
            println!(
                "|--------|-----------|-----------|---------|---------|--------------|------|----|----|----|"
            );
            for r in &rows {
                println!(
                    "| {} | {} | {} | {:.2} | {} | {} | {} | {} | {} | {:.2} |",
                    r.module,
                    r.func_count,
                    r.total_loc,
                    r.avg_cc,
                    r.max_cc,
                    r.cross_module,
                    r.dead_count,
                    r.ca,
                    r.ce,
                    r.instability
                );
            }
            println!("\n{} modules total", rows.len());
        }
        OutputFormat::Table => {
            if rows.is_empty() {
                println!("No modules found.");
                return;
            }
            let headers = [
                "Module",
                "Functions",
                "Total LOC",
                "Avg McC",
                "Max McC",
                "Cross-Module",
                "Dead",
                "Ca",
                "Ce",
                "I",
            ];
            let widths = [
                rows.iter()
                    .map(|r| r.module.len())
                    .max()
                    .unwrap()
                    .max(headers[0].len()),
                rows.iter()
                    .map(|r| digit_width(r.func_count))
                    .max()
                    .unwrap()
                    .max(headers[1].len()),
                rows.iter()
                    .map(|r| digit_width(r.total_loc))
                    .max()
                    .unwrap()
                    .max(headers[2].len()),
                rows.iter()
                    .map(|r| format!("{:.2}", r.avg_cc).len())
                    .max()
                    .unwrap()
                    .max(headers[3].len()),
                rows.iter()
                    .map(|r| digit_width(r.max_cc))
                    .max()
                    .unwrap()
                    .max(headers[4].len()),
                rows.iter()
                    .map(|r| digit_width(r.cross_module))
                    .max()
                    .unwrap()
                    .max(headers[5].len()),
                rows.iter()
                    .map(|r| digit_width(r.dead_count))
                    .max()
                    .unwrap()
                    .max(headers[6].len()),
                rows.iter()
                    .map(|r| digit_width(r.ca))
                    .max()
                    .unwrap()
                    .max(headers[7].len()),
                rows.iter()
                    .map(|r| digit_width(r.ce))
                    .max()
                    .unwrap()
                    .max(headers[8].len()),
                rows.iter()
                    .map(|r| format!("{:.2}", r.instability).len())
                    .max()
                    .unwrap()
                    .max(headers[9].len()),
            ];
            println!(
                "{:<w0$}  {:>w1$}  {:>w2$}  {:>w3$}  {:>w4$}  {:>w5$}  {:>w6$}  {:>w7$}  {:>w8$}  {:>w9$}",
                headers[0],
                headers[1],
                headers[2],
                headers[3],
                headers[4],
                headers[5],
                headers[6],
                headers[7],
                headers[8],
                headers[9],
                w0 = widths[0],
                w1 = widths[1],
                w2 = widths[2],
                w3 = widths[3],
                w4 = widths[4],
                w5 = widths[5],
                w6 = widths[6],
                w7 = widths[7],
                w8 = widths[8],
                w9 = widths[9],
            );
            let sep: String = widths
                .iter()
                .map(|&w| "-".repeat(w))
                .collect::<Vec<_>>()
                .join("  ");
            println!("{}", sep);
            for r in &rows {
                println!(
                    "{:<w0$}  {:>w1$}  {:>w2$}  {:>w3$.2}  {:>w4$}  {:>w5$}  {:>w6$}  {:>w7$}  {:>w8$}  {:>w9$.2}",
                    r.module,
                    r.func_count,
                    r.total_loc,
                    r.avg_cc,
                    r.max_cc,
                    r.cross_module,
                    r.dead_count,
                    r.ca,
                    r.ce,
                    r.instability,
                    w0 = widths[0],
                    w1 = widths[1],
                    w2 = widths[2],
                    w3 = widths[3],
                    w4 = widths[4],
                    w5 = widths[5],
                    w6 = widths[6],
                    w7 = widths[7],
                    w8 = widths[8],
                    w9 = widths[9],
                );
            }
            println!("\n{} modules total", rows.len());
        }
    }
}

/// Sorts report rows by the given column name. Returns false if column name is unknown.
pub fn sort_rows_by_column(rows: &mut [ReportRow], col: &str, ascending: bool) -> bool {
    let cmp: fn(&ReportRow, &ReportRow) -> std::cmp::Ordering = match col {
        "name" => |a, b| a.name.cmp(&b.name),
        "cc" => |a, b| a.cc.cmp(&b.cc),
        "cognitive" => |a, b| a.cognitive.cmp(&b.cognitive),
        "nesting" => |a, b| a.nesting.cmp(&b.nesting),
        "loc" => |a, b| a.loc.cmp(&b.loc),
        "params" => |a, b| a.params.cmp(&b.params),
        "fi" => |a, b| a.fan_in.cmp(&b.fan_in),
        "fo" => |a, b| a.fan_out.cmp(&b.fan_out),
        "rfc" => |a, b| a.rfc.cmp(&b.rfc),
        "depth" => |a, b| a.depth.cmp(&b.depth),
        "t-loc" | "tloc" => |a, b| a.t_loc.cmp(&b.t_loc),
        "t-cc" | "tcc" | "t-mcc" | "tmcc" => |a, b| a.t_cc.cmp(&b.t_cc),
        "mi" => |a, b| a.mi.partial_cmp(&b.mi).unwrap_or(std::cmp::Ordering::Equal),
        "returns" | "ret" => |a, b| a.returns.cmp(&b.returns),
        "bool-expr" | "bool" => |a, b| a.bool_expr.cmp(&b.bool_expr),
        _ => return false,
    };
    rows.sort_by(|a, b| {
        let ord = cmp(a, b);
        if ascending { ord } else { ord.reverse() }
    });
    true
}

/// Saves report rows as a JSON baseline file.
pub fn save_baseline(rows: &[ReportRow], path: &str) -> Result<(), String> {
    use std::io::Write;
    let destination = std::path::Path::new(path);
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let write = || -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_vec_pretty(rows)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&json)?;
        temporary.as_file().sync_all()?;
        temporary.persist_noclobber(destination)?;
        Ok(())
    };
    write().map_err(|e| {
        format!(
            "failed to create baseline {} (destination must be new): {}",
            path, e
        )
    })?;
    eprintln!("baseline saved to {}", path);
    Ok(())
}

/// Loads a baseline from a JSON file.
pub fn load_baseline(path: &str) -> Result<Vec<ReportRow>, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read baseline from {}: {}", path, e))?;
    serde_json::from_str(&contents)
        .map_err(|e| format!("failed to parse baseline from {}: {}", path, e))
}

/// Compares current rows against a baseline, printing deltas.
pub fn compare_baseline(
    current: &[ReportRow],
    baseline: &[ReportRow],
    fail_regression: bool,
) -> bool {
    let baseline_map: HashMap<String, &ReportRow> =
        baseline.iter().map(|r| (r.name.clone(), r)).collect();

    let mut regressions = 0usize;

    println!(
        "{:<50}  {:>5}  {:>5}  {:>10}  {:>10}  {:>8}  {:>8}",
        "Function", "McC", "Δ McC", "Cognitive", "Δ Cog", "LOC", "Δ LOC"
    );
    let sep = format!(
        "{}  {}  {}  {}  {}  {}  {}",
        "-".repeat(50),
        "-".repeat(5),
        "-".repeat(5),
        "-".repeat(10),
        "-".repeat(10),
        "-".repeat(8),
        "-".repeat(8)
    );
    println!("{}", sep);

    for r in current {
        if let Some(base) = baseline_map.get(&r.name) {
            let d_cc = r.cc as i64 - base.cc as i64;
            let d_cog = r.cognitive as i64 - base.cognitive as i64;
            let d_loc = r.loc as i64 - base.loc as i64;

            if d_cc != 0 || d_cog != 0 || d_loc != 0 {
                let fmt_delta = |d: i64| -> String {
                    if d > 0 {
                        format!("+{}", d)
                    } else {
                        format!("{}", d)
                    }
                };
                println!(
                    "{:<50}  {:>5}  {:>5}  {:>10}  {:>10}  {:>8}  {:>8}",
                    r.name,
                    r.cc,
                    fmt_delta(d_cc),
                    r.cognitive,
                    fmt_delta(d_cog),
                    r.loc,
                    fmt_delta(d_loc)
                );

                if d_cc > 0 || d_cog > 0 || d_loc > 0 {
                    regressions += 1;
                }
            }
        } else {
            println!(
                "{:<50}  {:>5}  {:>5}  {:>10}  {:>10}  {:>8}  {:>8}",
                r.name, r.cc, "new", r.cognitive, "new", r.loc, "new"
            );
        }
    }

    // Check for removed functions
    let current_names: std::collections::HashSet<&str> =
        current.iter().map(|r| r.name.as_str()).collect();
    for base in baseline {
        if !current_names.contains(base.name.as_str()) {
            println!(
                "{:<50}  {:>5}  {:>5}  {:>10}  {:>10}  {:>8}  {:>8}",
                base.name, "-", "del", "-", "del", "-", "del"
            );
        }
    }

    if regressions > 0 {
        eprintln!("\n{} function(s) regressed", regressions);
        if fail_regression {
            return true;
        }
    } else {
        println!("\nno regressions detected");
    }
    false
}

/// Prints a description of each report column.
pub fn print_column_descriptions() {
    let descriptions = [
        (
            "Function",
            "Fully qualified function name (module::Class.method or module::func)",
        ),
        (
            "McC",
            "McCabe cyclomatic complexity — number of independent execution paths",
        ),
        (
            "Cognitive",
            "Cognitive complexity — how hard the function is to understand",
        ),
        (
            "Nesting",
            "Maximum nesting depth of control-flow structures",
        ),
        ("LOC", "Lines of code in the function body"),
        ("Params", "Number of parameters"),
        ("FI", "Fan-In — number of direct callers"),
        ("FO", "Fan-Out — number of direct callees"),
        (
            "RFC",
            "Response For Class — count of reachable functions including self",
        ),
        (
            "Depth",
            "Maximum shortest-path distance to a reachable function; not runtime stack depth",
        ),
        (
            "T-LOC",
            "Transitive LOC — sum of LOC across all reachable callees (deduplicated)",
        ),
        (
            "T-McC",
            "Transitive McC — sum of McC across all reachable callees (deduplicated)",
        ),
        (
            "MI",
            "Maintainability Index (0-100) — higher is more maintainable",
        ),
        (
            "Returns",
            "Number of return statements in the function body",
        ),
        (
            "Bool-Expr",
            "Number of boolean operator expressions (and/or chains)",
        ),
        ("Lines", "Source line range (start-end)"),
    ];

    let width = descriptions
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap();
    for (name, desc) in &descriptions {
        println!("{:<width$}  {}", name, desc, width = width);
    }
}

/// Extracts a single metric value from a report row by column name.
pub fn extract_metric(row: &ReportRow, col: &str) -> Option<usize> {
    match col {
        "cc" => Some(row.cc),
        "cognitive" => Some(row.cognitive),
        "nesting" => Some(row.nesting),
        "loc" => Some(row.loc),
        "params" => Some(row.params),
        "fi" => Some(row.fan_in),
        "fo" => Some(row.fan_out),
        "rfc" => Some(row.rfc),
        "depth" => Some(row.depth),
        "t-loc" | "tloc" => Some(row.t_loc),
        "t-cc" | "tcc" | "t-mcc" | "tmcc" => Some(row.t_cc),
        "mi" => Some(row.mi as usize),
        "returns" | "ret" => Some(row.returns),
        "bool-expr" | "bool" => Some(row.bool_expr),
        _ => None,
    }
}

#[derive(Serialize)]
pub struct HistogramBucket {
    pub range_start: usize,
    pub range_end: usize,
    pub count: usize,
}

/// Builds histogram buckets from report rows for the given metric column.
pub fn build_histogram(rows: &[ReportRow], col: &str) -> (String, Vec<HistogramBucket>) {
    let metric_name = match col {
        "cc" => "McC",
        "cognitive" => "Cognitive",
        "nesting" => "Nesting",
        "loc" => "LOC",
        "params" => "Params",
        "fi" => "Fan-In",
        "fo" => "Fan-Out",
        "rfc" => "RFC",
        "depth" => "Depth",
        "t-loc" | "tloc" => "T-LOC",
        "t-cc" | "tcc" | "t-mcc" | "tmcc" => "T-McC",
        "mi" => "MI",
        "returns" | "ret" => "Returns",
        "bool-expr" | "bool" => "Bool-Expr",
        other => other,
    };

    let values: Vec<usize> = rows.iter().filter_map(|r| extract_metric(r, col)).collect();

    if values.is_empty() {
        return (metric_name.to_string(), vec![]);
    }

    let max_val = *values.iter().max().unwrap();

    // Exponential bins: 0, 1, 2-3, 4-7, 8-15, 16-31, ...
    let mut buckets: Vec<HistogramBucket> = Vec::new();
    buckets.push(HistogramBucket {
        range_start: 0,
        range_end: 0,
        count: 0,
    });
    if max_val >= 1 {
        let mut lo = 1usize;
        loop {
            let hi = (lo * 2 - 1).min(max_val);
            buckets.push(HistogramBucket {
                range_start: lo,
                range_end: hi,
                count: 0,
            });
            if hi >= max_val {
                break;
            }
            lo *= 2;
        }
    }

    let num_buckets = buckets.len();
    for v in &values {
        let idx = if *v == 0 {
            0
        } else {
            // floor(log2(v)) + 1 gives the bucket index (bucket 0 is for value 0)
            (usize::BITS - v.leading_zeros()) as usize
        };
        buckets[idx.min(num_buckets - 1)].count += 1;
    }

    (metric_name.to_string(), buckets)
}

/// Prints histogram in the given output format.
pub fn print_histogram(metric_name: &str, buckets: &[HistogramBucket], format: OutputFormat) {
    if buckets.is_empty() {
        println!("No functions found.");
        return;
    }

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&buckets).unwrap());
        }
        OutputFormat::Csv => {
            println!("range_start,range_end,count");
            for b in buckets {
                println!("{},{},{}", b.range_start, b.range_end, b.count);
            }
        }
        OutputFormat::Markdown => {
            println!("| Range Start | Range End | Count |");
            println!("|-------------|-----------|-------|");
            for b in buckets {
                println!("| {} | {} | {} |", b.range_start, b.range_end, b.count);
            }
            let total: usize = buckets.iter().map(|b| b.count).sum();
            println!("\n{} Distribution ({} functions)", metric_name, total);
        }
        OutputFormat::Table => {
            let max_count = buckets.iter().map(|b| b.count).max().unwrap_or(1).max(1);
            let max_bar_width = 40;

            let range_strs: Vec<String> = buckets
                .iter()
                .map(|b| {
                    if b.range_start == b.range_end {
                        format!("{}", b.range_start)
                    } else {
                        format!("{}-{}", b.range_start, b.range_end)
                    }
                })
                .collect();

            let range_width = range_strs.iter().map(|s| s.len()).max().unwrap_or(5);
            let count_width = buckets
                .iter()
                .map(|b| digit_width(b.count))
                .max()
                .unwrap_or(3);

            println!("{} Distribution", metric_name);
            println!();
            for (b, label) in buckets.iter().zip(range_strs.iter()) {
                let bar_len = if max_count > 0 {
                    (b.count * max_bar_width + max_count - 1) / max_count
                } else {
                    0
                };
                let bar: String = "█".repeat(bar_len);
                println!(
                    "  {:>rw$}  {:>cw$}  {}",
                    label,
                    b.count,
                    bar,
                    rw = range_width,
                    cw = count_width
                );
            }
            let total: usize = buckets.iter().map(|b| b.count).sum();
            println!("\n{} functions", total);
        }
    }
}
