//! e2e: the command-line surface itself — what `--help` advertises, what `-V` prints, and
//! how a command line monosplice cannot parse ends.
//!
//! Black-box like every other e2e file: this is the contract a user (and a shell completion
//! script) sees, so it is asserted through the built binary and nothing else.

mod common;

use common::{run_monosplice, sandbox};

/// Every command in the surface, in the order `--help` lists them.
const COMMANDS: [&str; 11] = [
    "init",
    "status",
    "push",
    "pull",
    "sync",
    "tag",
    "attach",
    "detach",
    "doctor",
    "update",
    "completion",
];

#[test]
fn help_lists_every_command() {
    let sb = sandbox();
    let res = run_monosplice(sb.path(), &["--help"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);

    for command in COMMANDS {
        assert!(
            res.stdout.contains(command),
            "`--help` must list `{command}`, got:\n{}",
            res.stdout
        );
    }
    assert!(
        res.stdout.contains("monosplice <COMMAND>"),
        "`--help` must show the flat command usage, got:\n{}",
        res.stdout
    );
}

#[test]
fn every_command_has_its_own_help() {
    let sb = sandbox();
    for command in COMMANDS {
        let res = run_monosplice(sb.path(), &[command, "--help"]);
        assert_eq!(
            res.exit_code, 0,
            "`{command} --help` failed: {}",
            res.stderr
        );
        assert!(
            res.stdout.contains(&format!("monosplice {command}")),
            "`{command} --help` must show its usage, got:\n{}",
            res.stdout
        );
    }
}

#[test]
fn version_prints_the_crate_version() {
    let sb = sandbox();
    let res = run_monosplice(sb.path(), &["-V"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert_eq!(res.stdout.trim(), "monosplice 1.0.1");

    let long = run_monosplice(sb.path(), &["--version"]);
    assert_eq!(long.stdout.trim(), "monosplice 1.0.1");
}

#[test]
fn an_unknown_command_is_a_usage_error() {
    let sb = sandbox();
    let res = run_monosplice(sb.path(), &["nope"]);
    assert_eq!(res.exit_code, 2, "stdout: {}", res.stdout);
    assert!(
        res.stderr.contains("nope"),
        "the error must name what was typed, got:\n{}",
        res.stderr
    );

    // An unknown flag on a real command is the same kind of failure.
    let flag = run_monosplice(sb.path(), &["status", "--nope"]);
    assert_eq!(flag.exit_code, 2, "stdout: {}", flag.stdout);
}

#[test]
fn no_command_at_all_prints_help_and_fails() {
    let sb = sandbox();
    let res = run_monosplice(sb.path(), &[]);
    assert_eq!(res.exit_code, 2, "stdout: {}", res.stdout);
    assert!(
        res.stderr.contains("Usage: monosplice"),
        "bare `monosplice` must show usage, got:\n{}",
        res.stderr
    );
}

#[test]
fn push_help_shows_the_whole_flag_surface() {
    let sb = sandbox();
    let res = run_monosplice(sb.path(), &["push", "--help"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);

    for flag in ["--yes", "--export-history", "--dry-run"] {
        assert!(
            res.stdout.contains(flag),
            "`push --help` must document {flag}, got:\n{}",
            res.stdout
        );
    }
    assert!(
        res.stdout.contains("-y"),
        "--yes keeps its short form, got:\n{}",
        res.stdout
    );
    // The oclif `static examples` render with the real binary name.
    assert!(
        res.stdout
            .contains("monosplice push core --yes --export-history"),
        "`push --help` must carry its examples, got:\n{}",
        res.stdout
    );
}

#[test]
fn completion_prints_a_script_for_each_supported_shell() {
    let sb = sandbox();
    for shell in ["bash", "zsh", "fish"] {
        let res = run_monosplice(sb.path(), &["completion", shell]);
        assert_eq!(
            res.exit_code, 0,
            "`completion {shell}` failed: {}",
            res.stderr
        );
        assert!(
            res.stdout.contains("monosplice"),
            "`completion {shell}` must name the binary, got:\n{}",
            res.stdout
        );
    }

    let bad = run_monosplice(sb.path(), &["completion", "clam"]);
    assert_eq!(bad.exit_code, 2, "stdout: {}", bad.stdout);
}
