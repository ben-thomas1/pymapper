//! `xtask lex-dump`: prints the lexer's trivia-mode token stream in CPython `tokenize`
//! categories, for differential testing with `xtask/scripts/lexdiff.py`.
//!
//! Output, per file: `@ PATH`, then one `KIND START END` line per token (byte offsets).

use std::io::{BufRead, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use pymapper_core::lex::{Lexer, TokenKind};

#[derive(clap::Args)]
pub struct Args {
    /// Files to dump; read from stdin (one path per line) when none are given.
    files: Vec<PathBuf>,
}

/// CPython `tokenize` name of a token kind.
fn category(k: TokenKind) -> &'static str {
    use TokenKind as K;
    match k {
        K::EndOfFile => "ENDMARKER",
        K::Newline => "NEWLINE",
        K::NonLogicalNewline => "NL",
        K::Indent => "INDENT",
        K::Dedent => "DEDENT",
        K::Comment => "COMMENT",
        K::Name => "NAME",
        K::Int | K::Float | K::Imaginary => "NUMBER",
        K::String => "STRING",
        K::FStringStart => "FSTRING_START",
        K::FStringMiddle => "FSTRING_MIDDLE",
        K::FStringEnd => "FSTRING_END",
        K::TStringStart => "TSTRING_START",
        K::TStringMiddle => "TSTRING_MIDDLE",
        K::TStringEnd => "TSTRING_END",
        k if k.is_keyword() => "NAME",
        k if k.is_operator() => "OP",
        _ => "ERRORTOKEN",
    }
}

pub fn run(args: &Args) -> anyhow::Result<ExitCode> {
    let mut paths = args.files.clone();
    if paths.is_empty() {
        for line in std::io::stdin().lock().lines() {
            let line = line.context("reading paths from stdin")?;
            if !line.is_empty() {
                paths.push(PathBuf::from(line));
            }
        }
    }
    let mut out = BufWriter::new(std::io::stdout().lock());
    for path in &paths {
        let src = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        writeln!(out, "@ {}", path.display())?;
        for t in Lexer::with_trivia(&src) {
            writeln!(out, "{} {} {}", category(t.kind), t.start, t.end())?;
        }
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}
