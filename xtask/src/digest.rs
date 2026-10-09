//! `xtask facts-digest`: one hash over everything the front end produces for a corpus, to
//! prove that a front-end optimisation changes no result (docs/benchmarks.md).
//!
//! Per file (fresh store and interner, so ids are deterministic): every fact column (in
//! `Debug` form, evidence hashes included), the rendered facts with resolved names,
//! the interned strings in id order, every metrics row, and the lexer's token streams in
//! parser mode and trivia mode plus its suppressions. File digests are combined in path
//! order. Not a hot path: it formats everything as text.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use pymapper_core::extract::facts::{FactStore, extract_with, render};
use pymapper_core::extract::{MetricsScratch, MetricsTable, MetricsVisitor};
use pymapper_core::intern::Interner;
use pymapper_core::lex::Lexer;
use pymapper_core::walk::{WalkOptions, walk};
use rayon::prelude::*;

#[derive(clap::Args)]
pub struct Args {
    /// Directories (or files) to digest; each gets its own line.
    pub dirs: Vec<PathBuf>,
    /// Also print one `HASH PATH` line per file (to find the file that changed).
    #[arg(long)]
    pub files: bool,
}

pub fn run(args: &Args) -> anyhow::Result<ExitCode> {
    for dir in &args.dirs {
        let w = walk(&[dir], &WalkOptions::everything());
        let mut paths: Vec<PathBuf> = w.files.iter().map(|p| p.to_path_buf()).collect();
        paths.sort();
        let digests: Vec<anyhow::Result<u64>> = paths.par_iter().map(|p| file_digest(p)).collect();
        let mut all = DefaultHasher::new();
        let mut bytes = 0u64;
        for (p, d) in paths.iter().zip(digests) {
            let d = d?;
            bytes += std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            if args.files {
                println!("{d:016x} {}", p.display());
            }
            p.hash(&mut all);
            d.hash(&mut all);
        }
        println!("{:016x} {} files {} bytes {}", all.finish(), paths.len(), bytes, dir.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// Digest of everything extracted from one file.
pub fn file_digest(path: &std::path::Path) -> anyhow::Result<u64> {
    let src = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(source_digest(&src))
}

/// Digest of everything extracted from `src`.
pub fn source_digest(src: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    let mut store = FactStore::default();
    let mut strs = Interner::new();
    let mut scratch = MetricsScratch::default();
    let mut metrics = MetricsTable::new();
    let m = MetricsVisitor::new(src, &mut scratch, &mut metrics);
    let (ff, _) = extract_with(src, &mut store, &mut strs, m);
    // Every fact column (not the store's private extractor scratch).
    let st = &store;
    format!(
        "{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}{:?}",
        st.defs, st.decorators, st.bases, st.params, st.imports, st.calls, st.bindings, st.decls, st.names,
        st.exports, st.evidence, st.diags, st.suppressions
    )
    .hash(&mut h);
    format!("{ff:?}").hash(&mut h);
    render(store.file(&ff), |s| strs.resolve(s)).hash(&mut h);
    for (sym, s) in strs.iter() {
        (sym.index(), s).hash(&mut h);
    }
    format!("{metrics:?}").hash(&mut h);
    for i in 0..metrics.len() {
        format!("{:?}", metrics.row(i)).hash(&mut h);
    }
    for trivia in [false, true] {
        let lx = if trivia { Lexer::with_trivia(src) } else { Lexer::new(src) };
        let (toks, sups) = lx.tokenize();
        for t in &toks {
            (t.kind as u8, t.start, t.len).hash(&mut h);
        }
        format!("{sups:?}").hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_deterministic_and_sees_every_layer() {
        let src = b"import os\n\ndef f(x):  # pymapper: ignore[PM401]\n    \"\"\"Doc.\"\"\"\n    return os.path.join(x, 'a.b')\n";
        let d = source_digest(src);
        assert_eq!(d, source_digest(src));
        // A fact change (call name), a position-only change, a metrics-only change (operand
        // count) and a trivia-only change (comment text) all change the digest.
        let variants: [&[u8]; 4] = [
            b"import os\n\ndef f(x):  # pymapper: ignore[PM401]\n    \"\"\"Doc.\"\"\"\n    return os.path.split(x, 'a.b')\n",
            b"import os\n\n\ndef f(x):  # pymapper: ignore[PM401]\n    \"\"\"Doc.\"\"\"\n    return os.path.join(x, 'a.b')\n",
            b"import os\n\ndef f(x):  # pymapper: ignore[PM401]\n    \"\"\"Doc.\"\"\"\n    return os.path.join(x, x)\n",
            b"import os\n\ndef f(x):  # pymapper: ignore[PM402]\n    \"\"\"Doc.\"\"\"\n    return os.path.join(x, 'a.b')\n",
        ];
        for v in variants {
            assert_ne!(source_digest(v), d, "{}", String::from_utf8_lossy(v));
        }
    }
}
