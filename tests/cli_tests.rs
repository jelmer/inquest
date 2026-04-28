//! Tests for top-level CLI behavior (subprocess-based).

use std::process::Command;
use tempfile::TempDir;

fn inq_bin() -> &'static str {
    env!("CARGO_BIN_EXE_inq")
}

#[test]
fn no_subcommand_runs_with_auto() {
    // With no subcommand, `inq` should behave like `inq run --auto`. In an empty
    // directory with no recognizable project, that means the auto-detection
    // error path fires (rather than clap's "missing subcommand" usage error or
    // the plain `inq run` "repository not found" error).
    let temp = TempDir::new().unwrap();

    let no_arg = Command::new(inq_bin())
        .arg("-C")
        .arg(temp.path())
        .output()
        .expect("run inq");

    let auto_arg = Command::new(inq_bin())
        .arg("-C")
        .arg(temp.path())
        .arg("run")
        .arg("--auto")
        .output()
        .expect("run inq run --auto");

    assert_eq!(no_arg.status.code(), auto_arg.status.code());
    assert_eq!(no_arg.stdout, auto_arg.stdout);
    assert_eq!(no_arg.stderr, auto_arg.stderr);

    let stderr = String::from_utf8_lossy(&no_arg.stderr);
    assert!(
        stderr.contains("Could not detect project type"),
        "expected auto-detect failure, got stderr: {stderr}"
    );
}

#[test]
fn no_subcommand_differs_from_plain_run() {
    // Sanity check: `inq` (no args) must NOT be equivalent to `inq run` — it
    // should imply --auto. With no inquest.toml present, plain `inq run` errors
    // out about a missing repository instead of attempting auto-detection.
    let temp = TempDir::new().unwrap();

    let no_arg = Command::new(inq_bin())
        .arg("-C")
        .arg(temp.path())
        .output()
        .expect("run inq");
    let plain_run = Command::new(inq_bin())
        .arg("-C")
        .arg(temp.path())
        .arg("run")
        .output()
        .expect("run inq run");

    assert_ne!(no_arg.stderr, plain_run.stderr);
}

#[test]
fn run_help_advertises_starting_with_flag() {
    let out = Command::new(inq_bin())
        .arg("run")
        .arg("--help")
        .output()
        .expect("run inq run --help");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--starting-with") && stdout.contains("-s"),
        "expected --starting-with/-s in help, got:\n{stdout}"
    );
}

#[test]
fn bisect_help_shows_good_and_bad_overrides() {
    let out = Command::new(inq_bin())
        .arg("bisect")
        .arg("--help")
        .output()
        .expect("run inq bisect --help");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("--good"),
        "expected --good in help: {stdout}"
    );
    assert!(stdout.contains("--bad"), "expected --bad in help: {stdout}");
    assert!(
        stdout.contains("<TEST>"),
        "expected positional TEST: {stdout}"
    );
}
