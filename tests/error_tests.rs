//! Error path testing
//!
//! This module tests error handling in various failure scenarios to ensure
//! the application properly handles and reports errors.

use std::fs;
use std::path::Path;
use tempfile::TempDir;
use testrepository::commands::{
    Command, FailingCommand, InitCommand, LastCommand, LoadCommand, RunCommand, StatsCommand,
};
use testrepository::error::Result;
use testrepository::repository::file::FileRepositoryFactory;
use testrepository::repository::{RepositoryFactory, TestResult, TestRun};
use testrepository::ui::UI;

// Test UI implementation
struct TestUI {
    pub output: Vec<String>,
    pub errors: Vec<String>,
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
    fn output(&mut self, message: &str) -> Result<()> {
        self.output.push(message.to_string());
        Ok(())
    }

    fn error(&mut self, message: &str) -> Result<()> {
        self.errors.push(message.to_string());
        Ok(())
    }

    fn warning(&mut self, message: &str) -> Result<()> {
        self.errors.push(format!("Warning: {}", message));
        Ok(())
    }
}

#[test]
fn test_load_command_no_repository() {
    let temp = TempDir::new().unwrap();
    let mut ui = TestUI::new();

    let cmd = LoadCommand::new(Some(temp.path().to_string_lossy().to_string()));
    let result = cmd.execute(&mut ui);

    // Should fail with appropriate error
    assert!(result.is_err());
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("Repository")
            || err_msg.contains("not found")
            || err_msg.contains("does not exist")
    );
}

#[test]
fn test_run_command_no_repository() {
    let temp = TempDir::new().unwrap();
    let mut ui = TestUI::new();

    let cmd = RunCommand::new(Some(temp.path().to_string_lossy().to_string()));
    let result = cmd.execute(&mut ui);

    // Should fail with appropriate error
    assert!(result.is_err());
}

#[test]
fn test_last_command_no_repository() {
    let temp = TempDir::new().unwrap();
    let mut ui = TestUI::new();

    let cmd = LastCommand::new(Some(temp.path().to_string_lossy().to_string()));
    let result = cmd.execute(&mut ui);

    // Should fail with appropriate error
    assert!(result.is_err());
}

#[test]
fn test_last_command_empty_repository() {
    let temp = TempDir::new().unwrap();

    // Initialize empty repository
    let factory = FileRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    let mut ui = TestUI::new();
    let cmd = LastCommand::new(Some(temp.path().to_string_lossy().to_string()));
    let result = cmd.execute(&mut ui);

    // Should fail because there are no runs
    assert!(result.is_err());
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(err_msg.contains("NoTestRuns"));
}

#[test]
fn test_stats_command_empty_repository() {
    let temp = TempDir::new().unwrap();

    // Initialize empty repository
    let factory = FileRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    let mut ui = TestUI::new();
    let cmd = StatsCommand::new(Some(temp.path().to_string_lossy().to_string()));
    let result = cmd.execute(&mut ui);

    // Should succeed but show 0 runs
    assert_eq!(result.unwrap(), 0);
    assert!(ui.output.iter().any(|s| s.contains("Total test runs: 0")));
}

#[test]
fn test_failing_command_no_repository() {
    let temp = TempDir::new().unwrap();
    let mut ui = TestUI::new();

    let cmd = FailingCommand::new(Some(temp.path().to_string_lossy().to_string()));
    let result = cmd.execute(&mut ui);

    // Should fail with appropriate error
    assert!(result.is_err());
}

#[test]
fn test_init_command_already_initialized() {
    let temp = TempDir::new().unwrap();

    // Initialize once
    let factory = FileRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // Try to initialize again
    let mut ui = TestUI::new();
    let cmd = InitCommand::new(Some(temp.path().to_string_lossy().to_string()));
    let result = cmd.execute(&mut ui);

    // Should fail or warn about existing repository
    assert!(
        result.is_err()
            || ui
                .errors
                .iter()
                .any(|e| e.contains("already") || e.contains("exists"))
    );
}

#[test]
fn test_run_command_no_test_command() {
    let temp = TempDir::new().unwrap();

    // Initialize repository but don't create .testr.conf
    let factory = FileRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    let mut ui = TestUI::new();
    let cmd = RunCommand::new(Some(temp.path().to_string_lossy().to_string()));
    let result = cmd.execute(&mut ui);

    // Should fail because .testr.conf is missing
    assert!(result.is_err());
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(err_msg.contains(".testr.conf") || err_msg.contains("config"));
}

#[test]
fn test_run_command_invalid_test_command() {
    let temp = TempDir::new().unwrap();

    // Initialize repository
    let factory = FileRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // Create .testr.conf with invalid command
    let config = r#"
[DEFAULT]
test_command=/nonexistent/command
"#;
    fs::write(temp.path().join(".testr.conf"), config).unwrap();

    let mut ui = TestUI::new();
    let cmd = RunCommand::new(Some(temp.path().to_string_lossy().to_string()));
    let result = cmd.execute(&mut ui);

    // Should fail because command doesn't exist
    // Either returns an error or returns non-zero exit code
    match result {
        Err(_) => {} // Error is good
        Ok(exit_code) => {
            assert_ne!(
                exit_code, 0,
                "Expected non-zero exit code for invalid command"
            );
        }
    }
}

#[test]
fn test_load_invalid_subunit_data() {
    // Test that the parser doesn't panic on invalid/corrupted data
    // The new subunit-rust treats plain text as valid (interleaved text in subunit v2),
    // so we use actually corrupted binary data
    let invalid_data: &[u8] = &[
        0xB2, // Start of subunit v2 signature
        0x9A, 0x00, // Incomplete/corrupted packet
        0xFF, 0xFF, 0xFF, // Invalid data
    ];
    let result = testrepository::subunit_stream::parse_stream(invalid_data, "0".to_string());

    // The key requirement is: no panic. Whether it returns an error or empty result
    // depends on how lenient the parser is. Both are acceptable as long as it doesn't crash.
    match result {
        Ok(run) => {
            // Parser was lenient and skipped/ignored the corrupted data
            assert_eq!(run.total_tests(), 0);
        }
        Err(_) => {
            // Parser detected corruption and returned an error - also acceptable
        }
    }
}

#[test]
fn test_repository_corrupted_next_stream() {
    let temp = TempDir::new().unwrap();

    // Initialize repository
    let factory = FileRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // Corrupt the next-stream file
    let next_stream_path = temp.path().join(".testrepository").join("next-stream");
    fs::write(&next_stream_path, "not a number").unwrap();

    // Try to open repository
    let result = factory.open(temp.path());

    // Should handle the corruption gracefully
    // It's acceptable to either fail on open or fail when getting next run ID
    if let Ok(repo) = result {
        // If it opens, getting next run ID may fail due to corruption
        let _next_id = repo.get_next_run_id();
        // Either succeeds (recovered) or fails (detected corruption) - both are valid
    }
    // If open fails, that's also acceptable behavior for corrupted data
}

#[test]
fn test_repository_missing_directory() {
    let temp = TempDir::new().unwrap();
    let nonexistent = temp.path().join("nonexistent");

    let factory = FileRepositoryFactory;
    let result = factory.open(&nonexistent);

    // Should fail because directory doesn't exist
    assert!(result.is_err());
}

#[test]
fn test_repository_file_permissions() {
    // This test only makes sense on Unix-like systems
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();

        // Initialize repository
        let factory = FileRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        // Make .testrepository directory read-only
        let repo_dir = temp.path().join(".testrepository");
        let mut perms = fs::metadata(&repo_dir).unwrap().permissions();
        perms.set_mode(0o444);
        fs::set_permissions(&repo_dir, perms).unwrap();

        // Try to add a test run (should fail due to permissions)
        let result = factory.open(temp.path());

        if let Ok(mut repo) = result {
            let mut test_run = TestRun::new("0".to_string());
            test_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
            test_run.add_result(TestResult::success("test1"));

            let result = repo.insert_test_run(test_run);
            // Should fail due to permissions
            assert!(result.is_err());
        }
    }
}

#[test]
fn test_testlist_parse_nonexistent_file() {
    let nonexistent = Path::new("/nonexistent/file.txt");
    let result = testrepository::testlist::parse_list_file(nonexistent);

    // Should fail because file doesn't exist
    assert!(result.is_err());
}

#[test]
fn test_testlist_parse_empty_file() {
    let temp = TempDir::new().unwrap();
    let empty_file = temp.path().join("empty.txt");
    fs::write(&empty_file, "").unwrap();

    let result = testrepository::testlist::parse_list_file(&empty_file);

    // Should succeed but return empty list
    assert!(result.is_ok());
    assert_eq!(result.unwrap().len(), 0);
}

#[test]
fn test_run_command_with_load_list_nonexistent() {
    let temp = TempDir::new().unwrap();

    // Initialize repository
    let factory = FileRepositoryFactory;
    factory.initialise(temp.path()).unwrap();

    // Create .testr.conf
    let config = r#"
[DEFAULT]
test_command=echo "test1"
"#;
    fs::write(temp.path().join(".testr.conf"), config).unwrap();

    let mut ui = TestUI::new();
    let cmd = RunCommand::with_all_options(
        Some(temp.path().to_string_lossy().to_string()),
        false,
        false,
        false,
        Some("/nonexistent/list.txt".to_string()),
        None,
        false, // until_failure
        false, // isolated
    );
    let result = cmd.execute(&mut ui);

    // Should fail because load-list file doesn't exist
    assert!(result.is_err());
}

#[test]
fn test_subunit_parse_empty_stream() {
    let empty_stream: &[u8] = &[];
    let result = testrepository::subunit_stream::parse_stream(empty_stream, "0".to_string());

    // Empty stream should be valid and return empty test run
    assert!(result.is_ok());
    let run = result.unwrap();
    assert_eq!(run.total_tests(), 0);
}

#[test]
fn test_repository_insert_duplicate_run_id() {
    let temp = TempDir::new().unwrap();

    // Initialize repository
    let factory = FileRepositoryFactory;
    let mut repo = factory.initialise(temp.path()).unwrap();

    // Insert a test run
    let mut test_run1 = TestRun::new("0".to_string());
    test_run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
    test_run1.add_result(TestResult::success("test1"));
    repo.insert_test_run(test_run1.clone()).unwrap();

    // Try to insert another run with the same ID
    let mut test_run2 = TestRun::new("0".to_string());
    test_run2.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
    test_run2.add_result(TestResult::success("test2"));

    // This might fail or overwrite depending on implementation
    // Just verify it doesn't panic
    let _ = repo.insert_test_run(test_run2);
}
