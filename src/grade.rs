use std::collections::HashMap;

use serde::Serialize;

use crate::dead_code;
use crate::metrics::GraphMetrics;
use crate::report;
use crate::types::{ModuleInfo, OutputFormat};

#[derive(Serialize)]
struct DimensionScore {
    dimension: String,
    grade: String,
    score: u32,
    detail: String,
}

#[derive(Serialize)]
struct ImprovementTarget {
    function: String,
    reason: String,
}

#[derive(Serialize)]
struct Scorecard {
    grade: String,
    score: u32,
    dimensions: Vec<DimensionScore>,
    top_targets: Vec<ImprovementTarget>,
}

fn letter_grade(score: u32) -> &'static str {
    match score {
        90..=100 => "A",
        80..=89 => "B",
        70..=79 => "C",
        60..=69 => "D",
        _ => "F",
    }
}

/// Computes a score for a dimension where lower values are better using per-function absolute scoring.
fn score_lower_is_better(values: &[f64], threshold_low: f64, threshold_high: f64) -> u32 {
    debug_assert!(!values.is_empty());
    let total: f64 = values
        .iter()
        .map(|&v| {
            if v <= threshold_low {
                100.0
            } else if v >= threshold_high {
                0.0
            } else {
                100.0 * (1.0 - (v - threshold_low) / (threshold_high - threshold_low))
            }
        })
        .sum();
    (total / values.len() as f64).clamp(0.0, 100.0) as u32
}

/// Computes a score where higher values are better (like MI) using per-function absolute scoring.
fn score_higher_is_better(values: &[f64], threshold_low: f64, threshold_mid: f64) -> u32 {
    debug_assert!(!values.is_empty());
    let total: f64 = values
        .iter()
        .map(|&v| {
            if v >= threshold_mid {
                100.0
            } else if v <= threshold_low {
                0.0
            } else {
                100.0 * (v - threshold_low) / (threshold_mid - threshold_low)
            }
        })
        .sum();
    (total / values.len() as f64).clamp(0.0, 100.0) as u32
}

/// Prints the grade/scorecard.
pub fn print_grade(
    module: &ModuleInfo,
    metrics: &HashMap<String, GraphMetrics>,
    format: OutputFormat,
    top_n: usize,
) {
    let funcs = &module.functions;
    if funcs.is_empty() {
        if matches!(format, OutputFormat::Json) {
            println!(
                "{}",
                serde_json::json!({"grade": null, "score": null, "dimensions": [], "top_targets": []})
            );
        } else {
            println!("No functions found.");
        }
        return;
    }

    // Gather metrics
    let ccs: Vec<f64> = funcs.iter().map(|f| f.complexity as f64).collect();
    let cognitives: Vec<f64> = funcs
        .iter()
        .map(|f| f.cognitive_complexity as f64)
        .collect();
    let nestings: Vec<f64> = funcs.iter().map(|f| f.max_nesting as f64).collect();
    let locs: Vec<f64> = funcs.iter().map(|f| f.line_count as f64).collect();
    let mis: Vec<f64> = funcs.iter().map(|f| f.maintainability_index).collect();

    let dead = dead_code::find_dead_code(funcs, &module.classes, metrics);
    let dead_pct = dead.len() as f64 / funcs.len() as f64 * 100.0;

    // Score each dimension
    let cc_score = score_lower_is_better(&ccs, 5.0, 10.0);
    let cc_above = ccs.iter().filter(|&&v| v > 5.0).count();

    let cog_score = score_lower_is_better(&cognitives, 8.0, 15.0);
    let cog_above = cognitives.iter().filter(|&&v| v > 8.0).count();

    let nesting_score = score_lower_is_better(&nestings, 3.0, 5.0);
    let nesting_above = nestings.iter().filter(|&&v| v > 3.0).count();

    let size_score = score_lower_is_better(&locs, 50.0, 100.0);
    let size_above = locs.iter().filter(|&&v| v > 50.0).count();

    let dead_score = (100.0 - dead_pct * 2.0).clamp(0.0, 100.0) as u32;

    let mi_score = score_higher_is_better(&mis, 20.0, 50.0);
    let mi_below = mis.iter().filter(|&&v| v < 50.0).count();

    // Compute coupling score from module summary
    let summary_rows = report::build_module_summary(module, metrics, None);
    let instabilities: Vec<f64> = summary_rows.iter().map(|r| r.instability).collect();
    let extreme = instabilities
        .iter()
        .filter(|&&i| i > 0.8 || (i < 0.2 && i > 0.0))
        .count();
    let ratio = extreme as f64 / instabilities.len() as f64;
    let coupling_score = (100.0 - ratio * 100.0).clamp(0.0, 100.0) as u32;

    let dimensions = vec![
        DimensionScore {
            dimension: "Complexity".to_string(),
            grade: letter_grade(cc_score).to_string(),
            score: cc_score,
            detail: format!(
                "avg McC {:.1}, {} functions above threshold",
                ccs.iter().sum::<f64>() / ccs.len() as f64,
                cc_above
            ),
        },
        DimensionScore {
            dimension: "Cognitive Load".to_string(),
            grade: letter_grade(cog_score).to_string(),
            score: cog_score,
            detail: format!("{} functions with cognitive > 8", cog_above),
        },
        DimensionScore {
            dimension: "Maintainability".to_string(),
            grade: letter_grade(mi_score).to_string(),
            score: mi_score,
            detail: format!("{} functions with MI < 50", mi_below),
        },
        DimensionScore {
            dimension: "Dead Code".to_string(),
            grade: letter_grade(dead_score).to_string(),
            score: dead_score,
            detail: format!(
                "{:.0}% potentially dead ({} functions)",
                dead_pct,
                dead.len()
            ),
        },
        DimensionScore {
            dimension: "Nesting".to_string(),
            grade: letter_grade(nesting_score).to_string(),
            score: nesting_score,
            detail: format!("{} functions with nesting > 3", nesting_above),
        },
        DimensionScore {
            dimension: "Function Size".to_string(),
            grade: letter_grade(size_score).to_string(),
            score: size_score,
            detail: format!("{} functions exceed 50 LOC", size_above),
        },
        DimensionScore {
            dimension: "Coupling".to_string(),
            grade: letter_grade(coupling_score).to_string(),
            score: coupling_score,
            detail: format!("{} modules analyzed", summary_rows.len()),
        },
    ];

    // Weighted average
    let weights = [3.0, 2.0, 2.0, 1.5, 1.0, 1.0, 1.5]; // complexity, cognitive, MI, dead, nesting, size, coupling
    let weighted_sum: f64 = dimensions
        .iter()
        .zip(weights.iter())
        .map(|(d, w)| d.score as f64 * w)
        .sum();
    let total_weight: f64 = weights.iter().sum();
    let overall_score = (weighted_sum / total_weight).round() as u32;
    let overall_grade = letter_grade(overall_score);

    // Top improvement targets
    let mut targets: Vec<(String, f64, String)> = funcs
        .iter()
        .map(|f| {
            let impact = f.complexity as f64 * 2.0
                + f.cognitive_complexity as f64
                + f.max_nesting as f64 * 3.0
                + (100.0 - f.maintainability_index).max(0.0) * 0.5;
            let reason = format!(
                "cc={}, cognitive={}, nesting={}, MI={:.0}",
                f.complexity, f.cognitive_complexity, f.max_nesting, f.maintainability_index
            );
            (f.qualified_name(), impact, reason)
        })
        .collect();
    targets.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    targets.truncate(top_n);

    let top_targets: Vec<ImprovementTarget> = targets
        .into_iter()
        .map(|(name, _, reason)| ImprovementTarget {
            function: name,
            reason,
        })
        .collect();

    let scorecard = Scorecard {
        grade: overall_grade.to_string(),
        score: overall_score,
        dimensions,
        top_targets,
    };

    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&scorecard).unwrap()),
        _ => print_scorecard_table(&scorecard),
    }
}

fn print_scorecard_table(sc: &Scorecard) {
    println!("Package Grade: {} ({}/100)\n", sc.grade, sc.score);

    let max_dim = sc
        .dimensions
        .iter()
        .map(|d| d.dimension.len())
        .max()
        .unwrap_or(0);
    for d in &sc.dimensions {
        println!(
            "  {:<width$}  {}  ({:>3}/100)  {}",
            d.dimension,
            d.grade,
            d.score,
            d.detail,
            width = max_dim
        );
    }

    if !sc.top_targets.is_empty() {
        println!("\nTop {} Improvement Targets:", sc.top_targets.len());
        for (i, t) in sc.top_targets.iter().enumerate() {
            println!("  {}. {} — {}", i + 1, t.function, t.reason);
        }
    }
}
