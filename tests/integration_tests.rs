//! Integration tests for full workflows
//!
//! These tests exercise complete user workflows by running actual commands
//! against real repositories in temporary directories.

use inquest::commands::{
    AnalyzeIsolationCommand, Command, FailingCommand, InitCommand, LastCommand, StatsCommand,
};
use inquest::repository::{RepositoryFactory, RunId, TestResult, TestRun};
use inquest::ui::UI;
use std::fs;
use std::io::Write;
use tempfile::TempDir;

/// Simple test UI that captures output for assertions
struct TestUI {
    output: Vec<String>,
    errors: Vec<String>,
    bytes_output: Vec<Vec<u8>>,
}

impl TestUI {
    fn new() -> Self {
        // Keep progress bars out of `cargo test`'s captured stdout. Idempotent.
        inquest::config::disable_progress_in_process();
        TestUI {
            output: Vec::new(),
            errors: Vec::new(),
            bytes_output: Vec::new(),
        }
    }
}

impl UI for TestUI {
    fn output(&mut self, message: &str) -> inquest::error::Result<()> {
        self.output.push(message.to_string());
        Ok(())
    }

    fn error(&mut self, message: &str) -> inquest::error::Result<()> {
        self.errors.push(message.to_string());
        Ok(())
    }

    fn warning(&mut self, message: &str) -> inquest::error::Result<()> {
        self.errors.push(format!("Warning: {}", message));
        Ok(())
    }

    fn output_bytes(&mut self, bytes: &[u8]) -> inquest::error::Result<()> {
        self.bytes_output.push(bytes.to_vec());
        Ok(())
    }
}

#[test]
fn test_full_workflow_init_load_last() {
    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Step 1: Initialize repository
    let mut ui = TestUI::new();
    let init_cmd = InitCommand::new(Some(base_path.clone()));
    let result = init_cmd.execute(&mut ui);
    assert_eq!(result.unwrap(), 0);
    assert!(ui.output[0].contains("Initialized"));

    // Verify repository was created
    assert!(temp.path().join(".inquest").exists());
    assert!(temp.path().join(".inquest/format").exists());

    // Step 2: Load a test run
    let mut test_run = TestRun::new(RunId::new("0"));
    test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
    test_run.add_result(TestResult::success("test1"));
    test_run.add_result(TestResult::failure("test2", "Failed"));
    test_run.add_result(TestResult::success("test3"));

    // Load the test run directly using the repository API
    // (In real usage, this would be done via LoadCommand reading from stdin)
    let factory = inquest::repository::inquest::InquestRepositoryFactory;
    let mut repo = factory.open(temp.path()).unwrap();
    repo.insert_test_run(test_run).unwrap();

    // Step 3: Check stats
    let mut ui = TestUI::new();
    let stats_cmd = StatsCommand::new(Some(base_path.clone()));
    let result = stats_cmd.execute(&mut ui);
    assert_eq!(result.unwrap(), 0);
    assert_eq!(ui.output.len(), 6);
    assert_eq!(ui.output[0], "Repository Statistics:");
    assert_eq!(ui.output[1], "  Total test runs: 1");
    assert_eq!(ui.output[2], "  Latest run: 0");
    assert_eq!(ui.output[3], "  Tests in latest run: 3");
    assert_eq!(ui.output[4], "  Failures in latest run: 1");
    assert_eq!(ui.output[5], "  Total tests executed: 3");

    // Step 4: Get last run
    let mut ui = TestUI::new();
    let last_cmd = LastCommand::new(Some(base_path.clone()));
    let result = last_cmd.execute(&mut ui);
    assert_eq!(result.unwrap(), 1); // Exit code 1 because there's a failure

    // Verify exact output structure
    // Note: insert_test_run() doesn't include file attachments, so we only get test IDs
    assert_eq!(ui.output.len(), 8);
    assert_eq!(ui.output[0], "Test run: 0");
    assert!(ui.output[1].starts_with("Timestamp: "));
    assert_eq!(ui.output[2], "Total tests: 3");
    assert_eq!(ui.output[3], "Passed: 2");
    assert_eq!(ui.output[4], "Failed: 1");
    assert_eq!(ui.output[5], "");
    assert_eq!(ui.output[6], "Failed tests:");
    assert_eq!(ui.output[7], "  test2");

    // No detailed output since insert_test_run() doesn't write file attachments
    assert_eq!(ui.bytes_output.len(), 0);
}

#[test]
fn test_workflow_with_failing_tests() {
    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize repository
    let mut ui = TestUI::new();
    let init_cmd = InitCommand::new(Some(base_path.clone()));
    init_cmd.execute(&mut ui).unwrap();

    // Load first run with failures
    let mut run1 = TestRun::new(RunId::new("0"));
    run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
    run1.add_result(TestResult::success("test1"));
    run1.add_result(TestResult::failure("test2", "Error"));
    run1.add_result(TestResult::failure("test3", "Error"));

    let factory = inquest::repository::inquest::InquestRepositoryFactory;
    let mut repo = factory.open(temp.path()).unwrap();
    repo.insert_test_run_partial(run1, false).unwrap();

    // Check failing tests
    let mut ui = TestUI::new();
    let failing_cmd = FailingCommand::new(Some(base_path.clone()));
    let result = failing_cmd.execute(&mut ui);
    assert_eq!(result.unwrap(), 1); // Exit code 1 when there are failures
    assert_eq!(ui.output[0], "2 failing test(s):");
    // The order might vary, so check both test IDs are present
    assert!(ui.output[1] == "  test2" || ui.output[1] == "  test3");
    assert!(ui.output[2] == "  test2" || ui.output[2] == "  test3");
    assert_ne!(ui.output[1], ui.output[2]); // Make sure they're different

    // Load second run where test2 passes
    let mut run2 = TestRun::new(RunId::new("1"));
    run2.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
    run2.add_result(TestResult::success("test1"));
    run2.add_result(TestResult::success("test2"));
    run2.add_result(TestResult::failure("test3", "Still failing"));

    repo.insert_test_run_partial(run2, false).unwrap();

    // Check failing tests again - should only have test3
    let mut ui = TestUI::new();
    let failing_cmd = FailingCommand::new(Some(base_path));
    let result = failing_cmd.execute(&mut ui);
    assert_eq!(result.unwrap(), 1); // Exit code 1 when there are failures
    assert_eq!(ui.output.len(), 2);
    assert_eq!(ui.output[0], "1 failing test(s):");
    assert_eq!(ui.output[1], "  test3");
}

#[test]
fn test_workflow_partial_mode() {
    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize repository
    let mut ui = TestUI::new();
    let init_cmd = InitCommand::new(Some(base_path.clone()));
    init_cmd.execute(&mut ui).unwrap();

    let factory = inquest::repository::inquest::InquestRepositoryFactory;
    let mut repo = factory.open(temp.path()).unwrap();

    // First full run
    let mut run1 = TestRun::new(RunId::new("0"));
    run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
    run1.add_result(TestResult::failure("test1", "Error"));
    run1.add_result(TestResult::failure("test2", "Error"));
    run1.add_result(TestResult::success("test3"));

    repo.insert_test_run_partial(run1, false).unwrap();

    // Check we have 2 failing tests
    let failing = repo.get_failing_tests().unwrap();
    assert_eq!(failing.len(), 2);

    // Second partial run - only test test1
    let mut run2 = TestRun::new(RunId::new("1"));
    run2.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
    run2.add_result(TestResult::success("test1")); // Now passes

    repo.insert_test_run_partial(run2, true).unwrap(); // Partial mode

    // Should only have test2 failing now
    let failing = repo.get_failing_tests().unwrap();
    assert_eq!(failing.len(), 1);
    assert!(failing.iter().any(|id| id.as_str() == "test2"));
    assert!(!failing.iter().any(|id| id.as_str() == "test1"));
}

#[test]
fn test_workflow_with_load_list() {
    let temp = TempDir::new().unwrap();

    // Create a test list file
    let test_list_path = temp.path().join("tests.txt");
    let mut file = fs::File::create(&test_list_path).unwrap();
    writeln!(file, "test1").unwrap();
    writeln!(file, "test3").unwrap();
    writeln!(file, "test5").unwrap();

    // Parse and verify
    let test_ids = inquest::testlist::parse_list_file(&test_list_path).unwrap();
    assert_eq!(test_ids.len(), 3);
    assert_eq!(test_ids[0].as_str(), "test1");
    assert_eq!(test_ids[1].as_str(), "test3");
    assert_eq!(test_ids[2].as_str(), "test5");
}

#[test]
fn test_workflow_times_database() {
    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize repository
    let mut ui = TestUI::new();
    let init_cmd = InitCommand::new(Some(base_path));
    init_cmd.execute(&mut ui).unwrap();

    let factory = inquest::repository::inquest::InquestRepositoryFactory;
    let mut repo = factory.open(temp.path()).unwrap();

    // Insert run with durations
    let mut run = TestRun::new(RunId::new("0"));
    run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
    run.add_result(
        TestResult::success("test1").with_duration(std::time::Duration::from_secs_f64(1.5)),
    );
    run.add_result(
        TestResult::success("test2").with_duration(std::time::Duration::from_secs_f64(0.3)),
    );

    repo.insert_test_run(run).unwrap();

    // Verify times were stored in the database
    let test_ids = vec![
        inquest::repository::TestId::new("test1"),
        inquest::repository::TestId::new("test2"),
    ];
    let times = repo.get_test_times_for_ids(&test_ids).unwrap();
    assert_eq!(times.len(), 2);
    assert_eq!(
        times
            .get(&inquest::repository::TestId::new("test1"))
            .unwrap()
            .as_secs_f64(),
        1.5
    );
    assert_eq!(
        times
            .get(&inquest::repository::TestId::new("test2"))
            .unwrap()
            .as_secs_f64(),
        0.3
    );
}

#[test]
fn test_workflow_list_flag() {
    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize and populate repository
    let mut ui = TestUI::new();
    let init_cmd = InitCommand::new(Some(base_path.clone()));
    init_cmd.execute(&mut ui).unwrap();

    let factory = inquest::repository::inquest::InquestRepositoryFactory;
    let mut repo = factory.open(temp.path()).unwrap();

    // Add a run with failures
    let mut run = TestRun::new(RunId::new("0"));
    run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
    run.add_result(TestResult::failure("test1", "Error"));
    run.add_result(TestResult::failure("test2", "Error"));
    run.add_result(TestResult::success("test3"));

    repo.insert_test_run_partial(run, false).unwrap();

    // Test --list flag
    let mut ui = TestUI::new();
    let failing_cmd = FailingCommand::with_list_only(Some(base_path));
    let result = failing_cmd.execute(&mut ui);
    assert_eq!(result.unwrap(), 1); // Exit code 1 when there are failures

    // Should output test IDs only, one per line
    assert_eq!(ui.output.len(), 2);
    // Order might vary, so check both are present
    assert!(ui.output[0] == "test1" || ui.output[0] == "test2");
    assert!(ui.output[1] == "test1" || ui.output[1] == "test2");
    assert_ne!(ui.output[0], ui.output[1]);
}

#[test]
fn test_error_handling_no_repository() {
    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Try to run last command without initializing
    let mut ui = TestUI::new();
    let last_cmd = LastCommand::new(Some(base_path));
    let result = last_cmd.execute(&mut ui);

    // Should fail with an error
    assert!(result.is_err());
}

#[test]
fn test_parallel_execution() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize repository
    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // Create a simple test configuration that outputs subunit
    let config = r#"
[DEFAULT]
test_command=python3 -c "import sys; import time; sys.stdout.buffer.write(b'\xb3\x29\x00\x16test1\x20\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xb3'); sys.stdout.buffer.flush()"
"#;
    fs::write(temp.path().join(".testr.conf"), config).unwrap();

    // Run with parallel execution
    let mut ui = TestUI::new();
    let cmd = RunCommand {
        base_path: Some(base_path.clone()),
        concurrency: Some(2),
        ..Default::default()
    };

    // Note: This test will fail to actually run because the command is synthetic
    // But it tests that the parallel code path is exercised
    let _result = cmd.execute(&mut ui);

    // The command should have at least attempted to run
    assert!(!ui.output.is_empty());
}

#[test]
fn test_parallel_execution_with_worker_tags() {
    use inquest::partition::partition_tests;
    use inquest::repository::TestId;
    use std::collections::HashMap;

    // Create a set of test IDs
    let test_ids = vec![
        TestId::new("test1"),
        TestId::new("test2"),
        TestId::new("test3"),
        TestId::new("test4"),
    ];

    // Partition across 2 workers
    let partitions = partition_tests(&test_ids, &HashMap::new(), 2);

    // Should create 2 partitions
    assert_eq!(partitions.len(), 2);

    // All tests should be accounted for
    let total_tests: usize = partitions.iter().map(|p| p.len()).sum();
    assert_eq!(total_tests, 4);

    // Each partition should have at least one test
    assert!(!partitions[0].is_empty());
    assert!(!partitions[1].is_empty());
}

#[test]
fn test_until_failure_flag_behavior() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize repository
    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // Create a simple test configuration that always succeeds
    let config = r#"
[DEFAULT]
test_command=echo ""
"#;
    fs::write(temp.path().join(".testr.conf"), config).unwrap();

    // Create command with until_failure set to true
    // The test will succeed but we can verify the flag was accepted
    // by checking that the command can be created
    let cmd = RunCommand {
        base_path: Some(base_path.clone()),
        until_failure: true,
        ..Default::default()
    };

    // Verify the command was created successfully
    // (The actual looping behavior would run infinitely with always-passing tests,
    // so we just verify the command can be constructed with the flag)
    assert_eq!(cmd.name(), "run");
}

#[test]
fn test_isolated_flag_behavior() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize repository
    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // Create a simple test configuration
    let config = r#"
[DEFAULT]
test_command=echo ""
"#;
    fs::write(temp.path().join(".testr.conf"), config).unwrap();

    // Create command with isolated set to true
    let cmd = RunCommand {
        base_path: Some(base_path.clone()),
        isolated: true,
        ..Default::default()
    };

    // Verify the command was created successfully
    assert_eq!(cmd.name(), "run");
}

#[test]
fn test_analyze_isolation_command_no_repository() {
    // Test that analyze-isolation gives proper error when repository doesn't exist
    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let mut ui = TestUI::new();
    let cmd = AnalyzeIsolationCommand::new(Some(base_path.clone()), "test_example".to_string());

    // Should fail because no repository exists
    let result = cmd.execute(&mut ui);
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Repository not found"));
}

#[test]
fn test_analyze_isolation_command_basic() {
    // Test that analyze-isolation command can be created and has correct metadata
    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize repository
    let mut ui = TestUI::new();
    let init_cmd = InitCommand::new(Some(base_path.clone()));
    init_cmd.execute(&mut ui).unwrap();

    // Create .testr.conf with a simple command that outputs passing test
    let config = r#"
[DEFAULT]
test_command=printf "test: test_target\nsuccess: test_target\n" | python3 -c "import sys; sys.stdout.buffer.write(b'\xb3)\x00\x00\x01\x1btest: test_target\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x05\xb3*\x00\x00\x00\x1asuccess: test_target\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x05')"
test_list_option=--list
"#;
    fs::write(temp.path().join(".testr.conf"), config).unwrap();

    // Create the analyze-isolation command
    let cmd = AnalyzeIsolationCommand::new(Some(base_path), "test_target".to_string());

    // Verify command metadata
    assert_eq!(cmd.name(), "analyze-isolation");
    assert_eq!(cmd.help(), "Analyze test isolation issues using bisection");
}

#[test]
fn test_group_regex_with_parallel_execution() {
    use inquest::testcommand::TestCommand;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize repository
    let mut ui = TestUI::new();
    let init_cmd = InitCommand::new(Some(base_path.clone()));
    init_cmd.execute(&mut ui).unwrap();

    // Create .testr.conf with group_regex to group by module
    let config = r#"
[DEFAULT]
test_command=echo ""
test_list_option=--list
group_regex=^([^.]+)\.
"#;
    fs::write(temp.path().join(".testr.conf"), config).unwrap();

    // Load TestCommand and verify group_regex is set
    let test_cmd = TestCommand::from_directory(temp.path()).unwrap();
    assert_eq!(
        test_cmd.config().group_regex,
        Some("^([^.]+)\\.".to_string())
    );
}

#[test]
fn test_run_concurrency_callout() {
    use inquest::testcommand::TestCommand;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize repository
    let mut ui = TestUI::new();
    let init_cmd = InitCommand::new(Some(base_path.clone()));
    init_cmd.execute(&mut ui).unwrap();

    // Create .testr.conf with test_run_concurrency
    let config = r#"
[DEFAULT]
test_command=echo ""
test_list_option=--list
test_run_concurrency=echo 2
"#;
    fs::write(temp.path().join(".testr.conf"), config).unwrap();

    // Load TestCommand and verify concurrency is determined from callout
    let test_cmd = TestCommand::from_directory(temp.path()).unwrap();
    let concurrency = test_cmd.get_concurrency().unwrap();
    assert_eq!(concurrency, Some(2));
}

#[test]
fn test_run_concurrency_callout_inquest_toml() {
    use inquest::testcommand::TestCommand;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize repository
    let mut ui = TestUI::new();
    let init_cmd = InitCommand::new(Some(base_path.clone()));
    init_cmd.execute(&mut ui).unwrap();

    // Create inquest.toml instead of .testr.conf
    let config = r#"
test_command = "echo \"\""
test_list_option = "--list"
test_run_concurrency = "echo 2"
"#;
    fs::write(temp.path().join("inquest.toml"), config).unwrap();

    // Load TestCommand and verify it works with TOML config
    let test_cmd = TestCommand::from_directory(temp.path()).unwrap();
    let concurrency = test_cmd.get_concurrency().unwrap();
    assert_eq!(concurrency, Some(2));
}

#[test]
#[cfg_attr(target_os = "windows", ignore = "sh does not handle Windows paths")]
fn test_serial_run_with_max_duration_kills_hanging_process() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    let config = "test_command = \"sleep 300\"\n";
    fs::write(temp.path().join("inquest.toml"), config).unwrap();

    // Write a load-list file so list_tests() is never called (which would hang)
    let load_list_path = temp.path().join("test_ids.txt");
    fs::write(&load_list_path, "fake_test\n").unwrap();

    let mut ui = TestUI::new();
    let cmd = RunCommand {
        base_path: Some(base_path),
        load_list: Some(load_list_path.to_string_lossy().to_string()),
        max_duration: inquest::config::TimeoutSetting::Fixed(std::time::Duration::from_secs(2)),
        ..Default::default()
    };

    let start = std::time::Instant::now();
    let result = cmd.execute(&mut ui);
    let elapsed = start.elapsed();
    // Should complete (not hang) — max_duration kills the process
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "Test took {:?} — process was not killed promptly by max_duration",
        elapsed
    );
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 1);
}

#[test]
fn test_serial_run_with_no_output_timeout_kills_silent_process() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    let config = "test_command = \"sleep 300\"\n";
    fs::write(temp.path().join("inquest.toml"), config).unwrap();

    let load_list_path = temp.path().join("test_ids.txt");
    fs::write(&load_list_path, "fake_test\n").unwrap();

    let mut ui = TestUI::new();
    let cmd = RunCommand {
        base_path: Some(base_path),
        load_list: Some(load_list_path.to_string_lossy().to_string()),
        no_output_timeout: Some(std::time::Duration::from_secs(2)),
        ..Default::default()
    };

    let start = std::time::Instant::now();
    let result = cmd.execute(&mut ui);
    let elapsed = start.elapsed();
    // Should complete (not hang) — no_output_timeout kills the process
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "Test took {:?} — process was not killed promptly by no_output_timeout",
        elapsed
    );
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), 1);
}

#[test]
#[cfg_attr(target_os = "windows", ignore = "sh does not handle Windows paths")]
fn test_run_persists_stderr_for_crashing_runner() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // A "test runner" that prints a marker line to stderr and exits with
    // a non-zero status without producing any subunit output.
    let config = "test_command = \"echo runner-crashed-marker 1>&2; exit 7\"\n";
    fs::write(temp.path().join("inquest.toml"), config).unwrap();

    let load_list_path = temp.path().join("test_ids.txt");
    fs::write(&load_list_path, "fake_test\n").unwrap();

    let mut ui = TestUI::new();
    let cmd = RunCommand {
        base_path: Some(base_path.clone()),
        load_list: Some(load_list_path.to_string_lossy().to_string()),
        ..Default::default()
    };
    let exit_code = cmd.execute(&mut ui).unwrap();
    assert_ne!(exit_code, 0);

    let factory = InquestRepositoryFactory;
    let repo = factory.open(temp.path()).unwrap();
    let latest = repo.get_latest_run().unwrap();
    let stderr = repo
        .get_run_stderr(&latest.id)
        .unwrap()
        .expect("stderr should be persisted");
    assert!(
        stderr
            .windows(b"runner-crashed-marker".len())
            .any(|w| w == b"runner-crashed-marker"),
        "stderr did not contain the marker, got: {}",
        String::from_utf8_lossy(&stderr)
    );
}

/// Generate subunit v2 binary bytes for an InProgress event.
fn subunit_inprogress(test_id: &str) -> Vec<u8> {
    use subunit::serialize::Serializable;
    use subunit::types::event::Event;
    use subunit::types::teststatus::TestStatus as SubunitTestStatus;
    let mut bytes = Vec::new();
    Event::new(SubunitTestStatus::InProgress)
        .test_id(test_id)
        .build()
        .serialize(&mut bytes)
        .unwrap();
    bytes
}

/// Generate subunit v2 binary bytes for a Success event.
fn subunit_success(test_id: &str) -> Vec<u8> {
    use subunit::serialize::Serializable;
    use subunit::types::event::Event;
    use subunit::types::teststatus::TestStatus as SubunitTestStatus;
    let mut bytes = Vec::new();
    Event::new(SubunitTestStatus::Success)
        .test_id(test_id)
        .build()
        .serialize(&mut bytes)
        .unwrap();
    bytes
}

/// Generate subunit v2 binary bytes for a Failed event.
fn subunit_failure(test_id: &str) -> Vec<u8> {
    use subunit::serialize::Serializable;
    use subunit::types::event::Event;
    use subunit::types::teststatus::TestStatus as SubunitTestStatus;
    let mut bytes = Vec::new();
    Event::new(SubunitTestStatus::Failed)
        .test_id(test_id)
        .build()
        .serialize(&mut bytes)
        .unwrap();
    bytes
}

/// Generate subunit v2 binary bytes for an Enumeration event (used by --list).
fn subunit_enumerate(test_id: &str) -> Vec<u8> {
    use subunit::serialize::Serializable;
    use subunit::types::event::Event;
    use subunit::types::teststatus::TestStatus as SubunitTestStatus;
    let mut bytes = Vec::new();
    Event::new(SubunitTestStatus::Enumeration)
        .test_id(test_id)
        .build()
        .serialize(&mut bytes)
        .unwrap();
    bytes
}

/// Write binary data to a file and return the path as a string.
fn write_bin(dir: &std::path::Path, name: &str, data: &[u8]) -> String {
    let path = dir.join(name);
    fs::write(&path, data).unwrap();
    path.to_string_lossy().to_string()
}

#[test]
#[cfg_attr(target_os = "windows", ignore = "sh does not handle Windows paths")]
fn test_serial_restart_on_per_test_timeout() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // Generate subunit binary files:
    // - test_a: inprogress + success (completes normally)
    // - test_hangs: inprogress only (simulates hang)
    // - test_b: inprogress + success (should run on restart)
    let mut pass_a = subunit_inprogress("test_a");
    pass_a.extend(subunit_success("test_a"));
    write_bin(temp.path(), "pass_a.bin", &pass_a);

    let hang = subunit_inprogress("test_hangs");
    write_bin(temp.path(), "hang.bin", &hang);

    let mut pass_b = subunit_inprogress("test_b");
    pass_b.extend(subunit_success("test_b"));
    write_bin(temp.path(), "pass_b.bin", &pass_b);

    // Listing: emit all three test IDs
    let mut list_bytes = subunit_enumerate("test_a");
    list_bytes.extend(subunit_enumerate("test_hangs"));
    list_bytes.extend(subunit_enumerate("test_b"));
    write_bin(temp.path(), "list.bin", &list_bytes);

    // Script: first invocation runs test_a then hangs on test_hangs.
    // Second invocation (restart) runs test_b and test_hangs (now succeeds).
    let dir = temp.path().to_string_lossy();
    let script = format!(
        r#"#!/bin/sh
DIR="{dir}"
if [ "$1" = "--list" ]; then
    cat "$DIR/list.bin"
    exit 0
fi
MARKER="$DIR/marker"
if [ ! -f "$MARKER" ]; then
    touch "$MARKER"
    cat "$DIR/pass_a.bin"
    cat "$DIR/hang.bin"
    sleep 300
else
    cat "$DIR/pass_b.bin"
fi
"#,
    );
    let script_path = temp.path().join("run.sh");
    fs::write(&script_path, &script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut config = toml::map::Map::new();
    config.insert(
        "test_command".into(),
        toml::Value::String(format!(
            "sh {} $LISTOPT $IDOPTION $IDFILE",
            script_path.display()
        )),
    );
    config.insert(
        "test_list_option".into(),
        toml::Value::String("--list".into()),
    );
    config.insert(
        "test_id_option".into(),
        toml::Value::String("--load-list".into()),
    );
    fs::write(
        temp.path().join("inquest.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();

    let mut ui = TestUI::new();
    let cmd = RunCommand {
        base_path: Some(base_path),
        test_timeout: inquest::config::TimeoutSetting::Fixed(std::time::Duration::from_secs(2)),
        ..Default::default()
    };

    let result = cmd.execute(&mut ui);
    assert!(result.is_ok(), "execute failed: {:?}", result);
    // Exit code 1 because test_hangs timed out
    assert_eq!(result.unwrap(), 1);
}

#[test]
#[cfg_attr(target_os = "windows", ignore = "sh does not handle Windows paths")]
fn test_serial_restart_no_explicit_test_list() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    // Same as above, but without load-list — test_cmd.list_tests() is called on restart
    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    let mut pass_a = subunit_inprogress("test_a");
    pass_a.extend(subunit_success("test_a"));
    write_bin(temp.path(), "pass_a.bin", &pass_a);

    let hang = subunit_inprogress("test_hangs");
    write_bin(temp.path(), "hang.bin", &hang);

    let mut pass_b = subunit_inprogress("test_b");
    pass_b.extend(subunit_success("test_b"));
    write_bin(temp.path(), "pass_b.bin", &pass_b);

    let mut list_bytes = subunit_enumerate("test_a");
    list_bytes.extend(subunit_enumerate("test_hangs"));
    list_bytes.extend(subunit_enumerate("test_b"));
    write_bin(temp.path(), "list.bin", &list_bytes);

    let dir = temp.path().to_string_lossy();
    let script = format!(
        r#"#!/bin/sh
DIR="{dir}"
if [ "$1" = "--list" ]; then
    cat "$DIR/list.bin"
    exit 0
fi
MARKER="$DIR/marker"
if [ ! -f "$MARKER" ]; then
    touch "$MARKER"
    cat "$DIR/pass_a.bin"
    cat "$DIR/hang.bin"
    sleep 300
else
    cat "$DIR/pass_b.bin"
fi
"#,
    );
    let script_path = temp.path().join("run.sh");
    fs::write(&script_path, &script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // No test_id_option — the runner does not pass IDs to the command.
    // On restart, list_tests() is called to discover remaining tests.
    let mut config = toml::map::Map::new();
    config.insert(
        "test_command".into(),
        toml::Value::String(format!("sh {} $LISTOPT", script_path.display())),
    );
    config.insert(
        "test_list_option".into(),
        toml::Value::String("--list".into()),
    );
    fs::write(
        temp.path().join("inquest.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();

    let mut ui = TestUI::new();
    // No load-list: test_ids will be None
    let cmd = RunCommand {
        base_path: Some(base_path),
        test_timeout: inquest::config::TimeoutSetting::Fixed(std::time::Duration::from_secs(2)),
        ..Default::default()
    };

    let result = cmd.execute(&mut ui);
    assert!(result.is_ok(), "execute failed: {:?}", result);
    assert_eq!(result.unwrap(), 1);
}

#[test]
#[cfg_attr(target_os = "windows", ignore = "sh does not handle Windows paths")]
fn test_parallel_restart_on_per_test_timeout() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    let mut pass_a = subunit_inprogress("test_a");
    pass_a.extend(subunit_success("test_a"));
    write_bin(temp.path(), "pass_a.bin", &pass_a);

    let hang = subunit_inprogress("test_hangs");
    write_bin(temp.path(), "hang.bin", &hang);

    let mut pass_b = subunit_inprogress("test_b");
    pass_b.extend(subunit_success("test_b"));
    write_bin(temp.path(), "pass_b.bin", &pass_b);

    let mut list_bytes = subunit_enumerate("test_a");
    list_bytes.extend(subunit_enumerate("test_hangs"));
    list_bytes.extend(subunit_enumerate("test_b"));
    write_bin(temp.path(), "list.bin", &list_bytes);

    let dir = temp.path().to_string_lossy();
    // Script reads $IDFILE to decide which tests to run.
    // If test_hangs is in the list AND no marker exists, it hangs.
    // Otherwise it completes all requested tests normally.
    let script = format!(
        r#"#!/bin/sh
DIR="{dir}"
if [ "$1" = "--list" ]; then
    cat "$DIR/list.bin"
    exit 0
fi
# Read test IDs from IDFILE (passed as $2, after --load-list)
IDFILE="$2"
MARKER="$DIR/marker"
if [ -n "$IDFILE" ] && [ -f "$IDFILE" ]; then
    while IFS= read -r test_id; do
        case "$test_id" in
            test_a) cat "$DIR/pass_a.bin" ;;
            test_b) cat "$DIR/pass_b.bin" ;;
            test_hangs)
                if [ ! -f "$MARKER" ]; then
                    touch "$MARKER"
                    cat "$DIR/hang.bin"
                    sleep 300
                    exit 1
                else
                    cat "$DIR/pass_b.bin"
                fi
                ;;
        esac
    done < "$IDFILE"
else
    cat "$DIR/pass_a.bin"
    cat "$DIR/hang.bin"
    sleep 300
fi
"#,
    );
    let script_path = temp.path().join("run.sh");
    fs::write(&script_path, &script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut config = toml::map::Map::new();
    config.insert(
        "test_command".into(),
        toml::Value::String(format!(
            "sh {} $LISTOPT $IDOPTION $IDFILE",
            script_path.display()
        )),
    );
    config.insert(
        "test_list_option".into(),
        toml::Value::String("--list".into()),
    );
    config.insert(
        "test_id_option".into(),
        toml::Value::String("--load-list".into()),
    );
    fs::write(
        temp.path().join("inquest.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();

    let mut ui = TestUI::new();
    let cmd = RunCommand {
        base_path: Some(base_path),
        concurrency: Some(2),
        test_timeout: inquest::config::TimeoutSetting::Fixed(std::time::Duration::from_secs(2)),
        ..Default::default()
    };

    let result = cmd.execute(&mut ui);
    assert!(result.is_ok(), "execute failed: {:?}", result);
    // Exit code 1 because test_hangs timed out on first attempt
    assert_eq!(result.unwrap(), 1);
}

#[test]
fn test_stream_interruption_partial_results() {
    // Parse a stream that starts valid but ends with garbage — partial results should be returned
    let mut stream = Vec::new();
    stream.extend(subunit_inprogress("test_ok"));
    stream.extend(subunit_success("test_ok"));
    stream.extend(subunit_inprogress("test_incomplete"));
    // Append garbage to simulate a truncated/corrupted stream
    stream.extend(b"\xff\xfe\xfd\xfc\xfb\xfa\xf9\xf8\xff\xfe\xfd\xfc\xfb\xfa\xf9\xf8");
    stream.extend(b"\xff\xfe\xfd\xfc\xfb\xfa\xf9\xf8\xff\xfe\xfd\xfc\xfb\xfa\xf9\xf8");

    let run = inquest::subunit_stream::parse_stream(std::io::Cursor::new(stream), RunId::new("0"));
    let run = run.unwrap();

    // test_ok should have been fully parsed
    assert_eq!(run.results.len(), 1);
    let (id, result) = run.results.iter().next().unwrap();
    assert_eq!(id.as_str(), "test_ok");
    assert_eq!(result.status, inquest::repository::TestStatus::Success);

    // test_incomplete started but never finished, so it shouldn't be in results
    assert!(!run
        .results
        .contains_key(&inquest::repository::TestId::new("test_incomplete")));
}

#[test]
#[cfg_attr(target_os = "windows", ignore = "sh does not handle Windows paths")]
fn test_parallel_execution_verifies_results() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // Create subunit data for two partitions
    let mut pass_a = subunit_inprogress("test_a");
    pass_a.extend(subunit_success("test_a"));
    write_bin(temp.path(), "pass_a.bin", &pass_a);

    let mut fail_b = subunit_inprogress("test_b");
    fail_b.extend(subunit_failure("test_b"));
    write_bin(temp.path(), "fail_b.bin", &fail_b);

    let mut list_bytes = subunit_enumerate("test_a");
    list_bytes.extend(subunit_enumerate("test_b"));
    write_bin(temp.path(), "list.bin", &list_bytes);

    // Script: emit test results based on which test IDs are requested.
    // With 2 workers each gets one test.
    let dir = temp.path().to_string_lossy();
    let script = format!(
        r#"#!/bin/sh
DIR="{dir}"
if [ "$1" = "--list" ]; then
    cat "$DIR/list.bin"
    exit 0
fi
# $1 is --load-list, $2 is the ID file path
if [ -n "$2" ]; then
    IDFILE="$2"
    if grep -q test_a "$IDFILE" 2>/dev/null; then
        cat "$DIR/pass_a.bin"
    fi
    if grep -q test_b "$IDFILE" 2>/dev/null; then
        cat "$DIR/fail_b.bin"
    fi
else
    # No ID file — run both
    cat "$DIR/pass_a.bin"
    cat "$DIR/fail_b.bin"
fi
"#,
    );
    let script_path = temp.path().join("run.sh");
    fs::write(&script_path, &script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut config = toml::map::Map::new();
    config.insert(
        "test_command".into(),
        toml::Value::String(format!("sh {} $LISTOPT $IDOPTION", script_path.display())),
    );
    config.insert(
        "test_list_option".into(),
        toml::Value::String("--list".into()),
    );
    config.insert(
        "test_id_option".into(),
        toml::Value::String("--load-list $IDFILE".into()),
    );
    fs::write(
        temp.path().join("inquest.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();

    let mut ui = TestUI::new();
    let cmd = RunCommand {
        base_path: Some(base_path.clone()),
        concurrency: Some(2),
        ..Default::default()
    };

    let result = cmd.execute(&mut ui);
    assert!(result.is_ok(), "execute failed: {:?}", result);
    // Exit code 1 because test_b failed
    assert_eq!(result.unwrap(), 1);

    // Verify the repository now has a run with correct results
    let repo = factory.open(temp.path()).unwrap();
    let failing = repo.get_failing_tests().unwrap();
    assert_eq!(failing.len(), 1);
    assert_eq!(failing[0].as_str(), "test_b");
}

#[test]
#[cfg_attr(target_os = "windows", ignore = "sh does not handle Windows paths")]
fn test_partial_mode_preserves_untested_failures_via_run() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    let mut repo = factory.initialise(temp.path()).unwrap();

    // Seed two failing tests
    let mut run = TestRun::new(RunId::new("0"));
    run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
    run.add_result(TestResult::failure("test_a", "Error"));
    run.add_result(TestResult::failure("test_b", "Error"));
    repo.insert_test_run_partial(run, false).unwrap();
    drop(repo);

    // Create subunit data that only re-runs test_a (now passes)
    let mut pass_a = subunit_inprogress("test_a");
    pass_a.extend(subunit_success("test_a"));
    write_bin(temp.path(), "pass_a.bin", &pass_a);

    let list_bytes = subunit_enumerate("test_a");
    write_bin(temp.path(), "list.bin", &list_bytes);

    let dir = temp.path().to_string_lossy();
    let script = format!(
        r#"#!/bin/sh
DIR="{dir}"
if [ "$1" = "--list" ]; then
    cat "$DIR/list.bin"
    exit 0
fi
cat "$DIR/pass_a.bin"
"#,
    );
    let script_path = temp.path().join("run.sh");
    fs::write(&script_path, &script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut config = toml::map::Map::new();
    config.insert(
        "test_command".into(),
        toml::Value::String(format!("sh {} $LISTOPT", script_path.display())),
    );
    config.insert(
        "test_list_option".into(),
        toml::Value::String("--list".into()),
    );
    fs::write(
        temp.path().join("inquest.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();

    // Run in partial mode — only test_a runs, test_b should remain failing
    let mut ui = TestUI::new();
    let cmd = RunCommand {
        base_path: Some(base_path),
        partial: true,
        ..Default::default()
    };

    let result = cmd.execute(&mut ui);
    assert!(result.is_ok(), "execute failed: {:?}", result);

    // test_b should still be failing (was not re-run in partial mode)
    let repo = factory.open(temp.path()).unwrap();
    let failing = repo.get_failing_tests().unwrap();
    assert_eq!(failing.len(), 1);
    assert_eq!(failing[0].as_str(), "test_b");
}

#[test]
fn test_serial_run_with_cancellation() {
    use inquest::repository::inquest::InquestRepositoryFactory;
    use inquest::test_executor::{CancellationToken, TestExecutor, TestExecutorConfig};

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    let mut repo = factory.initialise(temp.path()).unwrap();

    // Use sleep 300 as the test command — it would hang without cancellation
    let config = "test_command = \"sleep 300\"\n";
    fs::write(temp.path().join("inquest.toml"), config).unwrap();

    let test_cmd = inquest::testcommand::TestCommand::from_directory(temp.path()).unwrap();

    let token = CancellationToken::new();
    let token_clone = token.clone();

    // Cancel after 500ms from a background thread
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        token_clone.cancel();
    });

    let exec_config = TestExecutorConfig {
        base_path: Some(base_path),
        all_output: false,
        test_args: None,
        cancellation_token: Some(token),
        max_restarts: None,
        stderr_capture: None,
    };
    let executor = TestExecutor::new(&exec_config);

    let (run_id, writer) = repo.begin_test_run_raw().unwrap();

    let test_ids = vec![inquest::repository::TestId::new("fake_test")];
    let historical_times = std::collections::HashMap::new();

    let mut ui = TestUI::new();
    let start = std::time::Instant::now();
    let output = executor
        .run_serial(
            &mut ui,
            &test_cmd,
            Some(&test_ids),
            None,
            None,
            None,
            run_id,
            writer,
            &historical_times,
        )
        .unwrap();
    let elapsed = start.elapsed();

    // Should complete quickly (not hang for 300 seconds)
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "Test took {:?} — cancellation did not kill the process promptly",
        elapsed
    );
    assert!(output.any_command_failed);
}

#[test]
fn test_isolated_run_with_cancellation() {
    use inquest::repository::inquest::InquestRepositoryFactory;
    use inquest::test_executor::{CancellationToken, TestExecutor, TestExecutorConfig};

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    let config = "test_command = \"sleep 300\"\n";
    fs::write(temp.path().join("inquest.toml"), config).unwrap();

    let test_cmd = inquest::testcommand::TestCommand::from_directory(temp.path()).unwrap();

    let token = CancellationToken::new();
    let token_clone = token.clone();

    // Cancel after 500ms
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        token_clone.cancel();
    });

    let exec_config = TestExecutorConfig {
        base_path: Some(base_path),
        all_output: false,
        test_args: None,
        cancellation_token: Some(token),
        max_restarts: None,
        stderr_capture: None,
    };
    let executor = TestExecutor::new(&exec_config);

    // Multiple test IDs — cancellation should stop before running all of them
    let test_ids = vec![
        inquest::repository::TestId::new("test1"),
        inquest::repository::TestId::new("test2"),
        inquest::repository::TestId::new("test3"),
    ];

    let mut ui = TestUI::new();
    let start = std::time::Instant::now();
    let output = executor
        .run_isolated(&mut ui, &test_cmd, &test_ids, None, None, RunId::new("0"))
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "Test took {:?} — cancellation did not stop isolated run promptly",
        elapsed
    );
    assert!(output.any_command_failed);
}

#[test]
#[cfg_attr(target_os = "windows", ignore = "sh does not handle Windows paths")]
fn test_isolated_run_does_not_deadlock_on_large_stderr() {
    // Regression test for https://github.com/jelmer/inquest/issues/103
    // run_isolated previously piped stderr without draining it, so a child
    // writing more than the kernel pipe buffer (~64KB) would block forever.
    use inquest::repository::inquest::InquestRepositoryFactory;
    use inquest::test_executor::{TestExecutor, TestExecutorConfig};

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // Write 128KB to the parent pipe via stderr, then exit. Comfortably
    // over the typical 64KB pipe buffer so the bug-prior-to-fix would
    // deadlock on the second write.
    let config = "test_command = \"head -c 131072 /dev/zero >&2\"\n";
    fs::write(temp.path().join("inquest.toml"), config).unwrap();

    let test_cmd = inquest::testcommand::TestCommand::from_directory(temp.path()).unwrap();

    // Capture stderr so the 512KB of test noise doesn't pollute test output.
    let stderr_capture = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let exec_config = TestExecutorConfig {
        base_path: Some(base_path),
        all_output: false,
        test_args: None,
        cancellation_token: None,
        max_restarts: None,
        stderr_capture: Some(stderr_capture.clone()),
    };
    let executor = TestExecutor::new(&exec_config);

    let test_ids = vec![inquest::repository::TestId::new("noisy_test")];

    let mut ui = TestUI::new();
    let start = std::time::Instant::now();
    let _output = executor
        .run_isolated(
            &mut ui,
            &test_cmd,
            &test_ids,
            None,
            Some(std::time::Duration::from_secs(30)),
            RunId::new("0"),
        )
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "isolated run took {:?} — likely deadlocked on stderr pipe",
        elapsed
    );
    // Sanity: the stderr drainer ran and read the full 128KB.
    assert_eq!(stderr_capture.lock().unwrap().len(), 131072);
}

#[test]
#[cfg_attr(target_os = "windows", ignore = "sh does not handle Windows paths")]
fn test_run_with_profile_persists_active_profile_in_metadata() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // Base config + a `fast` profile that's the active one for this run.
    let config = r#"
test_command = "true"

[profiles.fast]
test_timeout = "1m"
"#;
    fs::write(temp.path().join("inquest.toml"), config).unwrap();

    let load_list_path = temp.path().join("test_ids.txt");
    fs::write(&load_list_path, "fake_test\n").unwrap();

    let mut ui = TestUI::new();
    let cmd = RunCommand {
        base_path: Some(base_path.clone()),
        load_list: Some(load_list_path.to_string_lossy().to_string()),
        profile: Some("fast".to_string()),
        ..Default::default()
    };
    let exit_code = cmd.execute(&mut ui).unwrap();

    // Walk the runs and confirm exactly one carries `profile = "fast"`.
    let repo = factory.open(temp.path()).unwrap();
    let run_ids = repo.list_run_ids().unwrap();
    assert_eq!(run_ids.len(), 1);
    let metadata = repo.get_run_metadata(&run_ids[0]).unwrap();
    assert_eq!(metadata.profile.as_deref(), Some("fast"));
    // Sanity: the run actually executed.
    assert_eq!(metadata.exit_code, Some(exit_code));
}

#[test]
#[cfg_attr(target_os = "windows", ignore = "sh does not handle Windows paths")]
fn test_run_without_profile_records_no_profile_in_metadata() {
    use inquest::commands::RunCommand;
    use inquest::repository::inquest::InquestRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    let factory = InquestRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    let config = r#"test_command = "true""#;
    fs::write(temp.path().join("inquest.toml"), config).unwrap();

    let load_list_path = temp.path().join("test_ids.txt");
    fs::write(&load_list_path, "fake_test\n").unwrap();

    let mut ui = TestUI::new();
    let cmd = RunCommand {
        base_path: Some(base_path.clone()),
        load_list: Some(load_list_path.to_string_lossy().to_string()),
        profile: None,
        ..Default::default()
    };
    cmd.execute(&mut ui).unwrap();

    let repo = factory.open(temp.path()).unwrap();
    let run_ids = repo.list_run_ids().unwrap();
    assert_eq!(run_ids.len(), 1);
    let metadata = repo.get_run_metadata(&run_ids[0]).unwrap();
    assert_eq!(metadata.profile, None);
}
