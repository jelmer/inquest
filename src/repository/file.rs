//! File-based repository implementation
//!
//! This maintains compatibility with the Python version's .testrepository/ format:
//! - format: version file (contains "1")
//! - next-stream: counter for run IDs
//! - 0, 1, 2, ...: individual test run files (subunit format)
//! - failing: synthetic run containing current failures
//! - times.dbm: test timing database (NOT YET IMPLEMENTED - will use different format)

use crate::error::{Error, Result};
use crate::repository::{Repository, RepositoryFactory, TestId, TestResult, TestRun};
use crate::subunit_stream;
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::Duration;

const REPOSITORY_FORMAT: &str = "1";
const REPO_DIR: &str = ".testrepository";

pub struct FileRepositoryFactory;

impl RepositoryFactory for FileRepositoryFactory {
    fn initialise(&self, base: &Path) -> Result<Box<dyn Repository>> {
        let repo_path = base.join(REPO_DIR);

        if repo_path.exists() {
            return Err(Error::RepositoryExists(repo_path));
        }

        fs::create_dir_all(&repo_path)?;

        // Write format file (with newline for Python compatibility)
        let format_path = repo_path.join("format");
        fs::write(&format_path, format!("{}\n", REPOSITORY_FORMAT))?;

        // Initialize next-stream counter (with newline for Python compatibility)
        let next_stream_path = repo_path.join("next-stream");
        fs::write(&next_stream_path, "0\n")?;

        Ok(Box::new(FileRepository { path: repo_path }))
    }

    fn open(&self, base: &Path) -> Result<Box<dyn Repository>> {
        let repo_path = base.join(REPO_DIR);

        if !repo_path.exists() {
            return Err(Error::RepositoryNotFound(repo_path));
        }

        // Verify format
        let format_path = repo_path.join("format");
        if !format_path.exists() {
            return Err(Error::InvalidFormat("Missing format file".to_string()));
        }

        let format = fs::read_to_string(&format_path)?.trim().to_string();
        if format != REPOSITORY_FORMAT {
            return Err(Error::InvalidFormat(format!(
                "Unsupported format version: {}",
                format
            )));
        }

        Ok(Box::new(FileRepository { path: repo_path }))
    }
}

pub struct FileRepository {
    path: PathBuf,
}

impl FileRepository {
    fn get_next_stream_path(&self) -> PathBuf {
        self.path.join("next-stream")
    }

    fn read_next_stream(&self) -> Result<u64> {
        let path = self.get_next_stream_path();
        let content = fs::read_to_string(&path)?;
        content
            .trim()
            .parse()
            .map_err(|e| Error::InvalidFormat(format!("Invalid next-stream: {}", e)))
    }

    fn write_next_stream(&self, value: u64) -> Result<()> {
        let path = self.get_next_stream_path();
        fs::write(&path, format!("{}\n", value))?;
        Ok(())
    }

    fn increment_next_stream(&mut self) -> Result<u64> {
        let current = self.read_next_stream()?;
        let next = current + 1;
        self.write_next_stream(next)?;
        Ok(current)
    }

    fn get_run_path(&self, run_id: &str) -> PathBuf {
        self.path.join(run_id)
    }

    fn get_failing_path(&self) -> PathBuf {
        self.path.join("failing")
    }

    fn read_failing_run(&self) -> Result<HashMap<TestId, TestResult>> {
        let path = self.get_failing_path();

        if !path.exists() {
            // No failing file yet, return empty
            return Ok(HashMap::new());
        }

        // Read the failing subunit stream
        let file = File::open(&path)?;
        let test_run = subunit_stream::parse_stream(file, "failing".to_string())?;

        Ok(test_run.results)
    }

    fn write_failing_run(&self, failing: &HashMap<TestId, TestResult>) -> Result<()> {
        let path = self.get_failing_path();

        if failing.is_empty() {
            // Remove the failing file if there are no failures
            if path.exists() {
                fs::remove_file(&path)?;
            }
            return Ok(());
        }

        // Create a synthetic test run with just the failing tests
        let mut test_run = TestRun::new("failing".to_string());
        // Use a fixed timestamp for the failing run
        test_run.timestamp =
            chrono::DateTime::from_timestamp(1000000000, 0).unwrap_or_else(chrono::Utc::now);
        test_run.results = failing.clone();

        let file = File::create(&path)?;
        subunit_stream::write_stream(&test_run, file)?;

        Ok(())
    }
}

impl Repository for FileRepository {
    fn get_test_run(&self, run_id: &str) -> Result<TestRun> {
        let path = self.get_run_path(run_id);
        if !path.exists() {
            return Err(Error::TestRunNotFound(run_id.to_string()));
        }

        let file = File::open(&path)?;
        subunit_stream::parse_stream(file, run_id.to_string())
    }

    fn insert_test_run(&mut self, run: TestRun) -> Result<String> {
        let run_id = self.increment_next_stream()?;
        let run_id_str = run_id.to_string();

        let path = self.get_run_path(&run_id_str);
        let file = File::create(&path)?;
        subunit_stream::write_stream(&run, file)?;

        Ok(run_id_str)
    }

    fn get_latest_run(&self) -> Result<TestRun> {
        let next_stream = self.read_next_stream()?;
        if next_stream == 0 {
            return Err(Error::NoTestRuns);
        }

        let run_id = (next_stream - 1).to_string();
        self.get_test_run(&run_id)
    }

    fn update_failing_tests(&mut self, run: &TestRun) -> Result<()> {
        // Read existing failing tests
        let mut failing = self.read_failing_run()?;

        // Update with results from this run
        for result in run.results.values() {
            if result.status.is_failure() {
                // Add or update failure
                failing.insert(result.test_id.clone(), result.clone());
            } else if result.status.is_success() {
                // Remove from failures if it passed
                failing.remove(&result.test_id);
            }
        }

        // Write back
        self.write_failing_run(&failing)?;
        Ok(())
    }

    fn replace_failing_tests(&mut self, run: &TestRun) -> Result<()> {
        // Collect all failing tests from this run
        let failing: HashMap<TestId, TestResult> = run
            .results
            .values()
            .filter(|r| r.status.is_failure())
            .map(|r| (r.test_id.clone(), r.clone()))
            .collect();

        self.write_failing_run(&failing)?;
        Ok(())
    }

    fn get_failing_tests(&self) -> Result<Vec<TestId>> {
        let failing = self.read_failing_run()?;
        Ok(failing.keys().cloned().collect())
    }

    fn get_test_times(&self) -> Result<HashMap<TestId, Duration>> {
        // TODO: Read from times database
        Ok(HashMap::new())
    }

    fn get_next_run_id(&self) -> Result<u64> {
        self.read_next_stream()
    }

    fn list_run_ids(&self) -> Result<Vec<String>> {
        let next_stream = self.read_next_stream()?;
        let mut ids = Vec::new();

        for i in 0..next_stream {
            let id = i.to_string();
            let path = self.get_run_path(&id);
            if path.exists() {
                ids.push(id);
            }
        }

        Ok(ids)
    }

    fn count(&self) -> Result<usize> {
        Ok(self.list_run_ids()?.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_initialize_repository() {
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;

        let repo = factory.initialise(temp.path()).unwrap();
        assert_eq!(repo.get_next_run_id().unwrap(), 0);

        // Verify files were created
        let repo_path = temp.path().join(REPO_DIR);
        assert!(repo_path.exists());
        assert!(repo_path.join("format").exists());
        assert!(repo_path.join("next-stream").exists());

        // Verify format content
        let format = fs::read_to_string(repo_path.join("format")).unwrap();
        assert_eq!(format.trim(), "1");
    }

    #[test]
    fn test_open_nonexistent_repository() {
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;

        let result = factory.open(temp.path());
        assert!(matches!(result, Err(Error::RepositoryNotFound(_))));
    }

    #[test]
    fn test_cannot_double_initialize() {
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;

        factory.initialise(temp.path()).unwrap();
        let result = factory.initialise(temp.path());
        assert!(matches!(result, Err(Error::RepositoryExists(_))));
    }

    #[test]
    fn test_open_existing_repository() {
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;

        factory.initialise(temp.path()).unwrap();
        let repo = factory.open(temp.path()).unwrap();
        assert_eq!(repo.get_next_run_id().unwrap(), 0);
    }

    #[test]
    fn test_insert_test_run() {
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;

        let mut repo = factory.initialise(temp.path()).unwrap();

        let run = TestRun::new("0".to_string());
        let run_id = repo.insert_test_run(run).unwrap();

        assert_eq!(run_id, "0");
        assert_eq!(repo.get_next_run_id().unwrap(), 1);

        // Verify file was created
        let repo_path = temp.path().join(REPO_DIR);
        assert!(repo_path.join("0").exists());
    }

    #[test]
    fn test_list_run_ids() {
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;

        let mut repo = factory.initialise(temp.path()).unwrap();

        assert_eq!(repo.list_run_ids().unwrap().len(), 0);

        repo.insert_test_run(TestRun::new("0".to_string())).unwrap();
        repo.insert_test_run(TestRun::new("1".to_string())).unwrap();

        let ids = repo.list_run_ids().unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids, vec!["0", "1"]);
    }

    #[test]
    fn test_count() {
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;

        let mut repo = factory.initialise(temp.path()).unwrap();
        assert_eq!(repo.count().unwrap(), 0);

        repo.insert_test_run(TestRun::new("0".to_string())).unwrap();
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn test_get_latest_run_empty_repository() {
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;

        let repo = factory.initialise(temp.path()).unwrap();
        let result = repo.get_latest_run();
        assert!(matches!(result, Err(Error::NoTestRuns)));
    }

    #[test]
    fn test_partial_run_update_failing() {
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // First run: test1 fails, test2 passes
        let mut run1 = TestRun::new("0".to_string());
        run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run1.add_result(TestResult::failure("test1", "Failed"));
        run1.add_result(TestResult::success("test2"));

        repo.insert_test_run_partial(run1, false).unwrap();

        // Check failing tests after first run
        let failing = repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 1);
        assert!(failing.iter().any(|id| id.as_str() == "test1"));

        // Second partial run: test1 now passes, test3 fails
        let mut run2 = TestRun::new("1".to_string());
        run2.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
        run2.add_result(TestResult::success("test1")); // Now passes
        run2.add_result(TestResult::failure("test3", "Failed")); // New failure

        repo.insert_test_run_partial(run2, true).unwrap(); // Partial mode

        // Check failing tests after partial run
        let failing = repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 1);
        // test1 should be removed (it passed), test3 should be added
        assert!(!failing.iter().any(|id| id.as_str() == "test1"));
        assert!(failing.iter().any(|id| id.as_str() == "test3"));
    }

    #[test]
    fn test_full_run_replaces_failing() {
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // First run: test1 and test2 fail
        let mut run1 = TestRun::new("0".to_string());
        run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run1.add_result(TestResult::failure("test1", "Failed"));
        run1.add_result(TestResult::failure("test2", "Failed"));

        repo.insert_test_run_partial(run1, false).unwrap();

        let failing = repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 2);

        // Second full run: only test3 fails
        let mut run2 = TestRun::new("1".to_string());
        run2.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
        run2.add_result(TestResult::success("test1"));
        run2.add_result(TestResult::success("test2"));
        run2.add_result(TestResult::failure("test3", "Failed"));

        repo.insert_test_run_partial(run2, false).unwrap(); // Full mode

        // Check that failing tests were replaced, not updated
        let failing = repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 1);
        assert!(failing.iter().any(|id| id.as_str() == "test3"));
        assert!(!failing.iter().any(|id| id.as_str() == "test1"));
        assert!(!failing.iter().any(|id| id.as_str() == "test2"));
    }
}
