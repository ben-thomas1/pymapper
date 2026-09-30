use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use jwalk::WalkDir;

use crate::parser;
use crate::types;

/// Thread counts for the walk and parse stages.
pub struct ThreadCounts {
    pub walk: usize,
    pub parse: usize,
}

/// Progress messages emitted during module loading.
pub enum LoadProgress {
    /// Walk phase has started (file discovery).
    Walking,
    /// Emitted as files are discovered during the walk phase.
    WalkProgress { discovered: usize },
    /// Parse progress: `done` files parsed out of `total`.
    Parsing { done: usize, total: usize },
    /// A file was skipped with a warning message.
    Warning(String),
}

/// Resolves thread counts from raw values (0 = auto).
pub fn resolve_thread_counts(walk: usize, parse: usize) -> ThreadCounts {
    if walk == 0 && parse == 0 {
        return ThreadCounts { walk: 0, parse: 0 };
    }
    let total = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let walk = if walk == 0 && parse != 0 {
        total.saturating_sub(parse).max(1)
    } else {
        walk.max(1)
    };
    let parse = if parse == 0 {
        total.saturating_sub(walk).max(1)
    } else {
        parse.max(1)
    };
    ThreadCounts { walk, parse }
}

/// Parses all .py files from the given paths and merges them into a single ModuleInfo.
pub fn load_modules(paths: &[String]) -> Result<types::ModuleInfo, String> {
    load_modules_with_progress(paths, ThreadCounts { walk: 0, parse: 0 }, |_| {})
}

/// Parses all .py files with a progress callback invoked during walk and parse phases.
///
/// Walk and parse are sequential: all files are discovered first (with
/// `WalkProgress` updates), then parsed in parallel with a fixed total.
pub fn load_modules_with_progress(
    paths: &[String],
    thread_counts: ThreadCounts,
    on_progress: impl Fn(LoadProgress) + Send + Sync,
) -> Result<types::ModuleInfo, String> {
    let base = compute_base_path(paths);

    // --- Phase 1: Walk ---
    on_progress(LoadProgress::Walking);
    let py_files = walk_py_files_with_progress(paths, thread_counts.walk, &on_progress)?;

    if py_files.is_empty() {
        return Err("no .py files found".to_string());
    }

    // --- Phase 2: Parse ---
    let total = py_files.len();
    on_progress(LoadProgress::Parsing { done: 0, total });

    let parsed_count = AtomicUsize::new(0);
    let results: Mutex<Vec<Result<types::ModuleInfo, String>>> = Mutex::new(Vec::new());

    let parse_one = |path: &PathBuf| {
        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                results
                    .lock()
                    .unwrap()
                    .push(Err(format!("cannot read {}: {}", path.display(), e)));
                let done = parsed_count.fetch_add(1, Ordering::Relaxed) + 1;
                on_progress(LoadProgress::Parsing { done, total });
                return;
            }
        };

        let module_prefix = module_prefix_from_path(path, &base);
        let mut module = match parser::parse_python_checked(&source) {
            Ok(module) => module,
            Err(error) => {
                results
                    .lock()
                    .unwrap()
                    .push(Err(format!("{}: {}", path.display(), error)));
                return;
            }
        };
        module.module_prefix = module_prefix.clone();

        let source_file = path.display().to_string();
        for func in &mut module.functions {
            func.source_module = module_prefix.clone();
            func.source_file = source_file.clone();
        }
        for class in &mut module.classes {
            class.source_module = module_prefix.clone();
        }
        for imp in &mut module.imports {
            imp.owning_module = module_prefix.clone();
        }

        results.lock().unwrap().push(Ok(module));
        let done = parsed_count.fetch_add(1, Ordering::Relaxed) + 1;
        on_progress(LoadProgress::Parsing { done, total });
    };

    let work = || {
        rayon::scope(|s| {
            for path in &py_files {
                let path = path.clone();
                s.spawn(move |_| parse_one(&path));
            }
        });
    };

    if thread_counts.parse > 0 {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(thread_counts.parse)
            .build()
            .map_err(|e| format!("failed to build parse thread pool: {}", e))?;
        pool.install(work);
    } else {
        work();
    }

    let mut modules = results
        .into_inner()
        .unwrap()
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    modules.sort_by(|a, b| a.module_prefix.cmp(&b.module_prefix));

    let mut combined = types::ModuleInfo::default();
    for module in modules {
        combined.merge(module);
    }

    Ok(combined)
}

/// Discover inputs strictly, without traversing directory symlinks or build environments.
fn walk_py_files_with_progress(
    paths: &[String],
    walk_threads: usize,
    on_progress: &(impl Fn(LoadProgress) + Send + Sync),
) -> Result<Vec<PathBuf>, String> {
    let mut result = Vec::new();
    for p in paths {
        let path = fs::canonicalize(p).map_err(|e| format!("cannot access {}: {}", p, e))?;
        if path
            .to_str()
            .is_none_or(|p| p.chars().any(char::is_control))
        {
            return Err("input paths must be UTF-8 without control characters".to_string());
        }
        if path.is_file() {
            if path.extension().is_some_and(|e| e == "py") {
                result.push(path);
            } else {
                return Err(format!("not a .py file: {}", path.display()));
            }
        } else if path.is_dir() {
            let walker = WalkDir::new(&path)
                .skip_hidden(true)
                .parallelism(jwalk::Parallelism::RayonNewPool(walk_threads.max(1)))
                .process_read_dir(|_, _, _, children| {
                    for entry in children.iter_mut().flatten() {
                        if [
                            "__pycache__",
                            "venv",
                            "node_modules",
                            "target",
                            "build",
                            "dist",
                            "gitbackup",
                        ]
                        .iter()
                        .any(|name| entry.file_name == *name)
                        {
                            entry.read_children_path = None;
                        }
                    }
                });
            for entry in walker {
                let entry = entry.map_err(|e| format!("cannot walk {}: {}", path.display(), e))?;
                if let Some(ref error) = entry.read_children_error {
                    return Err(format!(
                        "cannot read directory {}: {}",
                        entry.path().display(),
                        error
                    ));
                }
                if entry.file_type().is_file()
                    && entry.path().extension().is_some_and(|e| e == "py")
                {
                    result.push(entry.path());
                    on_progress(LoadProgress::WalkProgress {
                        discovered: result.len(),
                    });
                }
            }
        } else {
            return Err(format!(
                "not a regular file or directory: {}",
                path.display()
            ));
        }
    }
    result.sort();
    result.dedup();
    Ok(result)
}

/// Fallible file discovery for applications that must not silently omit inputs.
pub fn try_collect_py_files(paths: &[String]) -> Result<Vec<PathBuf>, String> {
    walk_py_files_with_progress(paths, 2, &|_| {})
}

fn walk_py_files(paths: &[String], walk_threads: usize) -> Vec<PathBuf> {
    walk_py_files_with_progress(paths, walk_threads, &|_| {}).unwrap_or_else(|error| {
        eprintln!("warning: {}", error);
        Vec::new()
    })
}

/// Computes a common base directory from CLI paths to strip from module prefixes.
pub fn compute_base_path(paths: &[String]) -> PathBuf {
    let resolved: Vec<PathBuf> = paths
        .iter()
        .map(|p| {
            let resolved = fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
            let path = resolved.as_path();
            if path.is_file() {
                path.parent().unwrap_or(path).to_path_buf()
            } else {
                path.to_path_buf()
            }
        })
        .collect();

    if resolved.is_empty() {
        return PathBuf::new();
    }

    let mut base = resolved[0].clone();
    for p in &resolved[1..] {
        base = common_prefix(&base, p);
    }
    base
}

/// Collects all `.py` files from the given paths (files added directly, directories walked recursively).
pub fn collect_py_files(paths: &[String]) -> Vec<PathBuf> {
    walk_py_files(paths, 0)
}

/// Recursively walks a directory collecting all `.py` files.
pub fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut files = walk_py_files(&[dir.display().to_string()], 0);
    out.append(&mut files);
}

/// Computes a dotted module prefix from a file path relative to a base directory.
pub fn module_prefix_from_path(path: &Path, base: &Path) -> String {
    let relative = path.strip_prefix(base).unwrap_or(path);
    let stem = relative.with_extension("");
    let prefix = stem
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(".");
    if prefix == "__init__" {
        String::new()
    } else {
        prefix
            .strip_suffix(".__init__")
            .unwrap_or(&prefix)
            .to_string()
    }
}

/// Returns the longest common path prefix of two paths.
fn common_prefix(a: &Path, b: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for (ca, cb) in a.components().zip(b.components()) {
        if ca == cb {
            result.push(ca);
        } else {
            break;
        }
    }
    result
}
