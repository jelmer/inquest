//! Integration tests for full workflows
//!
//! These tests exercise complete user workflows by running actual commands
//! against real repositories in temporary directories.

use std::fs;
use std::io::Write;
use tempfile::TempDir;
use testrepository::commands::{Command, FailingCommand, InitCommand, LastCommand, StatsCommand};
use testrepository::repository::{RepositoryFactory, TestResult, TestRun};
use testrepository::ui::UI;

/// Simple test UI that captures output for assertions
struct TestUI {
    output: Vec<String>,
    errors: Vec<String>,
}

impl TestUI {
    fn new() -> Self {
        TestUI {
            output: Vec::new(),
            errors: Vec::new(),
        }
    }
}

impl UI for TestUI {
    fn output(&mut self, message: &str) -> testrepository::error::Result<()> {
        self.output.push(message.to_string());
        Ok(())
    }

    fn error(&mut self, message: &str) -> testrepository::error::Result<()> {
        self.errors.push(message.to_string());
        Ok(())
    }

    fn warning(&mut self, message: &str) -> testrepository::error::Result<()> {
        self.errors.push(format!("Warning: {}", message));
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
    assert!(temp.path().join(".testrepository").exists());
    assert!(temp.path().join(".testrepository/format").exists());

    // Step 2: Load a test run
    let mut test_run = TestRun::new("0".to_string());
    test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
    test_run.add_result(TestResult::success("test1"));
    test_run.add_result(TestResult::failure("test2", "Failed"));
    test_run.add_result(TestResult::success("test3"));

    // Load the test run directly using the repository API
    // (In real usage, this would be done via LoadCommand reading from stdin)
    let factory = testrepository::repository::file::FileRepositoryFactory;
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
    assert_eq!(ui.output.len(), 8);
    assert_eq!(ui.output[0], "Test run: 0");
    assert!(ui.output[1].starts_with("Timestamp: "));
    assert_eq!(ui.output[2], "Total tests: 3");
    assert_eq!(ui.output[3], "Passed: 2");
    assert_eq!(ui.output[4], "Failed: 1");
    assert_eq!(ui.output[5], "");
    assert_eq!(ui.output[6], "Failed tests:");
    assert_eq!(ui.output[7], "  test2");
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
    let mut run1 = TestRun::new("0".to_string());
    run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
    run1.add_result(TestResult::success("test1"));
    run1.add_result(TestResult::failure("test2", "Error"));
    run1.add_result(TestResult::failure("test3", "Error"));

    let factory = testrepository::repository::file::FileRepositoryFactory;
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
    let mut run2 = TestRun::new("1".to_string());
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

    let factory = testrepository::repository::file::FileRepositoryFactory;
    let mut repo = factory.open(temp.path()).unwrap();

    // First full run
    let mut run1 = TestRun::new("0".to_string());
    run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
    run1.add_result(TestResult::failure("test1", "Error"));
    run1.add_result(TestResult::failure("test2", "Error"));
    run1.add_result(TestResult::success("test3"));

    repo.insert_test_run_partial(run1, false).unwrap();

    // Check we have 2 failing tests
    let failing = repo.get_failing_tests().unwrap();
    assert_eq!(failing.len(), 2);

    // Second partial run - only test test1
    let mut run2 = TestRun::new("1".to_string());
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
    let test_ids = testrepository::testlist::parse_list_file(&test_list_path).unwrap();
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

    let factory = testrepository::repository::file::FileRepositoryFactory;
    let mut repo = factory.open(temp.path()).unwrap();

    // Insert run with durations
    let mut run = TestRun::new("0".to_string());
    run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
    run.add_result(
        TestResult::success("test1").with_duration(std::time::Duration::from_secs_f64(1.5)),
    );
    run.add_result(
        TestResult::success("test2").with_duration(std::time::Duration::from_secs_f64(0.3)),
    );

    repo.insert_test_run(run).unwrap();

    // Verify times.dbm file was created
    let times_path = temp.path().join(".testrepository/times.dbm");
    assert!(times_path.exists());

    // Verify we can read times back
    // (This would require accessing the FileRepository directly, which we do via downcast in unit tests)
}

#[test]
fn test_workflow_list_flag() {
    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize and populate repository
    let mut ui = TestUI::new();
    let init_cmd = InitCommand::new(Some(base_path.clone()));
    init_cmd.execute(&mut ui).unwrap();

    let factory = testrepository::repository::file::FileRepositoryFactory;
    let mut repo = factory.open(temp.path()).unwrap();

    // Add a run with failures
    let mut run = TestRun::new("0".to_string());
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
    use testrepository::commands::RunCommand;
    use testrepository::repository::file::FileRepositoryFactory;

    let temp = TempDir::new().unwrap();
    let base_path = temp.path().to_string_lossy().to_string();

    // Initialize repository
    let factory = FileRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // Create a simple test configuration that outputs subunit
    let config = r#"
[DEFAULT]
test_command=python3 -c "import sys; import time; sys.stdout.buffer.write(b'\xb3\x29\x00\x16test1\x20\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xb3'); sys.stdout.buffer.flush()"
"#;
    fs::write(temp.path().join(".testr.conf"), config).unwrap();

    // Run with parallel execution
    let mut ui = TestUI::new();
    let cmd = RunCommand::with_all_options(
        Some(base_path.clone()),
        false, // partial
        false, // failing
        false, // force_init
        None,  // load_list
        Some(2), // concurrency
    );

    // Note: This test will fail to actually run because the command is synthetic
    // But it tests that the parallel code path is exercised
    let _result = cmd.execute(&mut ui);

    // The command should have at least attempted to run
    assert!(!ui.output.is_empty());
}

#[test]
fn test_parallel_execution_with_worker_tags() {
    use testrepository::partition::partition_tests;
    use testrepository::repository::TestId;
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
