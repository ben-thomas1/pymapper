use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub report: ReportConfig,
    pub dead_code: DeadCodeConfig,
    pub graph: GraphConfig,
    pub threads: ThreadsConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ThreadsConfig {
    pub walk: Option<usize>,
    pub parse: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ReportConfig {
    pub fail_above: Option<String>,
    pub min_cc: Option<usize>,
    pub top: Option<usize>,
    pub format: Option<String>,
    pub sort: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct DeadCodeConfig {
    pub exclude_decorated: Vec<String>,
    pub exclude_overrides: bool,
    pub profile: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct GraphConfig {
    pub no_externals: bool,
    pub splines: Option<String>,
}

/// Searches for `.pymapper.toml` walking up from cwd, then checks `pyproject.toml` `[tool.pymapper]`.
pub fn load_config() -> Result<Config, String> {
    if let Some(path) = find_config_file(".pymapper.toml") {
        let contents = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
        return toml::from_str(&contents)
            .map_err(|e| format!("invalid configuration {}: {}", path.display(), e));
    }

    if let Some(path) = find_config_file("pyproject.toml") {
        let contents = fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
        let pyproject = toml::from_str::<PyProject>(&contents)
            .map_err(|e| format!("invalid configuration {}: {}", path.display(), e))?;
        if let Some(config) = pyproject.tool.and_then(|tool| tool.pymapper) {
            return Ok(config);
        }
    }

    Ok(Config::default())
}

#[derive(Deserialize)]
struct PyProject {
    tool: Option<PyProjectTool>,
}

#[derive(Deserialize)]
struct PyProjectTool {
    pymapper: Option<Config>,
}

fn find_config_file(name: &str) -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}
