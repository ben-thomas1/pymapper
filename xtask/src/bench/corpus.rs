//! Benchmark corpora: the built-in set (tests, stdlib, venv) and ad-hoc `NAME=PATH` corpora.

use std::path::{Path, PathBuf};

use anyhow::bail;

pub const STDLIB_ENV: &str = "PYMAPPER_STDLIB";
pub const VENV_ENV: &str = "PYMAPPER_VENV";
pub const DEFAULT_STDLIB: &str =
    "/Users/bt386671/.local/share/uv/python/cpython-3.13.5-macos-aarch64-none/lib/python3.13";
pub const DEFAULT_VENV: &str = "/Users/bt386671/Code/gsk/aiml-lectio/.venv/lib/python3.12/site-packages";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Corpus {
    pub name: String,
    pub root: PathBuf,
    /// How to point the corpus elsewhere, shown when it is missing.
    pub hint: Option<&'static str>,
}

/// Built-in corpora in report order. `env` looks up an environment variable.
pub fn builtin(repo_root: &Path, env: impl Fn(&str) -> Option<String>) -> Vec<Corpus> {
    let from_env = |key: &str, default: &str| PathBuf::from(env(key).filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_owned()));
    vec![
        Corpus { name: "tests".to_owned(), root: repo_root.join("tests/corpus"), hint: None },
        Corpus { name: "stdlib".to_owned(), root: from_env(STDLIB_ENV, DEFAULT_STDLIB), hint: Some(STDLIB_ENV) },
        Corpus { name: "venv".to_owned(), root: from_env(VENV_ENV, DEFAULT_VENV), hint: Some(VENV_ENV) },
    ]
}

/// Resolves `--corpus` arguments: a built-in name, or `NAME=PATH`. No arguments selects all
/// built-ins. Duplicate names are rejected so results stay keyed uniquely.
pub fn select(args: &[String], builtins: &[Corpus]) -> anyhow::Result<Vec<Corpus>> {
    if args.is_empty() {
        return Ok(builtins.to_vec());
    }
    let mut out: Vec<Corpus> = Vec::with_capacity(args.len());
    for arg in args {
        let corpus = match arg.split_once('=') {
            Some((name, path)) => {
                if name.is_empty() || path.is_empty() {
                    bail!("bad corpus {arg:?}: expected NAME=PATH");
                }
                Corpus { name: name.to_owned(), root: PathBuf::from(path), hint: None }
            }
            None => match builtins.iter().find(|c| c.name == *arg) {
                Some(c) => c.clone(),
                None => {
                    let names: Vec<&str> = builtins.iter().map(|c| c.name.as_str()).collect();
                    bail!("unknown corpus {arg:?}: expected one of {} or NAME=PATH", names.join(", "));
                }
            },
        };
        if out.iter().any(|c| c.name == corpus.name) {
            bail!("corpus {:?} given twice", corpus.name);
        }
        out.push(corpus);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn builtins_use_defaults_without_env() {
        let b = builtin(Path::new("/repo"), no_env);
        let names: Vec<&str> = b.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["tests", "stdlib", "venv"]);
        assert_eq!(b[0].root, Path::new("/repo/tests/corpus"));
        assert_eq!(b[1].root, Path::new(DEFAULT_STDLIB));
        assert_eq!(b[2].root, Path::new(DEFAULT_VENV));
    }

    #[test]
    fn builtins_honour_env_but_not_empty_values() {
        let env = |k: &str| match k {
            STDLIB_ENV => Some("/opt/py/lib".to_owned()),
            VENV_ENV => Some(String::new()),
            _ => None,
        };
        let b = builtin(Path::new("/repo"), env);
        assert_eq!(b[1].root, Path::new("/opt/py/lib"));
        assert_eq!(b[2].root, Path::new(DEFAULT_VENV));
    }

    #[test]
    fn select_by_name_and_path() {
        let b = builtin(Path::new("/repo"), no_env);
        assert_eq!(select(&[], &b).unwrap(), b);
        let got = select(&["venv".to_owned(), "mine=/x/y=z".to_owned()], &b).unwrap();
        assert_eq!(got[0], b[2]);
        assert_eq!(got[1].name, "mine");
        assert_eq!(got[1].root, Path::new("/x/y=z"));
    }

    #[test]
    fn select_rejects_bad_input() {
        let b = builtin(Path::new("/repo"), no_env);
        assert!(select(&["nope".to_owned()], &b).is_err());
        assert!(select(&["=/x".to_owned()], &b).is_err());
        assert!(select(&["a=".to_owned()], &b).is_err());
        assert!(select(&["tests".to_owned(), "tests=/x".to_owned()], &b).is_err());
    }
}
