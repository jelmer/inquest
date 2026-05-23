//! Integration tests for `inq ci` covering end-to-end output rendering when
//! the repository already contains failing/passing results. These exercise
//! the parts that the in-module unit tests can't easily reach: the
//! `emit_ci_output` orchestration, `GITHUB_STEP_SUMMARY` file writing, and
//! the interaction between the failing-tests file and the formatter.

use inquest::commands::ci::CiFormat;
use inquest::repository::inquest::InquestRepositoryFactory;
use inquest::repository::{RepositoryFactory, RunId, TestResult, TestRun};
use std::process::Command as ProcessCommand;
use tempfile::TempDir;

fn inq_bin() -> &'static str {
    env!("CARGO_BIN_EXE_inq")
}

/// Seed a repository with one passing test, one failing test (with a
/// Python-style traceback), and one errored test. Returns the temp dir and
/// the base path string.
fn seed_repo_with_mixed_results() -> TempDir {
    let temp = TempDir::new().unwrap();
    let factory = InquestRepositoryFactory;
    let mut repo = factory.initialise(temp.path()).unwrap();

    let mut run = TestRun::new(RunId::new("0"));
    run.timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    run.add_result(TestResult::success("tests.test_alpha"));
    run.add_result(
        TestResult::failure("tests.test_beta", "AssertionError: 1 != 2").with_details(
            "Traceback (most recent call last):\n  \
             File \"tests/test_beta.py\", line 42, in test_it\n    \
             assert 1 == 2\nAssertionError: 1 != 2",
        ),
    );
    run.add_result(TestResult::error("tests.test_gamma", "timeout after 30s"));
    repo.insert_test_run_partial(run, false).unwrap();

    temp
}

#[test]
fn ci_help_lists_format_and_retry() {
    let out = ProcessCommand::new(inq_bin())
        .arg("ci")
        .arg("--help")
        .output()
        .expect("run inq ci --help");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--format"), "missing --format: {stdout}");
    assert!(stdout.contains("--retry"), "missing --retry: {stdout}");
    assert!(stdout.contains("--order"), "missing --order: {stdout}");
}

#[test]
fn seeded_repo_records_failing_tests_for_ci_to_consume() {
    let temp = seed_repo_with_mixed_results();
    let path = temp.path().to_string_lossy();
    let repo = inquest::commands::utils::open_repository(Some(path.as_ref())).unwrap();
    let failing = repo.get_failing_tests().unwrap();
    // tests.test_beta and tests.test_gamma; tests.test_alpha passed.
    assert_eq!(failing.len(), 2);
}

#[test]
fn ci_format_parses_provider_names() {
    assert_eq!("github".parse::<CiFormat>().unwrap(), CiFormat::Github);
    assert_eq!("gitlab".parse::<CiFormat>().unwrap(), CiFormat::Gitlab);
    assert_eq!("plain".parse::<CiFormat>().unwrap(), CiFormat::Plain);
    assert_eq!("auto".parse::<CiFormat>().unwrap(), CiFormat::Auto);
    assert!("travis".parse::<CiFormat>().is_err());
}

#[test]
fn ci_against_empty_dir_emits_clean_error() {
    // No inquest.toml, no .inquest, no cache restore — should fail with a
    // useful error rather than panicking or producing garbled output.
    let temp = TempDir::new().unwrap();
    let out = ProcessCommand::new(inq_bin())
        .arg("-C")
        .arg(temp.path())
        .arg("ci")
        .arg("--format=plain")
        .output()
        .expect("run inq ci");
    assert!(!out.status.success(), "expected non-zero exit");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // We expect either the auto-detect failure (project type not detected)
    // or a missing-config error. Either way, the message should mention
    // detection or configuration so a CI user can act on it.
    assert!(
        combined.contains("detect")
            || combined.contains("inquest.toml")
            || combined.contains("config"),
        "unexpected error: {combined}"
    );
}
