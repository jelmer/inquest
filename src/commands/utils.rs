//! Utility functions for command implementation

use crate::error::Result;
use crate::repository::file::FileRepositoryFactory;
use crate::repository::{Repository, RepositoryFactory, TestRun};
use crate::ui::UI;
use std::path::Path;

/// Open a repository at the given path (or current directory if None)
pub fn open_repository(base_path: Option<&str>) -> Result<Box<dyn Repository>> {
    let base = base_path.map(Path::new).unwrap_or_else(|| Path::new("."));

    let factory = FileRepositoryFactory;
    factory.open(base)
}

/// Initialize a repository at the given path (or current directory if None)
pub fn init_repository(base_path: Option<&str>) -> Result<Box<dyn Repository>> {
    let base = base_path.map(Path::new).unwrap_or_else(|| Path::new("."));

    let factory = FileRepositoryFactory;
    factory.initialise(base)
}

/// Extract test durations from a test run and update the repository's times database
pub fn update_test_times_from_run(
    repo: &mut Box<dyn Repository>,
    test_run: &TestRun,
) -> Result<()> {
    use std::collections::HashMap;

    let mut times = HashMap::new();
    for result in test_run.results.values() {
        if let Some(duration) = result.duration {
            times.insert(result.test_id.clone(), duration);
        }
    }

    if !times.is_empty() {
        repo.update_test_times(&times)?;
    }

    Ok(())
}

/// Update repository failing tests based on partial mode
pub fn update_repository_failing_tests(
    repo: &mut Box<dyn Repository>,
    test_run: &TestRun,
    partial: bool,
) -> Result<()> {
    if partial {
        repo.update_failing_tests(test_run)?;
    } else {
        repo.replace_failing_tests(test_run)?;
    }
    Ok(())
}

/// Display a test run summary
pub fn display_test_summary(ui: &mut dyn UI, run_id: &str, test_run: &TestRun) -> Result<()> {
    let total = test_run.total_tests();
    let failures = test_run.count_failures();
    let successes = test_run.count_successes();

    ui.output(&format!("\nTest run {}:", run_id))?;
    ui.output(&format!("  Total:   {}", total))?;
    ui.output(&format!("  Passed:  {}", successes))?;
    ui.output(&format!("  Failed:  {}", failures))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_init_repository() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();

        let result = init_repository(Some(&path));
        assert!(result.is_ok());
    }

    #[test]
    fn test_open_repository_nonexistent() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();

        let result = open_repository(Some(&path));
        assert!(result.is_err());
    }

    #[test]
    fn test_open_repository_existing() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();

        init_repository(Some(&path)).unwrap();
        let result = open_repository(Some(&path));
        assert!(result.is_ok());
    }
}
