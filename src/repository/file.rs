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
use subunit::serialize::Serializable;

const REPOSITORY_FORMAT: &str = "1";
const REPO_DIR: &str = ".testrepository";

/// Factory for creating file-based repositories.
///
/// Creates and opens repositories that store test data in the `.testrepository`
/// directory using the same format as the Python testrepository implementation.
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

/// File-based repository implementation.
///
/// Stores test runs and metadata in files within the `.testrepository` directory,
/// maintaining compatibility with the Python version's format.
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

        // Read the failing subunit stream using memory-mapped I/O
        let file = File::open(&path)?;
        let metadata = file.metadata()?;

        let test_run = if metadata.len() > 4096 {
            // Safety: We're only reading from the file, not modifying it
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            subunit_stream::parse_stream_bytes(&mmap, "failing".to_string())?
        } else {
            subunit_stream::parse_stream(file, "failing".to_string())?
        };

        Ok(test_run.results)
    }

    fn write_failing_run_from_raw(&self, run_id: &str) -> Result<()> {
        let failing_path = self.get_failing_path();
        let run_path = self.get_run_path(run_id);

        // Read the raw test run and filter for failing tests
        let reader = File::open(&run_path)?;
        let writer = File::create(&failing_path)?;

        subunit_stream::filter_failing_tests(reader, writer)?;

        // Check if the failing file is empty and remove it if so
        let metadata = std::fs::metadata(&failing_path)?;
        if metadata.len() == 0 {
            fs::remove_file(&failing_path)?;
        }

        Ok(())
    }

    fn update_failing_run_from_raw(&self, run_id: &str) -> Result<()> {
        use std::io::Write;

        // Get existing failing tests
        let mut existing_failing = self.read_failing_run().unwrap_or_default();

        // Get results from the new run
        let new_run = self.get_test_run(run_id)?;

        // Update the failing map based on new results
        for result in new_run.results.values() {
            if result.status.is_failure() {
                existing_failing.insert(result.test_id.clone(), result.clone());
            } else if result.status.is_success() {
                existing_failing.remove(&result.test_id);
            }
        }

        // If no failures remain, remove the failing file
        if existing_failing.is_empty() {
            let failing_path = self.get_failing_path();
            if failing_path.exists() {
                fs::remove_file(&failing_path)?;
            }
            return Ok(());
        }

        // Write updated failing tests by merging streams
        // TODO: This could be optimized by streaming instead of buffering
        let failing_path = self.get_failing_path();
        let temp_path = failing_path.with_extension("tmp");

        {
            let mut writer = File::create(&temp_path)?;

            // Write events from the new run for newly failing tests
            let new_run_reader = File::open(self.get_run_path(run_id))?;

            // Filter new run for failing tests
            let mut new_run_buffer = Vec::new();
            subunit_stream::filter_failing_tests(new_run_reader, &mut new_run_buffer)?;

            // If there's an existing failing file, copy events for tests that are still failing
            if failing_path.exists() {
                let existing_reader = File::open(&failing_path)?;
                let existing_test_ids: std::collections::HashSet<_> =
                    existing_failing.keys().cloned().collect();

                // Copy events from existing failing file for tests not in the new run
                for item in subunit::io::sync::iter_stream(existing_reader) {
                    if let Ok(subunit::types::stream::ScannedItem::Event(event)) = item {
                        if let Some(ref test_id) = event.test_id {
                            // Keep if still failing and not updated in new run
                            if existing_test_ids.contains(&TestId::new(test_id))
                                && !new_run.results.contains_key(&TestId::new(test_id))
                            {
                                event.serialize(&mut writer).map_err(|e| {
                                    crate::error::Error::Subunit(format!(
                                        "Failed to serialize: {}",
                                        e
                                    ))
                                })?;
                            }
                        }
                    }
                }
            }

            // Write new failing tests
            writer.write_all(&new_run_buffer)?;
        }

        // Replace the failing file with the temp file
        fs::rename(&temp_path, &failing_path)?;

        Ok(())
    }

    fn get_test_times_for_ids_impl(
        &self,
        test_ids: &[TestId],
    ) -> Result<HashMap<TestId, Duration>> {
        let times_path = self.path.join("times.dbm");

        // If the database doesn't exist yet, return empty
        if !times_path.exists() {
            return Ok(HashMap::new());
        }

        // Try SQLite first (Python's dbm.sqlite3 format - most common on modern systems)
        if let Ok(result) = self.read_times_sqlite(&times_path, test_ids) {
            return Ok(result);
        }

        // Fall back to GDBM (older Python versions or explicit GDBM usage)
        if let Ok(result) = self.read_times_gdbm(&times_path, test_ids) {
            return Ok(result);
        }

        // If neither format works, return empty (with warning)
        eprintln!("Warning: Could not read times database in SQLite or GDBM format, continuing without historical timing data");
        Ok(HashMap::new())
    }

    /// Read test times from SQLite database (Python dbm.sqlite3 format)
    fn read_times_sqlite(
        &self,
        path: &std::path::Path,
        test_ids: &[TestId],
    ) -> Result<HashMap<TestId, Duration>> {
        let conn = rusqlite::Connection::open(path)?;

        // Python's dbm.sqlite3 uses a table called 'Dict' with columns 'key' and 'value'
        let mut stmt = conn.prepare("SELECT key, value FROM Dict WHERE key = ?")?;
        let mut result = HashMap::new();

        for test_id in test_ids {
            if let Ok(duration_str) = stmt.query_row([test_id.as_str().as_bytes()], |row| {
                row.get::<_, Vec<u8>>(1)
            }) {
                if let Ok(s) = String::from_utf8(duration_str) {
                    if let Ok(seconds) = s.parse::<f64>() {
                        result.insert(test_id.clone(), Duration::from_secs_f64(seconds));
                    }
                }
            }
        }

        Ok(result)
    }

    /// Read test times from GDBM database (older Python versions)
    fn read_times_gdbm(
        &self,
        path: &std::path::Path,
        test_ids: &[TestId],
    ) -> Result<HashMap<TestId, Duration>> {
        let db = gdbm::Gdbm::new(path, 0, gdbm::Open::READER, 0o644)
            .map_err(|e| Error::Io(std::io::Error::other(format!("Failed to open GDBM: {}", e))))?;

        let mut result = HashMap::new();

        for test_id in test_ids {
            if let Ok(duration_str) = db.fetch_string(test_id.as_str().as_bytes()) {
                if let Ok(seconds) = duration_str.parse::<f64>() {
                    result.insert(test_id.clone(), Duration::from_secs_f64(seconds));
                }
            }
        }

        Ok(result)
    }

    fn update_test_times_impl(&mut self, times: &HashMap<TestId, Duration>) -> Result<()> {
        if times.is_empty() {
            return Ok(());
        }

        let times_path = self.path.join("times.dbm");

        // Use SQLite to match Python's dbm.sqlite3 format
        let conn = rusqlite::Connection::open(&times_path)?;

        // Create the Dict table if it doesn't exist (Python dbm.sqlite3 format)
        conn.execute(
            "CREATE TABLE IF NOT EXISTS Dict (
                key BLOB PRIMARY KEY,
                value BLOB
            )",
            [],
        )?;

        // Update each test time using UPSERT (INSERT OR REPLACE)
        let mut stmt = conn.prepare("INSERT OR REPLACE INTO Dict (key, value) VALUES (?, ?)")?;

        for (test_id, duration) in times {
            let key = test_id.as_str().as_bytes();
            let value = duration.as_secs_f64().to_string();
            stmt.execute([key, value.as_bytes()])?;
        }

        Ok(())
    }
}

impl Repository for FileRepository {
    fn get_test_run(&self, run_id: &str) -> Result<TestRun> {
        let path = self.get_run_path(run_id);
        if !path.exists() {
            return Err(Error::TestRunNotFound(run_id.to_string()));
        }

        // Use memory-mapped file for better performance with large files
        let file = File::open(&path)?;

        // Check file size - only use mmap for files larger than 4KB
        let metadata = file.metadata()?;
        let test_run = if metadata.len() > 4096 {
            // Safety: We're only reading from the file, not modifying it
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            subunit_stream::parse_stream_bytes(&mmap, run_id.to_string())
        } else {
            // For small files, regular I/O is faster
            subunit_stream::parse_stream(file, run_id.to_string())
        }?;

        Ok(test_run)
    }

    fn begin_test_run_raw(&mut self) -> Result<(String, Box<dyn std::io::Write + Send>)> {
        let run_id = self.increment_next_stream()?;
        let run_id_str = run_id.to_string();

        let path = self.get_run_path(&run_id_str);
        let file = File::create(&path)?;

        Ok((run_id_str, Box::new(file)))
    }

    fn get_latest_run(&self) -> Result<TestRun> {
        let next_stream = self.read_next_stream()?;
        if next_stream == 0 {
            return Err(Error::NoTestRuns);
        }

        let run_id = (next_stream - 1).to_string();
        self.get_test_run(&run_id)
    }

    fn get_test_run_raw(&self, run_id: &str) -> Result<Box<dyn std::io::Read>> {
        let path = self.get_run_path(run_id);
        let file = File::open(&path)?;
        Ok(Box::new(file))
    }

    fn update_failing_tests(&mut self, run: &TestRun) -> Result<()> {
        // For update mode (partial runs), merge with existing failing tests
        self.update_failing_run_from_raw(&run.id)
    }

    fn replace_failing_tests(&mut self, run: &TestRun) -> Result<()> {
        // For replace mode (full runs), completely replace the failing file
        self.write_failing_run_from_raw(&run.id)
    }

    fn get_failing_tests(&self) -> Result<Vec<TestId>> {
        let failing = self.read_failing_run()?;
        Ok(failing.keys().cloned().collect())
    }

    fn get_failing_tests_raw(&self) -> Result<Box<dyn std::io::Read>> {
        let failing_path = self.path.join("failing");
        let file = File::open(&failing_path)?;
        Ok(Box::new(file))
    }

    fn get_test_times(&self) -> Result<HashMap<TestId, Duration>> {
        // TODO: The gdbm crate doesn't expose iteration methods (firstkey/nextkey).
        // For now, return empty HashMap. This method isn't currently used in the CLI.
        // When needed, we can either:
        // 1. Add iteration support to the gdbm crate
        // 2. Use a different approach (e.g., maintain a separate index)
        // 3. Use get_test_times_for_ids() for specific lookups
        Ok(HashMap::new())
    }

    fn get_test_times_for_ids(&self, test_ids: &[TestId]) -> Result<HashMap<TestId, Duration>> {
        // Call the private implementation method to avoid infinite recursion
        self.get_test_times_for_ids_impl(test_ids)
    }

    fn update_test_times(&mut self, times: &HashMap<TestId, Duration>) -> Result<()> {
        self.update_test_times_impl(times)
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
    use std::time::Duration;
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

        // Use begin_test_run_raw to stream the test run
        let run = TestRun::new("0".to_string());
        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();

        // Write the test run as subunit stream
        crate::subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

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

        // Insert test runs using streaming API
        let run = TestRun::new("0".to_string());
        let (_, mut writer) = repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

        let run = TestRun::new("1".to_string());
        let (_, mut writer) = repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

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

        let run = TestRun::new("0".to_string());
        let (_, mut writer) = repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

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

        // Stream and store first run
        let (_, mut writer) = repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run1, &mut writer).unwrap();
        drop(writer);
        repo.replace_failing_tests(&run1).unwrap();

        // Check failing tests after first run
        let failing = repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 1);
        assert!(failing.iter().any(|id| id.as_str() == "test1"));

        // Second partial run: test1 now passes, test3 fails
        let mut run2 = TestRun::new("1".to_string());
        run2.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
        run2.add_result(TestResult::success("test1")); // Now passes
        run2.add_result(TestResult::failure("test3", "Failed")); // New failure

        // Stream and store second run
        let (_, mut writer) = repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run2, &mut writer).unwrap();
        drop(writer);
        repo.update_failing_tests(&run2).unwrap(); // Partial mode

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

        let (_, mut writer) = repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run1, &mut writer).unwrap();
        drop(writer);
        repo.replace_failing_tests(&run1).unwrap();

        let failing = repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 2);

        // Second full run: only test3 fails
        let mut run2 = TestRun::new("1".to_string());
        run2.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
        run2.add_result(TestResult::success("test1"));
        run2.add_result(TestResult::success("test2"));
        run2.add_result(TestResult::failure("test3", "Failed"));

        let (_, mut writer) = repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run2, &mut writer).unwrap();
        drop(writer);
        repo.replace_failing_tests(&run2).unwrap(); // Full mode

        // Check that failing tests were replaced, not updated
        let failing = repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 1);
        assert!(failing.iter().any(|id| id.as_str() == "test3"));
        assert!(!failing.iter().any(|id| id.as_str() == "test1"));
        assert!(!failing.iter().any(|id| id.as_str() == "test2"));
    }

    #[test]
    fn test_times_database_write_and_read() {
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;

        // Get the repo path directly
        let repo_path = temp.path().join(".testrepository");

        // Initialize using factory and then create a direct FileRepository instance for testing
        factory.initialise(temp.path()).unwrap();
        let mut file_repo = FileRepository {
            path: repo_path.clone(),
        };

        // Create a test run with durations
        let mut run = TestRun::new("0".to_string());
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::success("test1").with_duration(Duration::from_secs_f64(1.5)));
        run.add_result(TestResult::success("test2").with_duration(Duration::from_secs_f64(0.5)));
        run.add_result(TestResult::success("test3").with_duration(Duration::from_secs_f64(2.25)));

        // Insert the run (should write times to database)
        let (_, mut writer) = file_repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

        // Update times
        use std::collections::HashMap;
        let mut times = HashMap::new();
        for result in run.results.values() {
            if let Some(duration) = result.duration {
                times.insert(result.test_id.clone(), duration);
            }
        }
        file_repo.update_test_times(&times).unwrap();

        // Verify times.dbm file was created
        let times_path = repo_path.join("times.dbm");
        assert!(times_path.exists(), "times.dbm should be created");

        // Read times for specific test IDs
        let test_ids = vec![
            TestId::new("test1"),
            TestId::new("test2"),
            TestId::new("test3"),
        ];
        let times = file_repo.get_test_times_for_ids(&test_ids).unwrap();

        assert_eq!(times.len(), 3);
        assert_eq!(times.get(&TestId::new("test1")).unwrap().as_secs_f64(), 1.5);
        assert_eq!(times.get(&TestId::new("test2")).unwrap().as_secs_f64(), 0.5);
        assert_eq!(
            times.get(&TestId::new("test3")).unwrap().as_secs_f64(),
            2.25
        );
    }

    #[test]
    fn test_times_database_updates_on_multiple_runs() {
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;

        // Get the repo path directly
        let repo_path = temp.path().join(".testrepository");

        // Initialize using factory and then create a direct FileRepository instance for testing
        factory.initialise(temp.path()).unwrap();
        let mut file_repo = FileRepository {
            path: repo_path.clone(),
        };

        // First run with initial times
        let mut run1 = TestRun::new("0".to_string());
        run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run1.add_result(TestResult::success("test1").with_duration(Duration::from_secs_f64(1.0)));

        let (_, mut writer) = file_repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run1, &mut writer).unwrap();
        drop(writer);

        use std::collections::HashMap;
        let mut times = HashMap::new();
        for result in run1.results.values() {
            if let Some(duration) = result.duration {
                times.insert(result.test_id.clone(), duration);
            }
        }
        file_repo.update_test_times(&times).unwrap();

        // Second run with updated time for test1
        let mut run2 = TestRun::new("1".to_string());
        run2.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
        run2.add_result(TestResult::success("test1").with_duration(Duration::from_secs_f64(2.0)));

        let (_, mut writer) = file_repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run2, &mut writer).unwrap();
        drop(writer);

        let mut times = HashMap::new();
        for result in run2.results.values() {
            if let Some(duration) = result.duration {
                times.insert(result.test_id.clone(), duration);
            }
        }
        file_repo.update_test_times(&times).unwrap();

        // Verify that the time was updated (not accumulated)
        let test_ids = vec![TestId::new("test1")];
        let times = file_repo.get_test_times_for_ids(&test_ids).unwrap();

        assert_eq!(times.len(), 1);
        assert_eq!(times.get(&TestId::new("test1")).unwrap().as_secs_f64(), 2.0);
    }

    #[test]
    fn test_failing_file_created_on_run() {
        // Test that the failing file is created when tests fail
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;
        let mut file_repo = factory.initialise(temp.path()).unwrap();

        // Create a test run with failures
        let mut run = TestRun::new("0".to_string());
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::success("test1"));
        run.add_result(TestResult::failure("test2", "Failed"));
        run.add_result(TestResult::failure("test3", "Also failed"));

        // Write as raw stream
        let (_, mut writer) = file_repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

        // Replace failing tests (full run mode)
        file_repo.replace_failing_tests(&run).unwrap();

        // Check that failing file exists
        let failing_path = temp.path().join(".testrepository/failing");
        assert!(failing_path.exists(), "Failing file should be created");

        // Check that get_failing_tests returns the correct tests
        let failing_tests = file_repo.get_failing_tests().unwrap();
        assert_eq!(failing_tests.len(), 2, "Should have 2 failing tests");
        assert!(failing_tests.contains(&TestId::new("test2")));
        assert!(failing_tests.contains(&TestId::new("test3")));
    }

    #[test]
    fn test_failing_command_shows_all_failures() {
        // Test that testr failing shows all failing tests
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;
        let mut file_repo = factory.initialise(temp.path()).unwrap();

        // Create a test run with multiple failures
        let mut run = TestRun::new("0".to_string());
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::success("test.pass1"));
        run.add_result(TestResult::failure("test.fail1", "Failure 1"));
        run.add_result(TestResult::failure("test.fail2", "Failure 2"));
        run.add_result(TestResult::failure("test.fail3", "Failure 3"));
        run.add_result(TestResult::success("test.pass2"));

        // Write as raw stream and update failing tests
        let (_, mut writer) = file_repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);
        file_repo.replace_failing_tests(&run).unwrap();

        // Get failing tests
        let failing_tests = file_repo.get_failing_tests().unwrap();

        assert_eq!(
            failing_tests.len(),
            3,
            "Should have exactly 3 failing tests"
        );
        assert!(failing_tests.contains(&TestId::new("test.fail1")));
        assert!(failing_tests.contains(&TestId::new("test.fail2")));
        assert!(failing_tests.contains(&TestId::new("test.fail3")));
    }

    #[test]
    fn test_run_failing_runs_all_failed_tests() {
        // Test that --failing flag runs all failed tests
        let temp = TempDir::new().unwrap();
        let factory = FileRepositoryFactory;
        let mut file_repo = factory.initialise(temp.path()).unwrap();

        // First run with some failures
        let mut run1 = TestRun::new("0".to_string());
        run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run1.add_result(TestResult::success("test1"));
        run1.add_result(TestResult::failure("test2", "Failed"));
        run1.add_result(TestResult::failure("test3", "Failed"));
        run1.add_result(TestResult::success("test4"));

        let (_, mut writer) = file_repo.begin_test_run_raw().unwrap();
        crate::subunit_stream::write_stream(&run1, &mut writer).unwrap();
        drop(writer);
        file_repo.replace_failing_tests(&run1).unwrap();

        // Get the list of failing tests (this is what --failing would use)
        let latest_run = file_repo.get_latest_run().unwrap();
        let failing_tests = latest_run.get_failing_tests();

        assert_eq!(
            failing_tests.len(),
            2,
            "Should have 2 failing tests from latest run"
        );

        // Also check from the repository failing file
        let repo_failing = file_repo.get_failing_tests().unwrap();
        assert_eq!(
            repo_failing.len(),
            2,
            "Repository should track 2 failing tests"
        );
        assert!(repo_failing.contains(&TestId::new("test2")));
        assert!(repo_failing.contains(&TestId::new("test3")));
    }
}
