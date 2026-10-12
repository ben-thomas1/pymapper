//! Black-box tests of the `pymapper` binary skeleton.

use std::process::{Command, Output};

const SUBCOMMANDS: [&str; 11] = ["check", "callers", "callees", "impact", "path", "imports", "deps", "explain", "cycles", "summary", "config"];

fn pymapper(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pymapper"))
        .args(args)
        .output()
        .expect("failed to spawn pymapper binary")
}

#[test]
fn help_lists_every_subcommand() {
    let out = pymapper(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let commands = stdout.split("Commands:").nth(1).expect("commands section");
    let commands = commands.split("Options:").next().expect("commands");
    let listed: Vec<&str> = commands.lines().filter_map(|l| l.split_whitespace().next()).collect();
    let mut want = SUBCOMMANDS.to_vec();
    want.push("help");
    assert_eq!(listed, want, "{stdout}");
}

#[test]
fn query_subcommands_need_their_symbols() {
    for name in ["callers", "callees", "impact", "imports", "path"] {
        let out = pymapper(&[name]);
        assert_eq!(out.status.code(), Some(2), "{name}: missing symbol is a usage error");
        assert!(out.stdout.is_empty(), "{name}: stdout must stay clean");
    }
    assert_eq!(pymapper(&["path", "a"]).status.code(), Some(2), "path needs two symbols");
    assert_eq!(pymapper(&["cycles", "--calls"]).status.code(), Some(2), "--calls needs --symbol");
    assert_eq!(pymapper(&["callers", "x", "--edges", "sure"]).status.code(), Some(2), "bad --edges");
    assert_eq!(pymapper(&["callers", "x", "--depth", "0"]).status.code(), Some(2), "depth starts at 1");
}

#[test]
fn subcommand_help_exits_0() {
    for name in SUBCOMMANDS {
        let out = pymapper(&[name, "--help"]);
        assert_eq!(out.status.code(), Some(0), "{name} --help");
    }
}

#[test]
fn version_exits_0() {
    let out = pymapper(&["--version"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("pymapper "));
}

#[test]
fn usage_errors_exit_2() {
    assert_eq!(pymapper(&[]).status.code(), Some(2), "missing subcommand");
    assert_eq!(pymapper(&["frobnicate"]).status.code(), Some(2), "unknown subcommand");
    assert_eq!(pymapper(&["check", "--no-such-flag"]).status.code(), Some(2), "unknown flag");
}
