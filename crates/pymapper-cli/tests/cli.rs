//! Black-box tests of the `pymapper` binary skeleton.

use std::process::{Command, Output};

const SUBCOMMANDS: [&str; 8] = ["check", "callers", "callees", "impact", "path", "deps", "cycles", "summary"];

fn pymapper(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pymapper"))
        .args(args)
        .output()
        .expect("failed to spawn pymapper binary")
}

#[test]
fn unimplemented_subcommands_are_stubs_that_exit_2() {
    // `check` is implemented (see tests/check.rs).
    for name in SUBCOMMANDS.into_iter().filter(|&n| n != "check") {
        let out = pymapper(&[name]);
        assert_eq!(out.status.code(), Some(2), "{name}: wrong exit code");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("not implemented"), "{name}: stderr was {stderr:?}");
        assert!(stderr.contains(name), "{name}: stderr was {stderr:?}");
        assert!(out.stdout.is_empty(), "{name}: stdout must stay clean");
    }
}

#[test]
fn help_lists_only_implemented_subcommands() {
    let out = pymapper(&["--help"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let commands = stdout.split("Commands:").nth(1).expect("commands section");
    let commands = commands.split("Options:").next().expect("commands");
    let listed: Vec<&str> = commands.lines().filter_map(|l| l.split_whitespace().next()).collect();
    assert_eq!(listed, ["check", "config", "help"], "{stdout}");
}

#[test]
fn unimplemented_subcommands_accept_config_flags() {
    for name in SUBCOMMANDS.into_iter().filter(|&n| n != "check") {
        let out = pymapper(&[name, "some.symbol", "--isolated", "--deps", "--select", "PM1"]);
        assert_eq!(out.status.code(), Some(2), "{name}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("not implemented"), "{name}");
    }
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
