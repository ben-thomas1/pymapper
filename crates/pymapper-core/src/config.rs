//! Configuration: `[tool.pymapper]` in `pyproject.toml`, or a standalone `pymapper.toml`
//! (design: Checks / CLI).
//!
//! Precedence, lowest to highest: built-in defaults, the discovered (or explicit) config file,
//! `--set KEY=VALUE` TOML fragments, typed CLI flags. Every source is validated strictly:
//! unknown keys, wrong types and out-of-range values are errors (with "did you mean"
//! suggestions), and all errors of a source are reported together with file:line:col.
//!
//! Config is read once per run, so this module optimises for clear errors over speed; the
//! pieces used on hot paths ([`EntrypointMatcher`], [`RuleConfig::is_enabled`]) are
//! allocation-free lookups.
//!
//! Every option is documented, with its default and performance cost, in
//! `pymapper.toml.template` at the repository root ([`TEMPLATE`]).

mod glob;
mod parse;
mod presets;
mod suggest;
mod write;

#[cfg(test)]
mod tests;

use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

pub use glob::{Glob, GlobError, GlobSet};
pub use presets::Preset;

/// The fully commented configuration template documenting every option.
pub const TEMPLATE: &str = include_str!("../../../pymapper.toml.template");

/// Standalone config file name (top-level keys, no `[tool.pymapper]` prefix).
pub const STANDALONE_FILE: &str = "pymapper.toml";
/// Project file name; options live under `[tool.pymapper]`.
pub const PYPROJECT_FILE: &str = "pyproject.toml";
/// Default cache directory, relative to the project root.
pub const DEFAULT_CACHE_DIR: &str = ".pymapper_cache";
/// Default `exclude` globs (gitignore syntax, relative to the project root).
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".bzr",
    ".direnv",
    ".eggs",
    ".git",
    ".hg",
    ".mypy_cache",
    ".nox",
    ".pants.d",
    ".pymapper_cache",
    ".pytest_cache",
    ".pytype",
    ".ruff_cache",
    ".svn",
    ".tox",
    ".venv",
    "__pycache__",
    "__pypackages__",
    "_build",
    "buck-out",
    "build",
    "dist",
    "node_modules",
    "site-packages",
    "venv",
];
/// Default minimum confidence (0-100) a finding needs to be reported by `check`.
pub const DEFAULT_MIN_CONFIDENCE: u8 = 80;
/// Largest accepted `threads` value.
pub const MAX_THREADS: u32 = 4096;

/// Generates a closed string-valued enum: `ALL`, `as_str`, `Display` and a `FromStr` whose
/// error suggests the closest valid name.
macro_rules! name_enum {
    ($(#[$meta:meta])* $name:ident, $what:literal, { $($(#[$vmeta:meta])* $variant:ident = $text:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &[$name] = &[$($name::$variant,)+];
            /// Every accepted spelling, in declaration order.
            pub const NAMES: &[&str] = &[$($text,)+];

            /// The config-file spelling of this value.
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text,)+
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::config::UnknownName;

            fn from_str(s: &str) -> Result<Self, $crate::config::UnknownName> {
                match s {
                    $($text => Ok($name::$variant),)+
                    _ => Err($crate::config::UnknownName::new($what, s, $name::NAMES)),
                }
            }
        }
    };
}
pub(crate) use name_enum;

name_enum!(
    /// How dead-code analysis treats the public API (design: dead code (e)).
    Mode, "mode", {
        /// Application if the project declares entry points (`[project.scripts]`,
        /// `[project.gui-scripts]`, `[project.entry-points]`, `entry_points.txt`) or has a
        /// `__main__` module / guard; library otherwise.
        Auto = "auto",
        /// Only code reachable from entry points is live.
        Application = "application",
        /// Public (non-underscore) names of public modules are roots too.
        Library = "library",
    }
);

name_enum!(
    /// Finding output format (design: Output).
    OutputFormat, "output format", {
        /// `path:line:col: CODE message`, one finding per line.
        Concise = "concise",
        /// One JSON document.
        Json = "json",
        /// SARIF 2.1.0.
        Sarif = "sarif",
        /// GitHub Actions workflow commands (`::error file=...`).
        Github = "github",
    }
);

name_enum!(
    /// Kind of an import contract (design: layering contracts).
    ContractKindName, "contract kind", {
        /// Ordered layers: a layer may import lower layers, never higher ones.
        Layers = "layers",
        /// `source` modules may not import `forbidden` modules.
        Forbidden = "forbidden",
        /// The listed modules may not import each other.
        Independence = "independence",
    }
);

/// Error for an unrecognised enum spelling; carries a suggestion when one is close.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown {what} `{value}` (expected one of: {})", .expected.join(", "))]
pub struct UnknownName {
    /// What was being parsed, e.g. "output format".
    pub what: &'static str,
    /// The rejected input.
    pub value: String,
    /// Every valid spelling.
    pub expected: &'static [&'static str],
    /// The closest valid spelling, if any is close enough.
    pub suggestion: Option<&'static str>,
}

impl UnknownName {
    fn new(what: &'static str, value: &str, expected: &'static [&'static str]) -> UnknownName {
        let suggestion = suggest::closest(value, expected.iter().copied());
        UnknownName { what, value: value.to_owned(), expected, suggestion }
    }
}

/// A Python `major.minor` version. Only Python 3 is supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PythonVersion {
    /// Always 3.
    pub major: u8,
    /// Minor version, e.g. 13.
    pub minor: u8,
}

impl fmt::Display for PythonVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Error for a malformed `python-version`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PythonVersionError {
    /// Not of the form `3.N`.
    #[error("invalid Python version `{0}` (expected `3.N`, e.g. `3.13`)")]
    Syntax(String),
    /// Python 2 (or any non-3 major) is unsupported.
    #[error("unsupported Python version `{0}`: only Python 3 is supported")]
    Unsupported(String),
}

impl FromStr for PythonVersion {
    type Err = PythonVersionError;

    fn from_str(s: &str) -> Result<Self, PythonVersionError> {
        let syntax = || PythonVersionError::Syntax(s.to_owned());
        let (major, minor) = s.split_once('.').ok_or_else(syntax)?;
        let all_digits = |t: &str| !t.is_empty() && t.len() <= 3 && t.bytes().all(|b| b.is_ascii_digit());
        if !all_digits(major) || !all_digits(minor) {
            return Err(syntax());
        }
        let major: u8 = major.parse().map_err(|_| syntax())?;
        let minor: u8 = minor.parse().map_err(|_| syntax())?;
        if major != 3 {
            return Err(PythonVersionError::Unsupported(s.to_owned()));
        }
        Ok(PythonVersion { major, minor })
    }
}

/// Selects rules by code prefix: `ALL`, `PM`, `PM1`, `PM10`, `PM101`.
///
/// Rule codes are `PM` followed by exactly three digits; the numeric part is passed to
/// [`RuleSelector::matches`] as a `u16` in `0..1000`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RuleSelector {
    /// Every rule.
    All,
    /// Codes whose first `digits` digits equal `value` (`digits == 0` selects every `PM` rule).
    Prefix {
        /// The prefix digits as a number (`PM01` -> 1).
        value: u16,
        /// Number of digits given (0..=3).
        digits: u8,
    },
}

const POW10: [u16; 4] = [1, 10, 100, 1000];

impl RuleSelector {
    /// Whether this selector selects rule `PM{code:03}`. `code` must be `< 1000`.
    #[inline]
    pub fn matches(self, code: u16) -> bool {
        debug_assert!(code < 1000, "rule code out of range: {code}");
        match self {
            RuleSelector::All => true,
            RuleSelector::Prefix { value, digits } => code / POW10[3 - digits as usize] == value,
        }
    }

    /// How specific the selector is: `ALL` 0, `PM` 1, `PM1` 2, ..., `PM101` 4. When a rule is
    /// matched by both a select and an ignore selector, the more specific one wins.
    #[inline]
    pub fn specificity(self) -> u8 {
        match self {
            RuleSelector::All => 0,
            RuleSelector::Prefix { digits, .. } => digits + 1,
        }
    }
}

impl fmt::Display for RuleSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RuleSelector::All => f.write_str("ALL"),
            RuleSelector::Prefix { digits: 0, .. } => f.write_str("PM"),
            RuleSelector::Prefix { value, digits } => write!(f, "PM{value:0width$}", width = digits as usize),
        }
    }
}

/// Error for a malformed rule selector.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid rule selector `{value}` (expected `ALL` or `PM` followed by up to 3 digits, e.g. `PM1`, `PM101`)")]
pub struct RuleSelectorError {
    /// The rejected input.
    pub value: String,
    /// A corrected spelling (e.g. upper-cased), if one is valid.
    pub suggestion: Option<String>,
}

impl FromStr for RuleSelector {
    type Err = RuleSelectorError;

    fn from_str(s: &str) -> Result<Self, RuleSelectorError> {
        if let Some(sel) = parse_selector_exact(s) {
            return Ok(sel);
        }
        let upper = s.trim().to_ascii_uppercase();
        let suggestion = parse_selector_exact(&upper).map(|_| upper);
        Err(RuleSelectorError { value: s.to_owned(), suggestion })
    }
}

fn parse_selector_exact(s: &str) -> Option<RuleSelector> {
    if s == "ALL" {
        return Some(RuleSelector::All);
    }
    let digits = s.strip_prefix("PM")?;
    if digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value = 0u16;
    for b in digits.bytes() {
        value = value * 10 + u16::from(b - b'0');
    }
    Some(RuleSelector::Prefix { value, digits: digits.len() as u8 })
}

/// Rule selection (design: Rules).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RuleConfig {
    /// Replaces the registry's default rule set when set.
    pub select: Option<Vec<RuleSelector>>,
    /// Added on top of `select` (or of the default set).
    pub extend_select: Vec<RuleSelector>,
    /// Rules to disable.
    pub ignore: Vec<RuleSelector>,
    /// Per-path ignores, sorted by pattern.
    pub per_file_ignores: Vec<PerFileIgnore>,
}

/// `per-file-ignores` entry: rules ignored in files matching a gitignore-style glob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PerFileIgnore {
    /// Glob (gitignore syntax) relative to the project root.
    pub pattern: String,
    /// Rules ignored in matching files.
    pub rules: Vec<RuleSelector>,
}

impl PerFileIgnore {
    /// Whether this entry ignores rule `PM{code:03}`.
    pub fn ignores(&self, code: u16) -> bool {
        self.rules.iter().any(|s| s.matches(code))
    }
}

/// Most specific selector in `list` matching `code`, as `specificity + 1` (0 = no match).
#[inline]
fn best_match(list: &[RuleSelector], code: u16) -> u8 {
    let mut best = 0;
    for sel in list {
        if sel.matches(code) {
            best = best.max(sel.specificity() + 1);
        }
    }
    best
}

impl RuleConfig {
    /// Whether rule `PM{code:03}` runs. `default_enabled` is the registry's default for rules
    /// not mentioned by `select`.
    ///
    /// Ruff semantics: the most specific matching selector wins; on a tie, `ignore` wins.
    /// Without `select`, default-enabled rules count as selected at the lowest specificity.
    pub fn is_enabled(&self, code: u16, default_enabled: bool) -> bool {
        let selected = match &self.select {
            Some(select) => best_match(select, code),
            None => u8::from(default_enabled),
        }
        .max(best_match(&self.extend_select, code));
        selected != 0 && selected > best_match(&self.ignore, code)
    }
}

/// Per-rule numeric thresholds, named after the metric (design: Metric definitions). Each rule
/// documents which threshold it reads; a finding is raised when a value is strictly beyond it.
///
/// Decision (for the rules steps): thresholds are keyed by metric name with a fixed key set, not
/// by rule code (ruff-like, and stable when rule codes are renumbered). Adding a threshold means
/// updating this struct, `parse::THRESHOLD_KEYS` / `apply_thresholds`, `write.rs`, the template
/// and the template tests together; the template tests fail if one is missed.
#[derive(Clone, Debug, PartialEq)]
pub struct Thresholds {
    /// McCabe cyclomatic complexity per function (ruff C901 semantics).
    pub max_mccabe: u32,
    /// Cognitive complexity per function (SonarSource).
    pub max_cognitive: u32,
    /// Block nesting depth per function.
    pub max_nesting: u32,
    /// Lines per function (inclusive span).
    pub max_function_loc: u32,
    /// Parameters per function (excluding `self` / `cls`).
    pub max_params: u32,
    /// Minimum maintainability index (0-100) per function.
    pub min_maintainability: f64,
    /// Lines per module.
    pub max_module_loc: u32,
    /// Distinct first-party modules imported by one module (efferent coupling).
    pub max_fan_out: u32,
    /// Distance from the main sequence `|A + I - 1|` (0-1) per module.
    pub max_distance: f64,
    /// Smallest total coupling `Ca + Ce` of a module judged by the main-sequence distance.
    pub distance_min_coupling: u32,
    /// Classes and functions per module (god module, with `max_module_coupling`).
    pub max_module_symbols: u32,
    /// Total coupling `Ca + Ce` per module (god module, with `max_module_symbols`).
    pub max_module_coupling: u32,
    /// Weighted methods per class: summed McCabe complexity (god class, with
    /// `max_class_fan_out`).
    pub max_class_wmc: u32,
    /// Distinct other classes a class's methods call (god class, with `max_class_wmc`).
    pub max_class_fan_out: u32,
    /// Minimum token run reported as a duplicate (needs `analysis.dupes`).
    pub dupes_min_tokens: u32,
    /// Minimum line count of a reported duplicate (needs `analysis.dupes`).
    pub dupes_min_lines: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            max_mccabe: 10,
            max_cognitive: 15,
            max_nesting: 4,
            max_function_loc: 100,
            max_params: 7,
            min_maintainability: 10.0,
            max_module_loc: 1000,
            max_fan_out: 30,
            max_distance: 0.7,
            distance_min_coupling: 10,
            max_module_symbols: 80,
            max_module_coupling: 25,
            max_class_wmc: 47,
            max_class_fan_out: 5,
            dupes_min_tokens: 50,
            dupes_min_lines: 6,
        }
    }
}

/// User-declared entrypoint rules (design: dead code (d)). Presets add to these; use
/// [`Config::entrypoint_matcher`] for the merged, compiled form.
///
/// Patterns are stored as [`Glob`]s, which can only be built from valid patterns, so code that
/// builds or edits a `Config` directly cannot make [`Config::entrypoint_matcher`] fail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntrypointConfig {
    /// Globs over a decorator's dotted name as written, without call arguments
    /// (`@app.route("/")` -> `app.route`).
    pub decorators: Vec<Glob>,
    /// Globs over fully qualified base classes (`django.db.models.Model`); subclasses (and their
    /// methods) are roots.
    pub bases: Vec<Glob>,
    /// Globs over symbol names: patterns without `.`/`:` match the simple name, others match the
    /// qualified id (`pkg.mod:Class.method`).
    pub names: Vec<Glob>,
    /// Treat `[project.scripts]`, `[project.gui-scripts]`, `[project.entry-points]` and
    /// `entry_points.txt` targets as roots.
    pub project_scripts: bool,
}

impl Default for EntrypointConfig {
    fn default() -> Self {
        EntrypointConfig { decorators: Vec::new(), bases: Vec::new(), names: Vec::new(), project_scripts: true }
    }
}

/// Analysis toggles; the expensive ones are off by default (design: Performance rules).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisConfig {
    /// Duplicate-code detection (expensive).
    pub dupes: bool,
    /// Exact transitive reachability sets instead of HyperLogLog estimates (expensive).
    pub exact_reachability: bool,
    /// Dead code: treat a name referenced anywhere (attribute names, identifier-like string
    /// literals) as possibly live, lowering confidence instead of reporting it (O(n)).
    pub name_fallback: bool,
    /// Call resolution: link a call on an unknown receiver to the only method with that name
    /// (confidence `possible`) (O(calls)).
    pub unique_method_fallback: bool,
    /// Count `if TYPE_CHECKING:` imports when finding import cycles.
    pub type_checking_cycles: bool,
    /// coverage.py JSON report used as dead-code evidence (P6).
    pub coverage: Option<PathBuf>,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        AnalysisConfig {
            dupes: false,
            exact_reachability: false,
            name_fallback: true,
            unique_method_fallback: true,
            type_checking_cycles: false,
            coverage: None,
        }
    }
}

/// An import contract (design: layering contracts).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contract {
    /// Unique name, used in findings.
    pub name: String,
    /// Kind-specific module lists.
    pub kind: ContractKind,
    /// Exempt `if TYPE_CHECKING:` imports.
    pub allow_type_checking: bool,
    /// Also check indirect (transitive) imports: O(V+E) per contract instead of O(E).
    pub transitive: bool,
    /// Import edges exempt from this contract.
    pub ignore_imports: Vec<ImportPattern>,
}

/// Kind-specific part of a [`Contract`]. Module names are dotted (`pkg.sub`) and cover their
/// submodules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContractKind {
    /// Highest layer first; a layer may import any lower layer, never a higher one.
    Layers {
        /// Each layer is one or more sibling modules.
        layers: Vec<Vec<String>>,
        /// When non-empty, `layers` are relative to each container (`container.layer`).
        containers: Vec<String>,
        /// Siblings within one layer may not import each other.
        independent_siblings: bool,
    },
    /// `source` modules may not import `forbidden` modules.
    Forbidden {
        /// Importing modules.
        source: Vec<String>,
        /// Modules they may not import.
        forbidden: Vec<String>,
    },
    /// None of `modules` may import another.
    Independence {
        /// Mutually independent modules.
        modules: Vec<String>,
    },
}

impl ContractKind {
    /// The `kind = "..."` spelling.
    pub fn name(&self) -> ContractKindName {
        match self {
            ContractKind::Layers { .. } => ContractKindName::Layers,
            ContractKind::Forbidden { .. } => ContractKindName::Forbidden,
            ContractKind::Independence { .. } => ContractKindName::Independence,
        }
    }
}

/// `importer -> imported` edge pattern; `*` matches one dotted segment, `**` any number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportPattern {
    /// Importing module pattern.
    pub importer: String,
    /// Imported module pattern.
    pub imported: String,
}

/// All options. Paths are absolute (resolved against the directory of the file that set them,
/// or the working directory for CLI flags) and lexically normalised; globs stay relative to the
/// project root.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Files and directories to analyse.
    pub paths: Vec<PathBuf>,
    /// Extra first-party import roots; empty = auto-detect (project root, `src/` layout).
    pub src: Vec<PathBuf>,
    /// Gitignore-style globs to skip; replaces [`DEFAULT_EXCLUDES`]. Only checked to be
    /// non-empty here: the walker's gitignore matcher owns the syntax and must report invalid
    /// patterns (as must the rules step for `per-file-ignores`).
    pub exclude: Vec<String>,
    /// Globs added to `exclude`.
    pub extend_exclude: Vec<String>,
    /// Honour `.gitignore` / `.ignore` / `.git/info/exclude`.
    pub respect_gitignore: bool,
    /// Analyse third-party packages from the virtualenv.
    pub include_deps: bool,
    /// Analyse the standard library.
    pub include_stdlib: bool,
    /// Virtualenv to use; `None` = `$VIRTUAL_ENV`, then `.venv` / `venv` under the root.
    pub venv: Option<PathBuf>,
    /// Target Python version; `None` = from the venv's `pyvenv.cfg`, else `requires-python`.
    pub python_version: Option<PythonVersion>,
    /// Use the on-disk cache.
    pub cache: bool,
    /// Cache directory.
    pub cache_dir: PathBuf,
    /// Worker threads; 0 = one per logical CPU.
    pub threads: u32,
    /// Library vs application dead-code mode.
    pub mode: Mode,
    /// Finding output format.
    pub output_format: OutputFormat,
    /// Minimum confidence (0-100) for `check` to report a finding.
    pub min_confidence: u8,
    /// Baseline file: findings recorded in it are not reported.
    pub baseline: Option<PathBuf>,
    /// Framework presets, deduplicated, in the order given.
    pub presets: Vec<Preset>,
    /// Rule selection.
    pub rules: RuleConfig,
    /// Per-rule thresholds.
    pub thresholds: Thresholds,
    /// Entrypoint rules.
    pub entrypoints: EntrypointConfig,
    /// Analysis toggles.
    pub analysis: AnalysisConfig,
    /// Import contracts.
    pub contracts: Vec<Contract>,
}

impl Config {
    /// Defaults for a project rooted at `root` (absolute).
    pub fn new(root: &Path) -> Config {
        Config {
            paths: vec![root.to_path_buf()],
            src: Vec::new(),
            exclude: DEFAULT_EXCLUDES.iter().map(|s| (*s).to_owned()).collect(),
            extend_exclude: Vec::new(),
            respect_gitignore: true,
            include_deps: false,
            include_stdlib: false,
            venv: None,
            python_version: None,
            cache: true,
            cache_dir: root.join(DEFAULT_CACHE_DIR),
            threads: 0,
            mode: Mode::Auto,
            output_format: OutputFormat::Concise,
            min_confidence: DEFAULT_MIN_CONFIDENCE,
            baseline: None,
            presets: Vec::new(),
            rules: RuleConfig::default(),
            thresholds: Thresholds::default(),
            entrypoints: EntrypointConfig::default(),
            analysis: AnalysisConfig::default(),
            contracts: Vec::new(),
        }
    }

    /// Every exclude glob: `exclude` followed by `extend-exclude`.
    pub fn all_excludes(&self) -> impl Iterator<Item = &str> {
        self.exclude.iter().chain(&self.extend_exclude).map(String::as_str)
    }

    /// User entrypoint rules merged with the enabled presets, compiled for matching. Infallible:
    /// user patterns are [`Glob`]s and preset patterns are static and tested.
    pub fn entrypoint_matcher(&self) -> EntrypointMatcher {
        let ep = &self.entrypoints;
        let mut decorators: Vec<Glob> = Vec::new();
        let mut bases: Vec<Glob> = Vec::new();
        let mut names: Vec<Glob> = Vec::new();
        for preset in &self.presets {
            let rules = preset.rules();
            decorators.extend(rules.decorators.iter().map(|p| presets::glob(p)));
            bases.extend(rules.bases.iter().map(|p| presets::glob(p)));
            names.extend(rules.names.iter().map(|p| presets::glob(p)));
        }
        let mut simple: Vec<&Glob> = Vec::new();
        let mut qualified: Vec<&Glob> = Vec::new();
        for g in names.iter().chain(&ep.names) {
            if is_qualified_pattern(g) {
                qualified.push(g);
            } else {
                simple.push(g);
            }
        }
        EntrypointMatcher {
            decorators: GlobSet::from_globs(decorators.iter().chain(&ep.decorators)),
            bases: GlobSet::from_globs(bases.iter().chain(&ep.bases)),
            names: GlobSet::from_globs(simple),
            qualnames: GlobSet::from_globs(qualified),
        }
    }

    /// Serialises to `pymapper.toml` syntax; parsing the output yields an equal `Config`.
    pub fn to_toml(&self) -> String {
        write::to_toml(self)
    }
}

/// Name patterns containing `.` or `:` match the qualified id, others the bare name.
fn is_qualified_pattern(g: &Glob) -> bool {
    g.as_str().contains(['.', ':'])
}

/// Compiled entrypoint rules: user rules plus presets. All lookups are allocation-free.
#[derive(Clone, Debug)]
pub struct EntrypointMatcher {
    decorators: GlobSet,
    bases: GlobSet,
    names: GlobSet,
    qualnames: GlobSet,
}

impl EntrypointMatcher {
    /// Whether a decorator (dotted name as written, no call arguments) marks a root.
    #[inline]
    pub fn decorator(&self, dotted: &[u8]) -> bool {
        self.decorators.is_match(dotted)
    }

    /// Whether subclasses of this fully qualified base class are roots.
    #[inline]
    pub fn base(&self, qualified: &[u8]) -> bool {
        self.bases.is_match(qualified)
    }

    /// Whether a symbol is a root by name: `simple` is its bare name, `qualified` its stable id
    /// (`pkg.mod:Class.method`). When [`has_qualified_names`](Self::has_qualified_names) is
    /// false, prefer [`simple_name`](Self::simple_name) and skip building the qualified id.
    #[inline]
    pub fn name(&self, simple: &[u8], qualified: &[u8]) -> bool {
        self.simple_name(simple) || self.qualified_name(qualified)
    }

    /// Whether a bare symbol name matches a name pattern without `.`/`:`.
    #[inline]
    pub fn simple_name(&self, simple: &[u8]) -> bool {
        self.names.is_match(simple)
    }

    /// Whether a qualified id (`pkg.mod:Class.method`) matches a name pattern with `.`/`:`.
    #[inline]
    pub fn qualified_name(&self, qualified: &[u8]) -> bool {
        self.qualnames.is_match(qualified)
    }

    /// Whether any name pattern matches qualified ids. When false, callers need not build
    /// per-symbol qualified ids for name matching.
    #[inline]
    pub fn has_qualified_names(&self) -> bool {
        !self.qualnames.is_empty()
    }

    /// Whether any base-class pattern is configured (callers can skip class hierarchy work).
    #[inline]
    pub fn has_bases(&self) -> bool {
        !self.bases.is_empty()
    }

    /// Whether no rule is configured at all (callers can skip matching).
    pub fn is_empty(&self) -> bool {
        self.decorators.is_empty() && self.bases.is_empty() && self.names.is_empty() && self.qualnames.is_empty()
    }
}

/// Which kind of file a config came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigFileKind {
    /// `pymapper.toml` (or any explicit non-pyproject file): top-level keys.
    Standalone,
    /// `pyproject.toml`: the `[tool.pymapper]` table.
    Pyproject,
}

/// Where the project is and which files configured it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    /// Project root: the config file's directory, else the nearest directory with a
    /// `pyproject.toml`, else the start directory. Globs are relative to it.
    pub root: PathBuf,
    /// The config file applied, if any.
    pub config_file: Option<(PathBuf, ConfigFileKind)>,
    /// `pyproject.toml` at the project root, if present (`[project]` metadata for entry points
    /// and dependency hygiene).
    pub pyproject: Option<PathBuf>,
}

/// Result of [`load`].
#[derive(Clone, Debug, PartialEq)]
pub struct Loaded {
    /// Project location and config provenance.
    pub project: Project,
    /// Effective options.
    pub config: Config,
    /// Non-fatal notes (e.g. a shadowed `[tool.pymapper]`), for the CLI to print.
    pub warnings: Vec<String>,
}

/// Command-line overrides, applied on top of the config file. Relative paths are resolved
/// against the working directory passed to [`load`].
#[derive(Clone, Debug, Default)]
pub struct Overrides {
    /// Use this config file instead of discovering one.
    pub config_file: Option<PathBuf>,
    /// Ignore all config files (defaults + overrides only).
    pub isolated: bool,
    /// Paths to analyse; when non-empty they replace `paths`, and the first one is where config
    /// discovery starts (else the working directory).
    pub paths: Vec<PathBuf>,
    /// TOML fragments in `pymapper.toml` syntax, e.g. `thresholds.max-mccabe = 12`; applied in
    /// order, before the typed flags below.
    pub set: Vec<String>,
    /// Replaces `rules.select`.
    pub select: Option<Vec<RuleSelector>>,
    /// Appended to `rules.extend-select`.
    pub extend_select: Vec<RuleSelector>,
    /// Appended to `rules.ignore`.
    pub ignore: Vec<RuleSelector>,
    /// Appended to `extend-exclude`.
    pub extend_exclude: Vec<String>,
    /// Appended to `presets`.
    pub presets: Vec<Preset>,
    /// Replaces `output-format`.
    pub output_format: Option<OutputFormat>,
    /// Replaces `min-confidence` (0-100; larger values are rejected by [`load`]).
    pub min_confidence: Option<u8>,
    /// Replaces `cache-dir`.
    pub cache_dir: Option<PathBuf>,
    /// `Some(false)` disables the cache.
    pub cache: Option<bool>,
    /// Replaces `include-deps`.
    pub include_deps: Option<bool>,
    /// Replaces `include-stdlib`.
    pub include_stdlib: Option<bool>,
    /// Replaces `venv`.
    pub venv: Option<PathBuf>,
    /// Replaces `mode`.
    pub mode: Option<Mode>,
    /// Replaces `threads` (0-4096; larger values are rejected by [`load`]).
    pub threads: Option<u32>,
}

/// One validation problem, located in its source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// File path, or a description of a command-line source.
    pub origin: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column (characters).
    pub col: u32,
    /// What is wrong.
    pub message: String,
    /// How to fix it (e.g. "did you mean `select`?").
    pub help: Option<String>,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}: {}", self.origin, self.line, self.col, self.message)?;
        if let Some(help) = &self.help {
            write!(f, "\n  help: {help}")?;
        }
        Ok(())
    }
}

/// Configuration error.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A config file could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Io {
        /// File that failed.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// TOML syntax or validation errors, sorted by source position.
    #[error("invalid configuration:\n{}", DiagList(diagnostics))]
    Invalid {
        /// Every problem found.
        diagnostics: Vec<Diagnostic>,
    },
    /// A typed [`Overrides`] field is out of range (the CLI validates its flags itself; this
    /// guards library callers).
    #[error("invalid override `{field}`: {message}")]
    Override {
        /// The [`Overrides`] field.
        field: &'static str,
        /// What is wrong.
        message: String,
    },
}

struct DiagList<'a>(&'a [Diagnostic]);

impl fmt::Display for DiagList<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, d) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            write!(f, "{d}")?;
        }
        Ok(())
    }
}

/// Discovers the config file, applies it and the overrides, and validates everything.
///
/// `cwd` must be absolute; it anchors relative override paths and is where discovery starts
/// when `overrides.paths` is empty. Discovery walks up from the start directory; in each
/// directory `pymapper.toml` wins over a `pyproject.toml` with a `[tool.pymapper]` table, and a
/// `pyproject.toml` without one is skipped.
pub fn load(cwd: &Path, overrides: &Overrides) -> Result<Loaded, ConfigError> {
    check_overrides(overrides)?;
    let mut warnings = Vec::new();
    let start = match overrides.paths.first() {
        Some(p) => normalize(&cwd.join(p)),
        None => normalize(cwd),
    };
    let start_dir = if start.is_file() { start.parent().map_or_else(|| start.clone(), Path::to_path_buf) } else { start };

    let found = if overrides.isolated {
        None
    } else if let Some(file) = &overrides.config_file {
        let file = normalize(&cwd.join(file));
        let kind = if file.file_name().is_some_and(|n| n == PYPROJECT_FILE) {
            ConfigFileKind::Pyproject
        } else {
            ConfigFileKind::Standalone
        };
        let text = read(&file)?;
        Some((file, kind, text))
    } else {
        discover(&start_dir, &mut warnings)?
    };

    let root = match &found {
        Some((file, _, _)) => file.parent().map_or_else(|| start_dir.clone(), Path::to_path_buf),
        None => nearest_pyproject_dir(&start_dir).unwrap_or_else(|| start_dir.clone()),
    };
    let mut config = Config::new(&root);
    let mut diagnostics = Vec::new();

    if let Some((file, kind, text)) = &found {
        let origin = file.display().to_string();
        let base = file.parent().unwrap_or(&root);
        let source = parse::Source { origin: &origin, text, base };
        match kind {
            ConfigFileKind::Standalone => parse::apply_document(&source, &mut config, &mut diagnostics),
            ConfigFileKind::Pyproject => {
                if !parse::apply_pyproject(&source, &mut config, &mut diagnostics) {
                    warnings.push(format!("{origin}: no [tool.pymapper] table; using defaults"));
                }
            }
        }
    }

    for (i, fragment) in overrides.set.iter().enumerate() {
        let origin = format!("--set #{} `{fragment}`", i + 1);
        let source = parse::Source { origin: &origin, text: fragment, base: cwd };
        parse::apply_document(&source, &mut config, &mut diagnostics);
    }

    if !diagnostics.is_empty() {
        return Err(ConfigError::Invalid { diagnostics });
    }

    apply_typed(&mut config, cwd, overrides);
    let pyproject = Some(root.join(PYPROJECT_FILE)).filter(|p| p.is_file());
    let project = Project { root, config_file: found.map(|(file, kind, _)| (file, kind)), pyproject };
    Ok(Loaded { project, config, warnings })
}

/// Parses and validates `text` as a standalone `pymapper.toml` on top of `Config::new(base)`.
/// Relative paths resolve against `base`. Mainly for tests and tooling.
pub fn parse_standalone(text: &str, origin: &str, base: &Path) -> Result<Config, ConfigError> {
    let mut config = Config::new(base);
    let mut diagnostics = Vec::new();
    parse::apply_document(&parse::Source { origin, text, base }, &mut config, &mut diagnostics);
    if diagnostics.is_empty() { Ok(config) } else { Err(ConfigError::Invalid { diagnostics }) }
}

/// Range checks for typed overrides, matching the config-file limits.
fn check_overrides(o: &Overrides) -> Result<(), ConfigError> {
    if let Some(v) = o.min_confidence
        && v > 100
    {
        return Err(ConfigError::Override { field: "min_confidence", message: format!("{v} is out of range (0..=100)") });
    }
    if let Some(v) = o.threads
        && v > MAX_THREADS
    {
        return Err(ConfigError::Override { field: "threads", message: format!("{v} is out of range (0..={MAX_THREADS})") });
    }
    Ok(())
}

fn apply_typed(config: &mut Config, cwd: &Path, o: &Overrides) {
    if !o.paths.is_empty() {
        config.paths = o.paths.iter().map(|p| normalize(&cwd.join(p))).collect();
    }
    if let Some(select) = &o.select {
        config.rules.select = Some(select.clone());
    }
    config.rules.extend_select.extend_from_slice(&o.extend_select);
    config.rules.ignore.extend_from_slice(&o.ignore);
    config.extend_exclude.extend(o.extend_exclude.iter().cloned());
    for &p in &o.presets {
        if !config.presets.contains(&p) {
            config.presets.push(p);
        }
    }
    if let Some(v) = o.output_format {
        config.output_format = v;
    }
    if let Some(v) = o.min_confidence {
        config.min_confidence = v;
    }
    if let Some(v) = &o.cache_dir {
        config.cache_dir = normalize(&cwd.join(v));
    }
    if let Some(v) = o.cache {
        config.cache = v;
    }
    if let Some(v) = o.include_deps {
        config.include_deps = v;
    }
    if let Some(v) = o.include_stdlib {
        config.include_stdlib = v;
    }
    if let Some(v) = &o.venv {
        config.venv = Some(normalize(&cwd.join(v)));
    }
    if let Some(v) = o.mode {
        config.mode = v;
    }
    if let Some(v) = o.threads {
        config.threads = v;
    }
}

fn read(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|source| ConfigError::Io { path: path.to_path_buf(), source })
}

type Found = (PathBuf, ConfigFileKind, String);

/// Walks up from `start` looking for a config file.
fn discover(start: &Path, warnings: &mut Vec<String>) -> Result<Option<Found>, ConfigError> {
    for dir in start.ancestors() {
        let standalone = dir.join(STANDALONE_FILE);
        let pyproject = dir.join(PYPROJECT_FILE);
        if standalone.is_file() {
            if pyproject.is_file() && pyproject_has_section(&read(&pyproject)?) {
                warnings.push(format!(
                    "{} takes precedence; ignoring [tool.pymapper] in {}",
                    standalone.display(),
                    pyproject.display()
                ));
            }
            let text = read(&standalone)?;
            return Ok(Some((standalone, ConfigFileKind::Standalone, text)));
        }
        if pyproject.is_file() {
            let text = read(&pyproject)?;
            if pyproject_has_section(&text) {
                return Ok(Some((pyproject, ConfigFileKind::Pyproject, text)));
            }
        }
    }
    Ok(None)
}

/// Whether a `pyproject.toml` has a `[tool.pymapper]` table. Unparsable files that mention
/// "pymapper" count as having one so that the syntax error is reported rather than skipped.
fn pyproject_has_section(text: &str) -> bool {
    // Cheap prefilter: most pyproject files never mention us.
    if !text.contains("pymapper") {
        return false;
    }
    parse::pyproject_has_section(text).unwrap_or(true)
}

fn nearest_pyproject_dir(start: &Path) -> Option<PathBuf> {
    start.ancestors().find(|d| d.join(PYPROJECT_FILE).is_file()).map(Path::to_path_buf)
}

/// Lexically normalises a path: drops `.` components and folds `..` into the preceding normal
/// component (no filesystem access, symlinks are not resolved).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
                // `/..` is `/`.
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}
