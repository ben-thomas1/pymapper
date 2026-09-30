use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use petgraph::graph::{DiGraph, NodeIndex};

use crate::types::{ImportEntry, ModuleInfo, Splines};

/// Pre-built call graph with node index mapping.
pub struct CallGraph {
    pub graph: DiGraph<String, ()>,
    pub node_map: HashMap<String, NodeIndex>,
    pub external_nodes: HashSet<String>,
}

/// Builds a call graph from extracted module info.
pub fn build_call_graph(module: &ModuleInfo, no_externals: bool) -> CallGraph {
    build_call_graph_with_progress(module, no_externals, |_, _| {})
}

/// Builds a call graph with a progress callback invoked per function processed.
pub fn build_call_graph_with_progress(
    module: &ModuleInfo,
    no_externals: bool,
    on_progress: impl Fn(usize, usize),
) -> CallGraph {
    let mut graph: DiGraph<String, ()> = DiGraph::new();
    let mut node_map: HashMap<String, NodeIndex> = HashMap::new();
    let mut external_nodes: HashSet<String> = HashSet::new();

    let mut known: HashSet<String> = HashSet::new();
    for func in &module.functions {
        known.insert(func.qualified_name());
    }

    for func in &module.functions {
        let qname = func.qualified_name();
        let idx = graph.add_node(qname.clone());
        node_map.insert(qname, idx);
    }

    let inheritance: HashMap<&str, &[String]> = module
        .classes
        .iter()
        .map(|c| (c.name.as_str(), c.base_classes.as_slice()))
        .collect();

    let mut import_map: HashMap<String, Vec<&ImportEntry>> = HashMap::new();
    for imp in &module.imports {
        import_map
            .entry(imp.owning_module.clone())
            .or_default()
            .push(imp);
    }

    let total = module.functions.len();
    for (i, func) in module.functions.iter().enumerate() {
        let caller = func.qualified_name();
        let caller_idx = node_map[&caller];

        for call in &func.calls {
            let target = resolve_call(
                func.parent_class.as_deref(),
                &call.name,
                call.is_self,
                &func.source_module,
                &known,
                &inheritance,
                &import_map,
            );
            let target_idx = if let Some(idx) = node_map.get(&target) {
                *idx
            } else {
                if no_externals {
                    continue;
                }
                if !external_nodes.contains(&target) {
                    external_nodes.insert(target.clone());
                    let idx = graph.add_node(target.clone());
                    node_map.insert(target.clone(), idx);
                }
                node_map[&target]
            };
            graph.add_edge(caller_idx, target_idx, ());
        }
        for dec_name in &func.decorators {
            let target = resolve_call(
                func.parent_class.as_deref(),
                dec_name,
                false,
                &func.source_module,
                &known,
                &inheritance,
                &import_map,
            );
            let target_idx = if let Some(idx) = node_map.get(&target) {
                *idx
            } else {
                if no_externals {
                    continue;
                }
                if !external_nodes.contains(&target) {
                    external_nodes.insert(target.clone());
                    let idx = graph.add_node(target.clone());
                    node_map.insert(target.clone(), idx);
                }
                node_map[&target]
            };
            graph.add_edge(caller_idx, target_idx, ());
        }
        on_progress(i + 1, total);
    }

    CallGraph {
        graph,
        node_map,
        external_nodes,
    }
}

/// Renders a pre-built call graph as DOT format.
pub fn emit_dot(module: &ModuleInfo, call_graph: &CallGraph, splines: Splines) -> String {
    let CallGraph {
        graph,
        node_map,
        external_nodes,
    } = call_graph;

    let mut dot = String::new();
    writeln!(dot, "digraph PyMapper {{").unwrap();
    writeln!(dot, "  rankdir=LR;").unwrap();
    writeln!(dot, "  splines={};", splines.as_dot_str()).unwrap();
    writeln!(dot, "  concentrate=true;").unwrap();
    writeln!(dot, "  nodesep=0.8;").unwrap();
    writeln!(dot, "  ranksep=1.2;").unwrap();
    writeln!(
        dot,
        "  node [shape=box, style=filled, fillcolor=lightyellow, fontsize=10];"
    )
    .unwrap();
    writeln!(dot).unwrap();

    let label_map: HashMap<String, String> = module
        .functions
        .iter()
        .map(|f| {
            (
                f.qualified_name(),
                format!(
                    "{}\\nCC: {} | Cog: {} | Nest: {}\\nLOC: {} | Params: {}",
                    f.name,
                    f.complexity,
                    f.cognitive_complexity,
                    f.max_nesting,
                    f.line_count,
                    f.param_count
                ),
            )
        })
        .collect();

    let module_prefixes: Vec<String> = {
        let mut set: Vec<String> = module
            .functions
            .iter()
            .map(|f| f.source_module.clone())
            .chain(module.classes.iter().map(|c| c.source_module.clone()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        set.sort();
        set
    };

    for prefix in &module_prefixes {
        let module_label = if prefix.is_empty() {
            "(root)"
        } else {
            prefix.as_str()
        };
        let cluster_id = if prefix.is_empty() {
            "root".to_string()
        } else {
            sanitize_id(prefix)
        };

        writeln!(dot, "  subgraph cluster_mod_{} {{", cluster_id).unwrap();
        writeln!(dot, "    label=\"{}\";", dot_escape(module_label)).unwrap();
        writeln!(dot, "    style=rounded;").unwrap();
        writeln!(dot, "    color=blue;").unwrap();
        writeln!(dot).unwrap();

        for class in module.classes.iter().filter(|c| &c.source_module == prefix) {
            let class_cluster = if prefix.is_empty() {
                format!("cluster_{}", sanitize_id(&class.name))
            } else {
                format!("cluster_{}_{}", cluster_id, sanitize_id(&class.name))
            };
            writeln!(dot, "    subgraph {} {{", class_cluster).unwrap();
            writeln!(dot, "      label=\"{}\";", class.name).unwrap();
            writeln!(dot, "      style=dashed;").unwrap();
            for method_name in &class.method_names {
                let qname = if prefix.is_empty() {
                    format!("{}.{}", class.name, method_name)
                } else {
                    format!("{}::{}.{}", prefix, class.name, method_name)
                };
                if node_map.contains_key(&qname) {
                    let label = label_map.get(&qname).map(|s| s.as_str()).unwrap_or(&qname);
                    writeln!(
                        dot,
                        "      \"{}\" [label=\"{}\"];",
                        dot_escape(&qname),
                        dot_escape(label)
                    )
                    .unwrap();
                }
            }
            writeln!(dot, "    }}").unwrap();
            writeln!(dot).unwrap();
        }

        for func in module
            .functions
            .iter()
            .filter(|f| &f.source_module == prefix && f.parent_class.is_none())
        {
            let qname = func.qualified_name();
            if node_map.contains_key(&qname) {
                let label = label_map.get(&qname).map(|s| s.as_str()).unwrap_or(&qname);
                writeln!(
                    dot,
                    "    \"{}\" [label=\"{}\"];",
                    dot_escape(&qname),
                    dot_escape(label)
                )
                .unwrap();
            }
        }

        writeln!(dot, "  }}").unwrap();
        writeln!(dot).unwrap();
    }

    let idx_to_name: HashMap<NodeIndex, &str> = node_map
        .iter()
        .map(|(name, &idx)| (idx, name.as_str()))
        .collect();

    for ext in external_nodes {
        writeln!(
            dot,
            "  \"{}\" [label=\"{}\", style=\"filled,dotted\", fillcolor=lightgray];",
            dot_escape(ext),
            dot_escape(ext)
        )
        .unwrap();
    }
    writeln!(dot).unwrap();

    for edge in graph.edge_indices() {
        let (src, dst) = graph.edge_endpoints(edge).unwrap();
        let src_name = idx_to_name.get(&src).copied().unwrap_or("unknown");
        let dst_name = idx_to_name.get(&dst).copied().unwrap_or("unknown");
        writeln!(
            dot,
            "  \"{}\" -> \"{}\";",
            dot_escape(src_name),
            dot_escape(dst_name)
        )
        .unwrap();
    }
    writeln!(dot).unwrap();

    for class in &module.classes {
        for base in &class.base_classes {
            writeln!(
                dot,
                "  \"{}\" -> \"{}\" [style=dashed, label=\"inherits\"];",
                class.name, base
            )
            .unwrap();
        }
    }

    writeln!(dot, "}}").unwrap();
    dot
}

fn resolve_call(
    parent_class: Option<&str>,
    name: &str,
    is_self: bool,
    source_module: &str,
    known: &HashSet<String>,
    inheritance: &HashMap<&str, &[String]>,
    import_map: &HashMap<String, Vec<&ImportEntry>>,
) -> String {
    // 1. Self-calls: walk inheritance chain
    if is_self {
        if let Some(class) = parent_class {
            let mut current = Some(class.to_string());
            let mut seen = HashSet::new();
            while let Some(cls) = current {
                if !seen.insert(cls.clone()) {
                    break;
                }
                if !source_module.is_empty() {
                    let qualified = format!("{}::{}.{}", source_module, cls, name);
                    if known.contains(&qualified) {
                        return qualified;
                    }
                }
                let qualified = format!("{}.{}", cls, name);
                if known.contains(&qualified) {
                    return qualified;
                }
                current = inheritance
                    .get(cls.as_str())
                    .and_then(|bases| bases.first())
                    .cloned();
            }
        }
    }

    // 2. Same-module prefix (higher priority than imports for local definitions)
    if !source_module.is_empty() {
        let prefixed = format!("{}::{}", source_module, name);
        if known.contains(&prefixed) {
            return prefixed;
        }
    }

    // 3. Import-based resolution
    if let Some(imports) = import_map.get(source_module) {
        // 3a. Bare-name lookup: `from b import foo; foo()`
        for imp in imports.iter().rev() {
            if imp.local_name == name {
                let resolved_name = imp.imported_name.as_deref().unwrap_or(&imp.local_name);
                let qualified = format!("{}::{}", imp.source_module, resolved_name);
                if known.contains(&qualified) {
                    return qualified;
                }
            }
        }

        // 3b. Attribute-call lookup: `import utils; utils.helper()`
        if let Some((prefix, attr)) = name.split_once('.') {
            for imp in imports.iter().rev() {
                if imp.local_name == prefix && imp.imported_name.is_none() {
                    let qualified = format!("{}::{}", imp.source_module, attr);
                    if known.contains(&qualified) {
                        return qualified;
                    }
                }
            }
        }
    }

    // 4. Global bare-name fallback
    if known.contains(name) {
        return name.to_string();
    }
    name.to_string()
}

/// Sanitizes a string for use as a DOT identifier (no quotes).
fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
