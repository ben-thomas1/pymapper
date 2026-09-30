use std::collections::{HashMap, HashSet, VecDeque};

// ---------------------------------------------------------------------------
// Direction enum for spatial navigation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

// ---------------------------------------------------------------------------
// Sugiyama (layered) layout for local ego graphs
// ---------------------------------------------------------------------------

/// Extracts the N-hop neighborhood of `center` and lays it out in layers.
pub fn sugiyama_layout(
    center: &str,
    callers: &HashMap<String, Vec<String>>,
    callees: &HashMap<String, Vec<String>>,
    max_hops: usize,
) -> HashMap<String, (f64, f64)> {
    // BFS outward from center, collecting nodes by layer.
    // Negative layers = callers (above), positive layers = callees (below).
    let mut layer_of: HashMap<String, i32> = HashMap::new();
    layer_of.insert(center.to_string(), 0);

    // BFS upstream (callers) — negative layers
    {
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        queue.push_back((center.to_string(), 0));
        while let Some((node, depth)) = queue.pop_front() {
            if depth >= max_hops {
                continue;
            }
            if let Some(preds) = callers.get(&node) {
                for pred in preds {
                    if !layer_of.contains_key(pred) {
                        let layer = -(depth as i32 + 1);
                        layer_of.insert(pred.clone(), layer);
                        queue.push_back((pred.clone(), depth + 1));
                    }
                }
            }
        }
    }

    // BFS downstream (callees) — positive layers
    {
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        queue.push_back((center.to_string(), 0));
        while let Some((node, depth)) = queue.pop_front() {
            if depth >= max_hops {
                continue;
            }
            if let Some(succs) = callees.get(&node) {
                for succ in succs {
                    if !layer_of.contains_key(succ) {
                        let layer = depth as i32 + 1;
                        layer_of.insert(succ.clone(), layer);
                        queue.push_back((succ.clone(), depth + 1));
                    }
                }
            }
        }
    }

    // Group nodes by layer
    let mut layers: HashMap<i32, Vec<String>> = HashMap::new();
    for (node, layer) in &layer_of {
        layers.entry(*layer).or_default().push(node.clone());
    }

    // Barycenter ordering within each layer to reduce crossings
    let mut layer_keys: Vec<i32> = layers.keys().copied().collect();
    layer_keys.sort();

    // Build adjacency for barycenter: for each node, positions of its neighbors
    // in the adjacent layer
    let mut ordered_layers: HashMap<i32, Vec<String>> = HashMap::new();

    // First pass: sort alphabetically as initial order
    for &lk in &layer_keys {
        let mut nodes = layers[&lk].clone();
        nodes.sort();
        ordered_layers.insert(lk, nodes);
    }

    // Barycenter sweep: top-down then bottom-up
    for _pass in 0..2 {
        for i in 0..layer_keys.len() {
            let lk = layer_keys[i];
            if lk == 0 {
                // Center layer: center node stays in the middle
                let mut nodes = ordered_layers[&lk].clone();
                if let Some(pos) = nodes.iter().position(|n| n == center) {
                    nodes.remove(pos);
                    nodes.insert(nodes.len() / 2, center.to_string());
                }
                ordered_layers.insert(lk, nodes);
                continue;
            }

            let adjacent_layer = if lk < 0 { lk + 1 } else { lk - 1 };
            if let Some(adj_nodes) = ordered_layers.get(&adjacent_layer).cloned() {
                let adj_positions: HashMap<&str, usize> = adj_nodes
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.as_str(), i))
                    .collect();

                let mut nodes = ordered_layers[&lk].clone();
                let mut barycenters: HashMap<String, f64> = HashMap::new();

                for node in &nodes {
                    let neighbors: Vec<usize> = if lk < 0 {
                        // This node's callees that are in the adjacent (lower-numbered) layer
                        callees
                            .get(node)
                            .map(|v| {
                                v.iter()
                                    .filter_map(|n| adj_positions.get(n.as_str()).copied())
                                    .collect()
                            })
                            .unwrap_or_default()
                    } else {
                        // This node's callers that are in the adjacent layer
                        callers
                            .get(node)
                            .map(|v| {
                                v.iter()
                                    .filter_map(|n| adj_positions.get(n.as_str()).copied())
                                    .collect()
                            })
                            .unwrap_or_default()
                    };

                    if neighbors.is_empty() {
                        barycenters.insert(node.clone(), f64::MAX);
                    } else {
                        let avg = neighbors.iter().sum::<usize>() as f64 / neighbors.len() as f64;
                        barycenters.insert(node.clone(), avg);
                    }
                }

                nodes.sort_by(|a, b| {
                    barycenters[a]
                        .partial_cmp(&barycenters[b])
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                ordered_layers.insert(lk, nodes);
            }
        }
    }

    // Assign coordinates
    let layer_spacing_y = 6.0;
    let node_spacing_x = 14.0;

    let mut positions: HashMap<String, (f64, f64)> = HashMap::new();

    for &lk in &layer_keys {
        let nodes = &ordered_layers[&lk];
        let y = -(lk as f64) * layer_spacing_y; // negate so callers (neg layer) are above
        let total_width = (nodes.len() as f64 - 1.0) * node_spacing_x;
        let start_x = -total_width / 2.0;

        for (i, node) in nodes.iter().enumerate() {
            let x = start_x + i as f64 * node_spacing_x;
            positions.insert(node.clone(), (x, y));
        }
    }

    positions
}

// ---------------------------------------------------------------------------
// Force-directed (Fruchterman-Reingold) layout for global view
// ---------------------------------------------------------------------------

/// Computes a force-directed layout for the given nodes and edges.
pub fn force_directed_layout(
    nodes: &[String],
    edges: &[(String, String)],
    iterations: usize,
) -> HashMap<String, (f64, f64)> {
    if nodes.is_empty() {
        return HashMap::new();
    }
    if nodes.len() == 1 {
        let mut m = HashMap::new();
        m.insert(nodes[0].clone(), (0.0, 0.0));
        return m;
    }

    let n = nodes.len();
    let name_to_idx: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i))
        .collect();

    // Initial positions: arrange in a circle
    let mut pos: Vec<(f64, f64)> = Vec::with_capacity(n);
    for i in 0..n {
        let angle = 2.0 * std::f64::consts::PI * i as f64 / n as f64;
        pos.push((angle.cos() * 50.0, angle.sin() * 50.0));
    }

    // Build edge index list
    let edge_indices: Vec<(usize, usize)> = edges
        .iter()
        .filter_map(|(a, b)| {
            let ai = name_to_idx.get(a.as_str())?;
            let bi = name_to_idx.get(b.as_str())?;
            Some((*ai, *bi))
        })
        .collect();

    let area = (n as f64).sqrt() * 40.0;
    let k = (area / n as f64).sqrt(); // ideal spring length

    for iter in 0..iterations {
        let temperature = area * (1.0 - iter as f64 / iterations as f64) * 0.1;
        if temperature < 0.01 {
            break;
        }

        let mut disp: Vec<(f64, f64)> = vec![(0.0, 0.0); n];

        // Repulsive forces between all pairs
        for i in 0..n {
            for j in (i + 1)..n {
                let dx = pos[i].0 - pos[j].0;
                let dy = pos[i].1 - pos[j].1;
                let dist = (dx * dx + dy * dy).sqrt().max(0.01);
                let force = k * k / dist;
                let fx = dx / dist * force;
                let fy = dy / dist * force;
                disp[i].0 += fx;
                disp[i].1 += fy;
                disp[j].0 -= fx;
                disp[j].1 -= fy;
            }
        }

        // Attractive forces along edges
        for &(u, v) in &edge_indices {
            let dx = pos[u].0 - pos[v].0;
            let dy = pos[u].1 - pos[v].1;
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let force = dist * dist / k;
            let fx = dx / dist * force;
            let fy = dy / dist * force;
            disp[u].0 -= fx;
            disp[u].1 -= fy;
            disp[v].0 += fx;
            disp[v].1 += fy;
        }

        // Apply displacements, clamped by temperature
        for i in 0..n {
            let dx = disp[i].0;
            let dy = disp[i].1;
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let capped = dist.min(temperature);
            pos[i].0 += dx / dist * capped;
            pos[i].1 += dy / dist * capped;
        }
    }

    // Center the layout around origin
    let cx: f64 = pos.iter().map(|p| p.0).sum::<f64>() / n as f64;
    let cy: f64 = pos.iter().map(|p| p.1).sum::<f64>() / n as f64;

    nodes
        .iter()
        .enumerate()
        .map(|(i, name)| (name.clone(), (pos[i].0 - cx, pos[i].1 - cy)))
        .collect()
}

// ---------------------------------------------------------------------------
// Spatial navigation helper
// ---------------------------------------------------------------------------

/// Finds the nearest node in the given direction from `current` position.
pub fn nearest_node_in_direction(
    current: (f64, f64),
    direction: Direction,
    positions: &HashMap<String, (f64, f64)>,
    current_node: &str,
) -> Option<String> {
    let mut best: Option<(String, f64)> = None;

    for (name, &(x, y)) in positions {
        if name == current_node {
            continue;
        }

        let dx = x - current.0;
        let dy = y - current.1;

        // Check if the node is in the correct direction.
        // Use a cone: the component in the desired direction must be larger
        // than the perpendicular component.
        let in_direction = match direction {
            Direction::Up => dy > 0.0 && dy.abs() >= dx.abs(),
            Direction::Down => dy < 0.0 && dy.abs() >= dx.abs(),
            Direction::Right => dx > 0.0 && dx.abs() >= dy.abs(),
            Direction::Left => dx < 0.0 && dx.abs() >= dy.abs(),
        };

        if !in_direction {
            continue;
        }

        let dist = (dx * dx + dy * dy).sqrt();
        if best.as_ref().is_none_or(|(_, d)| dist < *d) {
            best = Some((name.clone(), dist));
        }
    }

    best.map(|(name, _)| name)
}

// ---------------------------------------------------------------------------
// Ego subgraph extraction
// ---------------------------------------------------------------------------

/// Extracts the N-hop neighborhood (nodes + edges) around `center`.
pub fn extract_ego_subgraph(
    center: &str,
    callers: &HashMap<String, Vec<String>>,
    callees: &HashMap<String, Vec<String>>,
    max_hops: usize,
) -> (
    Vec<String>,
    Vec<(String, String)>,
    HashSet<(String, String)>,
) {
    let mut nodes: HashSet<String> = HashSet::new();
    let mut back_edges: HashSet<(String, String)> = HashSet::new();
    nodes.insert(center.to_string());

    // BFS upstream
    {
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        queue.push_back((center.to_string(), 0));
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(center.to_string());
        while let Some((node, depth)) = queue.pop_front() {
            if depth >= max_hops {
                continue;
            }
            if let Some(preds) = callers.get(&node) {
                for pred in preds {
                    nodes.insert(pred.clone());
                    if visited.contains(pred) {
                        back_edges.insert((pred.clone(), node.clone()));
                    } else {
                        visited.insert(pred.clone());
                        queue.push_back((pred.clone(), depth + 1));
                    }
                }
            }
        }
    }

    // BFS downstream
    {
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        queue.push_back((center.to_string(), 0));
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(center.to_string());
        while let Some((node, depth)) = queue.pop_front() {
            if depth >= max_hops {
                continue;
            }
            if let Some(succs) = callees.get(&node) {
                for succ in succs {
                    nodes.insert(succ.clone());
                    if visited.contains(succ) {
                        back_edges.insert((node.clone(), succ.clone()));
                    } else {
                        visited.insert(succ.clone());
                        queue.push_back((succ.clone(), depth + 1));
                    }
                }
            }
        }
    }

    // Collect edges between nodes in the subgraph
    let mut edges: Vec<(String, String)> = Vec::new();
    for node in &nodes {
        if let Some(succs) = callees.get(node) {
            for succ in succs {
                if nodes.contains(succ) {
                    edges.push((node.clone(), succ.clone()));
                }
            }
        }
    }

    let node_vec: Vec<String> = nodes.into_iter().collect();
    (node_vec, edges, back_edges)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_callers_callees(
        edges: &[(&str, &str)],
    ) -> (HashMap<String, Vec<String>>, HashMap<String, Vec<String>>) {
        let mut callers: HashMap<String, Vec<String>> = HashMap::new();
        let mut callees: HashMap<String, Vec<String>> = HashMap::new();
        for &(from, to) in edges {
            callees
                .entry(from.to_string())
                .or_default()
                .push(to.to_string());
            callers
                .entry(to.to_string())
                .or_default()
                .push(from.to_string());
        }
        (callers, callees)
    }

    // -- sugiyama_layout tests --

    #[test]
    fn sugiyama_single_node() {
        let callers = HashMap::new();
        let callees = HashMap::new();
        let pos = sugiyama_layout("A", &callers, &callees, 2);
        assert_eq!(pos.len(), 1);
        assert_eq!(pos["A"], (0.0, 0.0));
    }

    #[test]
    fn sugiyama_linear_chain() {
        // A -> B -> C
        let (callers, callees) = make_callers_callees(&[("A", "B"), ("B", "C")]);
        let pos = sugiyama_layout("B", &callers, &callees, 2);
        assert_eq!(pos.len(), 3);
        // A is a caller (layer -1) → should be above B (higher y)
        assert!(pos["A"].1 > pos["B"].1, "caller A should be above center B");
        // C is a callee (layer +1) → should be below B (lower y)
        assert!(pos["C"].1 < pos["B"].1, "callee C should be below center B");
    }

    #[test]
    fn sugiyama_fan_out() {
        // center -> A, center -> B, center -> C
        let (callers, callees) =
            make_callers_callees(&[("center", "A"), ("center", "B"), ("center", "C")]);
        let pos = sugiyama_layout("center", &callers, &callees, 1);
        assert_eq!(pos.len(), 4);
        // All callees should be on the same layer (same y)
        let ya = pos["A"].1;
        let yb = pos["B"].1;
        let yc = pos["C"].1;
        assert!((ya - yb).abs() < 0.01, "callees should share a layer");
        assert!((yb - yc).abs() < 0.01, "callees should share a layer");
        // Callees should be below center
        assert!(ya < pos["center"].1, "callees should be below center");
    }

    #[test]
    fn sugiyama_with_cycle() {
        // A -> B -> C -> A (cycle); center on B
        let (callers, callees) = make_callers_callees(&[("A", "B"), ("B", "C"), ("C", "A")]);
        let pos = sugiyama_layout("B", &callers, &callees, 2);
        // Should not panic or loop forever; all 3 nodes placed
        assert_eq!(pos.len(), 3);
    }

    // -- force_directed_layout tests --

    #[test]
    fn force_directed_single_node() {
        let nodes = vec!["A".to_string()];
        let edges: Vec<(String, String)> = vec![];
        let pos = force_directed_layout(&nodes, &edges, 50);
        assert_eq!(pos.len(), 1);
        assert_eq!(pos["A"], (0.0, 0.0));
    }

    #[test]
    fn force_directed_non_overlapping() {
        let nodes: Vec<String> = ["A", "B", "C", "D"].iter().map(|s| s.to_string()).collect();
        let edges = vec![
            ("A".to_string(), "B".to_string()),
            ("B".to_string(), "C".to_string()),
            ("C".to_string(), "D".to_string()),
        ];
        let pos = force_directed_layout(&nodes, &edges, 100);
        assert_eq!(pos.len(), 4);
        // Check no two nodes are at the same position
        let positions: Vec<(f64, f64)> = pos.values().copied().collect();
        for i in 0..positions.len() {
            for j in (i + 1)..positions.len() {
                let dist = ((positions[i].0 - positions[j].0).powi(2)
                    + (positions[i].1 - positions[j].1).powi(2))
                .sqrt();
                assert!(dist > 0.1, "nodes should not overlap: dist={dist}");
            }
        }
    }

    #[test]
    fn force_directed_empty() {
        let nodes: Vec<String> = vec![];
        let edges: Vec<(String, String)> = vec![];
        let pos = force_directed_layout(&nodes, &edges, 50);
        assert!(pos.is_empty());
    }

    // -- nearest_node_in_direction tests --

    #[test]
    fn nearest_up() {
        let mut positions = HashMap::new();
        positions.insert("A".to_string(), (0.0, 10.0));
        positions.insert("B".to_string(), (0.0, -10.0));
        positions.insert("C".to_string(), (10.0, 0.0));

        let result = nearest_node_in_direction((0.0, 0.0), Direction::Up, &positions, "center");
        assert_eq!(result, Some("A".to_string()));
    }

    #[test]
    fn nearest_down() {
        let mut positions = HashMap::new();
        positions.insert("A".to_string(), (0.0, 10.0));
        positions.insert("B".to_string(), (0.0, -10.0));

        let result = nearest_node_in_direction((0.0, 0.0), Direction::Down, &positions, "center");
        assert_eq!(result, Some("B".to_string()));
    }

    #[test]
    fn nearest_none_in_direction() {
        let mut positions = HashMap::new();
        positions.insert("A".to_string(), (0.0, 10.0)); // above only

        let result = nearest_node_in_direction((0.0, 0.0), Direction::Down, &positions, "center");
        assert_eq!(result, None);
    }

    #[test]
    fn nearest_skips_current() {
        let mut positions = HashMap::new();
        positions.insert("center".to_string(), (0.0, 0.0));
        positions.insert("A".to_string(), (0.0, 10.0));

        let result = nearest_node_in_direction((0.0, 0.0), Direction::Up, &positions, "center");
        assert_eq!(result, Some("A".to_string()));
    }
}
