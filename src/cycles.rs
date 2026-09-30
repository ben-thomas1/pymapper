use petgraph::algo::kosaraju_scc;
use serde::Serialize;

use crate::graph::CallGraph;
use crate::types::OutputFormat;

#[derive(Serialize)]
struct CycleRow {
    cycle: usize,
    functions: Vec<String>,
    size: usize,
}

/// Finds strongly connected components with more than one node.
pub fn find_cycles(call_graph: &CallGraph) -> Vec<Vec<String>> {
    let sccs = kosaraju_scc(&call_graph.graph);
    sccs.into_iter()
        .filter(|scc| scc.len() > 1)
        .map(|scc| {
            scc.iter()
                .map(|&idx| call_graph.graph[idx].clone())
                .collect()
        })
        .collect()
}

/// Prints cycle detection results.
pub fn print_cycles(cycles: &[Vec<String>], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            let rows: Vec<CycleRow> = cycles
                .iter()
                .enumerate()
                .map(|(i, c)| CycleRow {
                    cycle: i + 1,
                    functions: c.clone(),
                    size: c.len(),
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows).unwrap());
        }
        OutputFormat::Csv => {
            println!("cycle_id,function");
            for (i, cycle) in cycles.iter().enumerate() {
                for func in cycle {
                    println!("{},\"{}\"", i + 1, func);
                }
            }
        }
        OutputFormat::Markdown => {
            if cycles.is_empty() {
                println!("No cycles found.");
                return;
            }
            println!("| Cycle | Size | Functions |");
            println!("|-------|------|-----------|");
            for (i, cycle) in cycles.iter().enumerate() {
                let chain: String = cycle
                    .iter()
                    .chain(std::iter::once(&cycle[0]))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" → ");
                println!("| {} | {} | {} |", i + 1, cycle.len(), chain);
            }
            let total_funcs: usize = cycles.iter().map(|c| c.len()).sum();
            println!(
                "\n{} cycle(s) found involving {} functions",
                cycles.len(),
                total_funcs
            );
        }
        OutputFormat::Table => {
            if cycles.is_empty() {
                println!("No cycles found.");
                return;
            }
            for (i, cycle) in cycles.iter().enumerate() {
                println!("Cycle {} ({} functions):", i + 1, cycle.len());
                let chain: String = cycle
                    .iter()
                    .chain(std::iter::once(&cycle[0]))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" → ");
                println!("  {}", chain);
                println!();
            }
            let total_funcs: usize = cycles.iter().map(|c| c.len()).sum();
            println!(
                "{} cycle(s) found involving {} functions",
                cycles.len(),
                total_funcs
            );
        }
    }
}
