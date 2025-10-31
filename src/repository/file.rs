//! File-based repository implementation
//!
//! This maintains compatibility with the Python version's .testrepository/ format:
//! - format: version file (contains "1")
//! - next-stream: counter for run IDs
//! - 0, 1, 2, ...: individual test run files (subunit format)
//! - failing: synthetic run containing current failures
//! - times.dbm: test timing database (NOT YET IMPLEMENTED - will use different format)

use crate::error::{Error, Result};
use crate::repository::{Repository, RepositoryFactory, TestId, TestRun};
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

        // Write format file
        let format_path = repo_path.join("format");
        fs::write(&format_path, REPOSITORY_FORMAT)?;

        // Initialize next-stream counter
        let next_stream_path = repo_path.join("next-stream");
        fs::write(&next_stream_path, "0")?;

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
        fs::write(&path, value.to_string())?;
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

    fn get_failing_tests(&self) -> Result<Vec<TestId>> {
        // TODO: Read from 'failing' synthetic run
        Ok(Vec::new())
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
}
