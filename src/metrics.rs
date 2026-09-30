use std::collections::{HashMap, HashSet, VecDeque};

use petgraph::Direction;

use crate::graph::CallGraph;
use crate::types::FunctionInfo;

/// Graph-aggregate metrics for a single function node.
pub struct GraphMetrics {
    pub fan_in: usize,
    pub fan_out: usize,
    pub rfc: usize,
    pub call_depth: usize,
    pub t_loc: usize,
    pub t_cc: usize,
}

/// Computes graph-aggregate metrics for every internal node in the call graph.
pub fn compute_graph_metrics(
    call_graph: &CallGraph,
    functions: &[FunctionInfo],
) -> HashMap<String, GraphMetrics> {
    compute_graph_metrics_with_progress(call_graph, functions, |_, _| {})
}

/// Computes graph-aggregate metrics with a progress callback invoked per function.
pub fn compute_graph_metrics_with_progress(
    call_graph: &CallGraph,
    functions: &[FunctionInfo],
    on_progress: impl Fn(usize, usize),
) -> HashMap<String, GraphMetrics> {
    let func_data: HashMap<String, (usize, usize)> = functions
        .iter()
        .map(|f| (f.qualified_name(), (f.line_count, f.complexity)))
        .collect();

    let mut result = HashMap::new();
    let total = functions.len();

    for (i, func) in functions.iter().enumerate() {
        let qname = func.qualified_name();
        let &node_idx = call_graph
            .node_map
            .get(&qname)
            .expect("all functions should be in node_map");

        let fan_in = call_graph
            .graph
            .neighbors_directed(node_idx, Direction::Incoming)
            .collect::<HashSet<_>>()
            .len();
        let fan_out = call_graph
            .graph
            .neighbors_directed(node_idx, Direction::Outgoing)
            .collect::<HashSet<_>>()
            .len();

        // BFS computes maximum shortest-path distance, independent of edge order.
        let mut visited = HashSet::new();
        let mut stack = VecDeque::from([(node_idx, 0usize)]);
        let mut max_depth = 0usize;
        let mut t_loc = 0usize;
        let mut t_cc = 0usize;

        while let Some((current, depth)) = stack.pop_front() {
            if !visited.insert(current) {
                continue;
            }
            if depth > max_depth {
                max_depth = depth;
            }

            let name = &call_graph.graph[current];
            if let Some(&(loc, cc)) = func_data.get(name) {
                t_loc += loc;
                t_cc += cc;
            }

            for neighbor in call_graph.graph.neighbors(current) {
                if !visited.contains(&neighbor) {
                    stack.push_back((neighbor, depth + 1));
                }
            }
        }

        let rfc = visited.len();

        result.insert(
            qname,
            GraphMetrics {
                fan_in,
                fan_out,
                rfc,
                call_depth: max_depth,
                t_loc,
                t_cc,
            },
        );
        on_progress(i + 1, total);
    }

    result
}
