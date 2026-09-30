use std::collections::HashSet;

use tree_sitter::Node;

/// Computes McCabe cyclomatic complexity for a function body node.
pub fn compute_complexity(node: &Node) -> usize {
    let mut count = 0;
    count_decision_points(node, &mut count);
    count + 1
}

fn count_decision_points(node: &Node, count: &mut usize) {
    if matches!(
        node.kind(),
        "function_definition" | "class_definition" | "lambda"
    ) {
        return;
    }
    match node.kind() {
        "if_statement"
        | "elif_clause"
        | "for_statement"
        | "while_statement"
        | "except_clause"
        | "with_statement"
        | "assert_statement"
        | "conditional_expression"
        | "if_clause" => {
            *count += 1;
        }
        "boolean_operator" => {
            *count += 1;
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count_decision_points(&child, count);
    }
}

/// Computes cognitive complexity for a function body node.
pub fn compute_cognitive_complexity(node: &Node) -> usize {
    let mut score = 0;
    cognitive_walk(node, 0, &mut score);
    score
}

fn cognitive_walk(node: &Node, nesting: usize, score: &mut usize) {
    if matches!(
        node.kind(),
        "function_definition" | "class_definition" | "lambda"
    ) {
        return;
    }
    let control = matches!(
        node.kind(),
        "if_statement" | "for_statement" | "while_statement" | "except_clause" | "with_statement"
    );
    let branch = matches!(node.kind(), "elif_clause" | "else_clause");
    if control {
        *score += 1 + nesting;
    } else if branch || node.kind() == "boolean_operator" {
        *score += 1;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let child_nesting = if child.kind() == "block" && (control || branch) {
            nesting + 1
        } else {
            nesting
        };
        cognitive_walk(&child, child_nesting, score);
    }
}

/// Computes the maximum nesting depth of control structures.
pub fn compute_max_nesting(node: &Node) -> usize {
    let mut max_depth = 0;
    nesting_walk(node, 0, &mut max_depth);
    max_depth
}

fn nesting_walk(node: &Node, depth: usize, max_depth: &mut usize) {
    if matches!(
        node.kind(),
        "function_definition" | "class_definition" | "lambda"
    ) {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "if_statement" | "for_statement" | "while_statement" | "with_statement"
            | "try_statement" => {
                let new_depth = depth + 1;
                if new_depth > *max_depth {
                    *max_depth = new_depth;
                }
                let mut inner = child.walk();
                for grandchild in child.children(&mut inner) {
                    if grandchild.kind() == "block"
                        || grandchild.kind() == "elif_clause"
                        || grandchild.kind() == "else_clause"
                        || grandchild.kind() == "except_clause"
                        || grandchild.kind() == "finally_clause"
                    {
                        nesting_walk(&grandchild, new_depth, max_depth);
                    } else {
                        nesting_walk(&grandchild, depth, max_depth);
                    }
                }
            }
            _ => {
                nesting_walk(&child, depth, max_depth);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Halstead metrics
// ---------------------------------------------------------------------------

/// Halstead metrics for a function.
#[allow(dead_code)]
pub struct HalsteadMetrics {
    pub volume: f64,
    pub difficulty: f64,
    pub effort: f64,
}

const OPERATOR_KINDS: &[&str] = &[
    // Arithmetic
    "+", "-", "*", "/", "//", "%", "**", // Assignment
    "=", "+=", "-=", "*=", "/=", "//=", "%=", "**=", "&=", "|=", "^=", "<<=", ">>=",
    // Comparison
    "==", "!=", "<", ">", "<=", ">=", // Logical / bitwise
    "and", "or", "not", "&", "|", "^", "~", "<<", ">>", // Keywords that act as operators
    "in", "is", "not in", "is not",
    // Control flow keywords (counted as operators in Halstead)
    "if", "elif", "else", "for", "while", "return", "yield", "raise", "try", "except", "finally",
    "with", "as", "assert", "pass", "break", "continue", "def", "class", "lambda", "import",
    "from", "del", "global", "nonlocal", "await",
];

const OPERAND_KINDS: &[&str] = &[
    "identifier",
    "integer",
    "float",
    "string",
    "true",
    "false",
    "none",
    "concatenated_string",
    "formatted_string",
];

fn is_operator(kind: &str) -> bool {
    OPERATOR_KINDS.contains(&kind)
}

fn is_operand(kind: &str) -> bool {
    OPERAND_KINDS.contains(&kind)
}

/// Computes Halstead metrics for a function body node.
pub fn compute_halstead(node: &Node, src: &[u8]) -> HalsteadMetrics {
    let mut operators: Vec<String> = Vec::new();
    let mut operands: Vec<String> = Vec::new();
    collect_halstead(node, src, &mut operators, &mut operands);

    let n1 = operators.len(); // total operators
    let n2 = operands.len(); // total operands
    let eta1: HashSet<&str> = operators.iter().map(|s| s.as_str()).collect();
    let eta2: HashSet<&str> = operands.iter().map(|s| s.as_str()).collect();
    let n1_unique = eta1.len();
    let n2_unique = eta2.len();

    let n = n1 + n2;
    let eta = n1_unique + n2_unique;

    if eta == 0 || n == 0 || n2_unique == 0 {
        return HalsteadMetrics {
            volume: 0.0,
            difficulty: 0.0,
            effort: 0.0,
        };
    }

    let volume = n as f64 * (eta as f64).log2();
    let difficulty = (n1_unique as f64 / 2.0) * (n2 as f64 / n2_unique as f64);
    let effort = volume * difficulty;

    HalsteadMetrics {
        volume,
        difficulty,
        effort,
    }
}

fn collect_halstead(
    node: &Node,
    src: &[u8],
    operators: &mut Vec<String>,
    operands: &mut Vec<String>,
) {
    if matches!(
        node.kind(),
        "function_definition" | "class_definition" | "lambda"
    ) {
        return;
    }
    let kind = node.kind();
    // Count lexical operators once, not both a statement and its keyword child.
    if is_operator(kind) && node.child_count() == 0 {
        operators.push(kind.to_string());
    } else if is_operand(kind) {
        operands.push(node.utf8_text(src).unwrap_or("").to_string());
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_halstead(&child, src, operators, operands);
    }
}

/// Computes the Maintainability Index for a function.
pub fn compute_maintainability_index(halstead_volume: f64, cc: usize, loc: usize) -> f64 {
    if loc == 0 {
        return 100.0;
    }
    let v = if halstead_volume > 0.0 {
        halstead_volume
    } else {
        1.0
    };
    let raw = 171.0 - 5.2 * v.ln() - 0.23 * cc as f64 - 16.2 * (loc as f64).ln();
    (raw * 100.0 / 171.0).clamp(0.0, 100.0)
}
