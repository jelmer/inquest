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

#[test]
fn config_with_profile_resolves_overlay() {
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join("inquest.toml"),
        r#"
test_command = "echo"
test_timeout = "1m"

[profiles.ci]
test_timeout = "5m"
"#,
    )
    .unwrap();

    let out = Command::new(inq_bin())
        .arg("-C")
        .arg(temp.path())
        .arg("--profile")
        .arg("ci")
        .arg("config")
        .output()
        .expect("run inq --profile ci config");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Active profile: ci"), "got: {stdout}");
    assert!(
        stdout.contains("test_timeout: 5m [profile:ci]"),
        "got: {stdout}"
    );
}

#[test]
fn config_list_profiles_lists_names() {
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join("inquest.toml"),
        r#"
test_command = "echo"

[profiles.ci]
test_timeout = "5m"

[profiles.nightly]
test_timeout = "30m"
"#,
    )
    .unwrap();

    let out = Command::new(inq_bin())
        .arg("-C")
        .arg(temp.path())
        .arg("config")
        .arg("--list-profiles")
        .output()
        .expect("run inq config --list-profiles");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Profiles:"), "got: {stdout}");
    assert!(stdout.contains("  ci"), "got: {stdout}");
    assert!(stdout.contains("  nightly"), "got: {stdout}");
}

#[test]
fn unknown_profile_errors_with_available_list() {
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join("inquest.toml"),
        r#"
test_command = "echo"

[profiles.ci]
test_timeout = "5m"
"#,
    )
    .unwrap();

    let out = Command::new(inq_bin())
        .arg("-C")
        .arg(temp.path())
        .arg("--profile")
        .arg("nope")
        .arg("config")
        .output()
        .expect("run inq --profile nope config");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nope"), "stderr: {stderr}");
    assert!(stderr.contains("ci"), "stderr: {stderr}");
}

#[test]
fn inq_profile_env_var_selects_profile() {
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join("inquest.toml"),
        r#"
test_command = "echo"

[profiles.ci]
test_timeout = "5m"
"#,
    )
    .unwrap();

    let out = Command::new(inq_bin())
        .arg("-C")
        .arg(temp.path())
        .arg("config")
        .env("INQ_PROFILE", "ci")
        .output()
        .expect("run inq config with INQ_PROFILE");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Active profile: ci"), "got: {stdout}");
}

#[test]
fn cli_profile_overrides_env_var() {
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join("inquest.toml"),
        r#"
test_command = "echo"

[profiles.ci]
test_timeout = "5m"

[profiles.dev]
test_timeout = "10m"
"#,
    )
    .unwrap();

    let out = Command::new(inq_bin())
        .arg("-C")
        .arg(temp.path())
        .arg("--profile")
        .arg("dev")
        .arg("config")
        .env("INQ_PROFILE", "ci")
        .output()
        .expect("run inq --profile dev config");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Active profile: dev"), "got: {stdout}");
    assert!(
        stdout.contains("test_timeout: 10m [profile:dev]"),
        "got: {stdout}"
    );
}

#[test]
fn shard_help_documents_spec_and_options() {
    let out = Command::new(inq_bin())
        .arg("shard")
        .arg("--help")
        .output()
        .expect("run inq shard --help");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("<N/M>"),
        "expected <N/M> placeholder: {stdout}"
    );
    assert!(
        stdout.contains("--group-regex"),
        "expected --group-regex flag: {stdout}"
    );
    assert!(
        stdout.contains("--zero-indexed"),
        "expected --zero-indexed flag: {stdout}"
    );
}

#[test]
fn shard_rejects_invalid_spec() {
    let temp = TempDir::new().unwrap();
    let out = Command::new(inq_bin())
        .arg("-C")
        .arg(temp.path())
        .arg("shard")
        .arg("not-a-spec")
        .output()
        .expect("run inq shard");
    assert!(!out.status.success());
}

#[test]
fn flat_config_still_works_unchanged() {
    // Backwards-compat smoke test: a flat config (no profiles, no
    // default_profile) loads and resolves with no profile annotations.
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join("inquest.toml"),
        r#"
test_command = "cargo subunit $LISTOPT $IDOPTION"
test_id_option = "--load-list $IDFILE"
test_list_option = "--list"
"#,
    )
    .unwrap();

    let out = Command::new(inq_bin())
        .arg("-C")
        .arg(temp.path())
        .arg("config")
        .output()
        .expect("run inq config");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("Active profile:"), "got: {stdout}");
    assert!(!stdout.contains("[profile:"), "got: {stdout}");
}
