//! Inquest-native repository implementation
//!
//! Uses a `.inquest/` directory with SQLite for metadata and
//! subunit files for raw run data. This format provides:
//!
//! - `runs/` directory for individual run streams
//! - Rich metadata per run (git commit, command, concurrency)
//! - SQLite-only storage (no gdbm)
//! - Structured test result storage alongside raw streams

use crate::error::{Error, Result};
use crate::repository::{
    Repository, RepositoryFactory, RunMetadata, TestId, TestResult, TestRun, TestStatus,
};
use crate::subunit_stream;
use rusqlite::params;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A writer that removes a lock file when dropped.
struct LockingWriter {
    inner: File,
    lock_path: PathBuf,
}

impl Write for LockingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Drop for LockingWriter {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

const FORMAT_VERSION: &str = "1";
const SCHEMA_VERSION: i64 = 1;
const REPO_DIR: &str = ".inquest";
/// File size threshold (in bytes) above which we use memory-mapped I/O
const MMAP_THRESHOLD_BYTES: u64 = 4096;

/// Factory for creating inquest-native repositories.
pub struct InquestRepositoryFactory;

impl RepositoryFactory for InquestRepositoryFactory {
    fn initialise(&self, base: &Path) -> Result<Box<dyn Repository>> {
        let repo_path = base.join(REPO_DIR);

        if repo_path.exists() {
            return Err(Error::RepositoryExists(repo_path));
        }

        let runs_path = repo_path.join("runs");
        fs::create_dir_all(&runs_path)?;

        // Write format file
        fs::write(repo_path.join("format"), format!("{}\n", FORMAT_VERSION))?;

        let db_path = repo_path.join("metadata.db");
        let conn = rusqlite::Connection::open(&db_path)?;
        create_schema(&conn)?;

        Ok(Box::new(InquestRepository {
            path: repo_path,
            conn,
        }))
    }

    fn open(&self, base: &Path) -> Result<Box<dyn Repository>> {
        let repo_path = base.join(REPO_DIR);

        if !repo_path.exists() {
            return Err(Error::RepositoryNotFound(repo_path));
        }

        // Verify format file
        let format_path = repo_path.join("format");
        if !format_path.exists() {
            return Err(Error::InvalidFormat("Missing format file".to_string()));
        }

        let format = fs::read_to_string(&format_path)?.trim().to_string();
        if format != FORMAT_VERSION {
            return Err(Error::InvalidFormat(format!(
                "Unsupported format version: {}",
                format
            )));
        }

        let db_path = repo_path.join("metadata.db");
        if !db_path.exists() {
            return Err(Error::InvalidFormat("Missing metadata.db file".to_string()));
        }

        let conn = rusqlite::Connection::open(&db_path)?;

        // Verify schema version
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .map_err(|_| Error::InvalidFormat("Missing or corrupt schema_version".to_string()))?;

        if version != SCHEMA_VERSION {
            return Err(Error::InvalidFormat(format!(
                "Unsupported schema version: {}",
                version
            )));
        }

        migrate_schema(&conn)?;

        Ok(Box::new(InquestRepository {
            path: repo_path,
            conn,
        }))
    }
}

fn create_schema(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE schema_version (
            version INTEGER NOT NULL
        );

        CREATE TABLE runs (
            id INTEGER PRIMARY KEY,
            timestamp TEXT NOT NULL,
            git_commit TEXT,
            git_dirty INTEGER,
            command TEXT,
            concurrency INTEGER,
            duration_secs REAL,
            exit_code INTEGER,
            total_tests INTEGER NOT NULL DEFAULT 0,
            failures INTEGER NOT NULL DEFAULT 0,
            errors INTEGER NOT NULL DEFAULT 0,
            skips INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE test_results (
            run_id INTEGER NOT NULL REFERENCES runs(id),
            test_id TEXT NOT NULL,
            status TEXT NOT NULL,
            duration_secs REAL,
            message TEXT,
            details TEXT,
            tags TEXT,
            PRIMARY KEY (run_id, test_id)
        );

        CREATE TABLE test_times (
            test_id TEXT PRIMARY KEY,
            duration_secs REAL NOT NULL
        );

        CREATE TABLE failing_tests (
            test_id TEXT PRIMARY KEY,
            run_id INTEGER NOT NULL REFERENCES runs(id),
            status TEXT NOT NULL,
            message TEXT,
            details TEXT
        );
        ",
    )?;

    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?)",
        [SCHEMA_VERSION],
    )?;

    Ok(())
}

/// Try to add a column, ignoring "duplicate column name" errors.
fn add_column_if_missing(conn: &rusqlite::Connection, table: &str, column: &str, col_type: &str) {
    let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, col_type);
    match conn.execute_batch(&sql) {
        Ok(()) => {}
        Err(e) if e.to_string().contains("duplicate column name") => {}
        Err(e) => tracing::warn!("failed to add column {}.{}: {}", table, column, e),
    }
}

/// Add columns that may be missing from older databases.
fn migrate_schema(conn: &rusqlite::Connection) -> Result<()> {
    add_column_if_missing(conn, "runs", "duration_secs", "REAL");
    add_column_if_missing(conn, "runs", "exit_code", "INTEGER");
    add_column_if_missing(conn, "runs", "git_dirty", "INTEGER");
    Ok(())
}

/// Inquest-native repository implementation.
///
/// Stores test runs and metadata in a `.inquest/` directory using SQLite
/// for structured data and subunit files for raw stream data.
pub struct InquestRepository {
    path: PathBuf,
    conn: rusqlite::Connection,
}

impl InquestRepository {
    fn runs_path(&self) -> PathBuf {
        self.path.join("runs")
    }

    fn run_file_path(&self, run_id: &str) -> PathBuf {
        self.runs_path().join(run_id)
    }

    fn run_lock_path(&self, run_id: &str) -> PathBuf {
        self.runs_path().join(format!("{}.lock", run_id))
    }
}

fn status_to_str(status: TestStatus) -> &'static str {
    match status {
        TestStatus::Success => "success",
        TestStatus::Failure => "failure",
        TestStatus::Error => "error",
        TestStatus::Skip => "skip",
        TestStatus::ExpectedFailure => "xfail",
        TestStatus::UnexpectedSuccess => "uxsuccess",
    }
}

fn str_to_status(s: &str) -> TestStatus {
    match s {
        "success" => TestStatus::Success,
        "failure" => TestStatus::Failure,
        "error" => TestStatus::Error,
        "skip" => TestStatus::Skip,
        "xfail" => TestStatus::ExpectedFailure,
        "uxsuccess" => TestStatus::UnexpectedSuccess,
        _ => TestStatus::Error,
    }
}

impl Repository for InquestRepository {
    fn get_test_run(&self, run_id: &str) -> Result<TestRun> {
        let path = self.run_file_path(run_id);
        if !path.exists() {
            return Err(Error::TestRunNotFound(run_id.to_string()));
        }

        let file = File::open(&path)?;
        let metadata = file.metadata()?;
        let test_run = if metadata.len() > MMAP_THRESHOLD_BYTES {
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            subunit_stream::parse_stream_bytes(&mmap, run_id.to_string())
        } else {
            subunit_stream::parse_stream(file, run_id.to_string())
        }?;

        Ok(test_run)
    }

    fn begin_test_run_raw(&mut self) -> Result<(String, Box<dyn std::io::Write + Send>)> {
        let next_id = self.get_next_run_id()?;
        let timestamp = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO runs (id, timestamp) VALUES (?, ?)",
            params![next_id as i64, &timestamp],
        )?;
        let run_id_str = next_id.to_string();

        let lock_path = self.run_lock_path(&run_id_str);
        fs::write(&lock_path, std::process::id().to_string())?;

        let path = self.run_file_path(&run_id_str);
        let file = File::create(&path)?;

        Ok((
            run_id_str,
            Box::new(LockingWriter {
                inner: file,
                lock_path,
            }),
        ))
    }

    fn get_latest_run(&self) -> Result<TestRun> {
        let run_id: i64 = self
            .conn
            .query_row("SELECT id FROM runs ORDER BY id DESC LIMIT 1", [], |row| {
                row.get(0)
            })
            .map_err(|_| Error::NoTestRuns)?;

        self.get_test_run(&run_id.to_string())
    }

    fn get_test_run_raw(&self, run_id: &str) -> Result<Box<dyn std::io::Read>> {
        let path = self.run_file_path(run_id);
        let file = File::open(&path)?;
        Ok(Box::new(file))
    }

    fn replace_failing_tests(&mut self, run: &TestRun) -> Result<()> {
        let run_id: i64 = run
            .id
            .parse()
            .map_err(|e| Error::Other(format!("Invalid run ID: {}", e)))?;

        let tx = self.conn.transaction()?;

        // Clear all existing failures
        tx.execute("DELETE FROM failing_tests", [])?;

        // Insert new failures
        {
            let mut stmt = tx.prepare(
                "INSERT INTO failing_tests (test_id, run_id, status, message, details) VALUES (?, ?, ?, ?, ?)",
            )?;
            for result in run.results.values() {
                if result.status.is_failure() {
                    stmt.execute(params![
                        result.test_id.as_str(),
                        run_id,
                        status_to_str(result.status),
                        result.message,
                        result.details,
                    ])?;
                }
            }
        }

        // Update run summary
        update_run_summary(&tx, run_id, run)?;

        // Update test_results table
        insert_test_results(&tx, run_id, run)?;

        tx.commit()?;
        Ok(())
    }

    fn update_failing_tests(&mut self, run: &TestRun) -> Result<()> {
        let run_id: i64 = run
            .id
            .parse()
            .map_err(|e| Error::Other(format!("Invalid run ID: {}", e)))?;

        let tx = self.conn.transaction()?;

        for result in run.results.values() {
            if result.status.is_failure() {
                tx.execute(
                    "INSERT OR REPLACE INTO failing_tests (test_id, run_id, status, message, details) VALUES (?, ?, ?, ?, ?)",
                    params![
                        result.test_id.as_str(),
                        run_id,
                        status_to_str(result.status),
                        result.message,
                        result.details,
                    ],
                )?;
            } else if result.status.is_success() {
                tx.execute(
                    "DELETE FROM failing_tests WHERE test_id = ?",
                    [result.test_id.as_str()],
                )?;
            }
        }

        // Update run summary
        update_run_summary(&tx, run_id, run)?;

        // Update test_results table
        insert_test_results(&tx, run_id, run)?;

        tx.commit()?;
        Ok(())
    }

    fn get_failing_tests(&self) -> Result<Vec<TestId>> {
        let mut stmt = self.conn.prepare("SELECT test_id FROM failing_tests")?;
        let ids = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                Ok(TestId::new(id))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn get_failing_tests_raw(&self) -> Result<Box<dyn std::io::Read>> {
        // Synthesize a subunit stream from the failing_tests table
        let mut stmt = self
            .conn
            .prepare("SELECT test_id, status, message, details FROM failing_tests")?;

        let mut test_run = TestRun::new("failing".to_string());
        let rows = stmt.query_map([], |row| {
            let test_id: String = row.get(0)?;
            let status_str: String = row.get(1)?;
            let message: Option<String> = row.get(2)?;
            let details: Option<String> = row.get(3)?;
            Ok((test_id, status_str, message, details))
        })?;

        for row in rows {
            let (test_id, status_str, message, details) = row?;
            let status = str_to_status(&status_str);
            let mut result = TestResult {
                test_id: TestId::new(test_id),
                status,
                duration: None,
                message,
                details,
                tags: vec![],
            };
            // Ensure details are preserved
            if result.details.is_none() {
                result.details = result.message.clone();
            }
            test_run.add_result(result);
        }

        let mut buf = Vec::new();
        subunit_stream::write_stream(&test_run, &mut buf)?;
        Ok(Box::new(std::io::Cursor::new(buf)))
    }

    fn get_test_times(&self) -> Result<HashMap<TestId, Duration>> {
        let mut stmt = self
            .conn
            .prepare("SELECT test_id, duration_secs FROM test_times")?;
        let mut result = HashMap::new();
        let rows = stmt.query_map([], |row| {
            let test_id: String = row.get(0)?;
            let duration: f64 = row.get(1)?;
            Ok((test_id, duration))
        })?;

        for row in rows {
            let (test_id, duration) = row?;
            result.insert(TestId::new(test_id), Duration::from_secs_f64(duration));
        }
        Ok(result)
    }

    fn get_test_times_for_ids(&self, test_ids: &[TestId]) -> Result<HashMap<TestId, Duration>> {
        if test_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut result = HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT duration_secs FROM test_times WHERE test_id = ?")?;

        for test_id in test_ids {
            if let Ok(duration) = stmt.query_row([test_id.as_str()], |row| row.get::<_, f64>(0)) {
                result.insert(test_id.clone(), Duration::from_secs_f64(duration));
            }
        }

        Ok(result)
    }

    fn update_test_times(&mut self, times: &HashMap<TestId, Duration>) -> Result<()> {
        if times.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO test_times (test_id, duration_secs) VALUES (?, ?)",
            )?;
            for (test_id, duration) in times {
                stmt.execute(params![test_id.as_str(), duration.as_secs_f64()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn get_next_run_id(&self) -> Result<u64> {
        let id: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(id) + 1, 0) FROM runs", [], |row| {
                    row.get(0)
                })?;
        Ok(id as u64)
    }

    fn list_run_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT id FROM runs ORDER BY id ASC")?;
        let ids = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                Ok(id.to_string())
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    fn get_run_metadata(&self, run_id: &str) -> Result<RunMetadata> {
        let result = self.conn.query_row(
            "SELECT git_commit, git_dirty, command, concurrency, duration_secs, exit_code FROM runs WHERE id = ?",
            [run_id],
            |row| {
                Ok(RunMetadata {
                    git_commit: row.get(0)?,
                    git_dirty: row.get(1)?,
                    command: row.get(2)?,
                    concurrency: row.get::<_, Option<i64>>(3)?.map(|v| v as u32),
                    duration_secs: row.get(4)?,
                    exit_code: row.get(5)?,
                })
            },
        );
        match result {
            Ok(metadata) => Ok(metadata),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(RunMetadata::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn is_run_in_progress(&self, run_id: &str) -> Result<bool> {
        let lock_path = self.run_lock_path(run_id);
        let contents = match fs::read_to_string(&lock_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let pid: u32 = match contents.trim().parse() {
            Ok(p) => p,
            Err(_) => {
                let _ = fs::remove_file(&lock_path);
                return Ok(false);
            }
        };
        let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
        if !alive {
            let _ = fs::remove_file(&lock_path);
        }
        Ok(alive)
    }

    fn get_running_run_ids(&self) -> Result<Vec<String>> {
        let runs_dir = self.runs_path();
        let mut result = Vec::new();
        let entries = match fs::read_dir(&runs_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(result),
            Err(e) => return Err(e.into()),
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "lock") {
                let run_id = path.file_stem().unwrap().to_string_lossy().to_string();
                if self.is_run_in_progress(&run_id)? {
                    result.push(run_id);
                }
            }
        }
        result.sort();
        Ok(result)
    }

    fn set_run_metadata(&mut self, run_id: &str, metadata: RunMetadata) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET git_commit = ?, git_dirty = ?, command = ?, concurrency = ?, duration_secs = ?, exit_code = ? WHERE id = ?",
            params![
                metadata.git_commit,
                metadata.git_dirty,
                metadata.command,
                metadata.concurrency,
                metadata.duration_secs,
                metadata.exit_code,
                run_id,
            ],
        )?;
        Ok(())
    }
}

fn update_run_summary(conn: &rusqlite::Connection, run_id: i64, run: &TestRun) -> Result<()> {
    let total = run.total_tests() as i64;
    let failures = run
        .results
        .values()
        .filter(|r| matches!(r.status, TestStatus::Failure))
        .count() as i64;
    let errors = run
        .results
        .values()
        .filter(|r| matches!(r.status, TestStatus::Error))
        .count() as i64;
    let skips = run
        .results
        .values()
        .filter(|r| matches!(r.status, TestStatus::Skip))
        .count() as i64;

    conn.execute(
        "UPDATE runs SET total_tests = ?, failures = ?, errors = ?, skips = ? WHERE id = ?",
        params![total, failures, errors, skips, run_id],
    )?;

    Ok(())
}

fn insert_test_results(conn: &rusqlite::Connection, run_id: i64, run: &TestRun) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR REPLACE INTO test_results (run_id, test_id, status, duration_secs, message, details, tags) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )?;

    for result in run.results.values() {
        let tags = if result.tags.is_empty() {
            None
        } else {
            Some(result.tags.join(","))
        };

        stmt.execute(params![
            run_id,
            result.test_id.as_str(),
            status_to_str(result.status),
            result.duration.map(|d| d.as_secs_f64()),
            result.message,
            result.details,
            tags,
        ])?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_initialize_repository() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;

        let repo = factory.initialise(temp.path()).unwrap();
        assert_eq!(repo.get_next_run_id().unwrap(), 0);

        // Verify directory structure
        let repo_path = temp.path().join(REPO_DIR);
        assert!(repo_path.exists());
        assert!(repo_path.join("format").exists());
        assert!(repo_path.join("runs").exists());
        assert!(repo_path.join("metadata.db").exists());

        // Verify format content
        let format = fs::read_to_string(repo_path.join("format")).unwrap();
        assert_eq!(format.trim(), "1");
    }

    #[test]
    fn test_open_nonexistent_repository() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;

        let result = factory.open(temp.path());
        assert!(matches!(result, Err(Error::RepositoryNotFound(_))));
    }

    #[test]
    fn test_cannot_double_initialize() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;

        factory.initialise(temp.path()).unwrap();
        let result = factory.initialise(temp.path());
        assert!(matches!(result, Err(Error::RepositoryExists(_))));
    }

    #[test]
    fn test_open_existing_repository() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;

        factory.initialise(temp.path()).unwrap();
        let repo = factory.open(temp.path()).unwrap();
        assert_eq!(repo.get_next_run_id().unwrap(), 0);
    }

    #[test]
    fn test_insert_test_run() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;

        let mut repo = factory.initialise(temp.path()).unwrap();

        let run = TestRun::new("0".to_string());
        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();

        subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

        assert_eq!(run_id, "0");
        assert_eq!(repo.get_next_run_id().unwrap(), 1);

        // Verify file was created
        let run_path = temp.path().join(REPO_DIR).join("runs").join("0");
        assert!(run_path.exists());
    }

    #[test]
    fn test_list_run_ids() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;

        let mut repo = factory.initialise(temp.path()).unwrap();
        assert_eq!(repo.list_run_ids().unwrap().len(), 0);

        // Insert two runs
        let run = TestRun::new("0".to_string());
        let (_, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

        let run = TestRun::new("1".to_string());
        let (_, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

        let ids = repo.list_run_ids().unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids, vec!["0", "1"]);
    }

    #[test]
    fn test_count() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;

        let mut repo = factory.initialise(temp.path()).unwrap();
        assert_eq!(repo.count().unwrap(), 0);

        let run = TestRun::new("0".to_string());
        let (_, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn test_get_latest_run_empty_repository() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;

        let repo = factory.initialise(temp.path()).unwrap();
        let result = repo.get_latest_run();
        assert!(matches!(result, Err(Error::NoTestRuns)));
    }

    #[test]
    fn test_failing_tests_replace() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // First run: test1 fails, test2 passes
        let mut run1 = TestRun::new("0".to_string());
        run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run1.add_result(TestResult::failure("test1", "Failed"));
        run1.add_result(TestResult::success("test2"));

        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run1, &mut writer).unwrap();
        drop(writer);
        run1.id = run_id;
        repo.replace_failing_tests(&run1).unwrap();

        let failing = repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 1);
        assert!(failing.iter().any(|id| id.as_str() == "test1"));

        // Second full run: only test3 fails
        let mut run2 = TestRun::new("1".to_string());
        run2.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
        run2.add_result(TestResult::success("test1"));
        run2.add_result(TestResult::success("test2"));
        run2.add_result(TestResult::failure("test3", "Failed"));

        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run2, &mut writer).unwrap();
        drop(writer);
        run2.id = run_id;
        repo.replace_failing_tests(&run2).unwrap();

        let failing = repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 1);
        assert!(failing.iter().any(|id| id.as_str() == "test3"));
        assert!(!failing.iter().any(|id| id.as_str() == "test1"));
    }

    #[test]
    fn test_failing_tests_partial_update() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // First run: test1 fails
        let mut run1 = TestRun::new("0".to_string());
        run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run1.add_result(TestResult::failure("test1", "Failed"));
        run1.add_result(TestResult::success("test2"));

        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run1, &mut writer).unwrap();
        drop(writer);
        run1.id = run_id;
        repo.replace_failing_tests(&run1).unwrap();

        // Second partial run: test1 passes, test3 fails
        let mut run2 = TestRun::new("1".to_string());
        run2.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
        run2.add_result(TestResult::success("test1"));
        run2.add_result(TestResult::failure("test3", "Failed"));

        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run2, &mut writer).unwrap();
        drop(writer);
        run2.id = run_id;
        repo.update_failing_tests(&run2).unwrap();

        let failing = repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 1);
        assert!(!failing.iter().any(|id| id.as_str() == "test1"));
        assert!(failing.iter().any(|id| id.as_str() == "test3"));
    }

    #[test]
    fn test_partial_mode_keeps_untested_failures() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // First run: test1, test2, test3 all fail
        let mut run1 = TestRun::new("0".to_string());
        run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run1.add_result(TestResult::failure("test1", "Failed"));
        run1.add_result(TestResult::failure("test2", "Failed"));
        run1.add_result(TestResult::failure("test3", "Failed"));

        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run1, &mut writer).unwrap();
        drop(writer);
        run1.id = run_id;
        repo.replace_failing_tests(&run1).unwrap();

        assert_eq!(repo.get_failing_tests().unwrap().len(), 3);

        // Second partial run: only test1 passes, test2/test3 not re-tested
        let mut run2 = TestRun::new("1".to_string());
        run2.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
        run2.add_result(TestResult::success("test1"));

        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run2, &mut writer).unwrap();
        drop(writer);
        run2.id = run_id;
        repo.update_failing_tests(&run2).unwrap();

        let failing = repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 2);
        assert!(!failing.contains(&TestId::new("test1")));
        assert!(failing.contains(&TestId::new("test2")));
        assert!(failing.contains(&TestId::new("test3")));
    }

    #[test]
    fn test_test_times() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut times = HashMap::new();
        times.insert(TestId::new("test1"), Duration::from_secs_f64(1.5));
        times.insert(TestId::new("test2"), Duration::from_secs_f64(0.5));
        times.insert(TestId::new("test3"), Duration::from_secs_f64(2.25));

        repo.update_test_times(&times).unwrap();

        // Read all times
        let all_times = repo.get_test_times().unwrap();
        assert_eq!(all_times.len(), 3);
        assert_eq!(
            all_times.get(&TestId::new("test1")).unwrap().as_secs_f64(),
            1.5
        );

        // Read specific times
        let test_ids = vec![TestId::new("test1"), TestId::new("test3")];
        let specific_times = repo.get_test_times_for_ids(&test_ids).unwrap();
        assert_eq!(specific_times.len(), 2);
        assert_eq!(
            specific_times
                .get(&TestId::new("test3"))
                .unwrap()
                .as_secs_f64(),
            2.25
        );
    }

    #[test]
    fn test_times_updates_on_multiple_runs() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut times1 = HashMap::new();
        times1.insert(TestId::new("test1"), Duration::from_secs_f64(1.0));
        repo.update_test_times(&times1).unwrap();

        let mut times2 = HashMap::new();
        times2.insert(TestId::new("test1"), Duration::from_secs_f64(2.0));
        repo.update_test_times(&times2).unwrap();

        let test_ids = vec![TestId::new("test1")];
        let times = repo.get_test_times_for_ids(&test_ids).unwrap();
        assert_eq!(times.get(&TestId::new("test1")).unwrap().as_secs_f64(), 2.0);
    }

    #[test]
    fn test_run_metadata() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        // Create a run
        let run = TestRun::new("0".to_string());
        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

        // Set metadata
        repo.set_run_metadata(
            &run_id,
            RunMetadata {
                git_commit: Some("abc123".to_string()),
                git_dirty: Some(true),
                command: Some("python -m pytest".to_string()),
                concurrency: Some(4),
                duration_secs: Some(12.5),
                exit_code: Some(1),
            },
        )
        .unwrap();

        // Verify metadata was stored (open as new connection to verify)
        let db_path = temp.path().join(REPO_DIR).join("metadata.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (git_commit, git_dirty, command, concurrency, duration_secs, exit_code): (
            Option<String>,
            Option<bool>,
            Option<String>,
            Option<i64>,
            Option<f64>,
            Option<i32>,
        ) = conn
            .query_row(
                "SELECT git_commit, git_dirty, command, concurrency, duration_secs, exit_code FROM runs WHERE id = ?",
                [&run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .unwrap();

        assert_eq!(git_commit, Some("abc123".to_string()));
        assert_eq!(git_dirty, Some(true));
        assert_eq!(command, Some("python -m pytest".to_string()));
        assert_eq!(concurrency, Some(4));
        assert_eq!(duration_secs, Some(12.5));
        assert_eq!(exit_code, Some(1));
    }

    #[test]
    fn test_lock_file_created_and_removed() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let (run_id, writer) = repo.begin_test_run_raw().unwrap();
        let lock_path = temp
            .path()
            .join(REPO_DIR)
            .join("runs")
            .join(format!("{}.lock", run_id));

        assert!(lock_path.exists());
        let contents = fs::read_to_string(&lock_path).unwrap();
        assert_eq!(contents, std::process::id().to_string());

        // While writer is alive, run is in progress
        assert!(repo.is_run_in_progress(&run_id).unwrap());

        drop(writer);
        assert!(!lock_path.exists());
        assert!(!repo.is_run_in_progress(&run_id).unwrap());
    }

    #[test]
    fn test_stale_lock_file_cleaned_up() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let run = TestRun::new("0".to_string());
        let run_id = repo.insert_test_run(run).unwrap();

        // Create a stale lock file with a PID that doesn't exist
        let lock_path = temp
            .path()
            .join(REPO_DIR)
            .join("runs")
            .join(format!("{}.lock", run_id));
        fs::write(&lock_path, "999999999").unwrap();

        assert!(!repo.is_run_in_progress(&run_id).unwrap());
        assert!(!lock_path.exists());
    }

    #[test]
    fn test_migrate_adds_missing_columns() {
        let temp = TempDir::new().unwrap();
        let repo_dir = temp.path().join(REPO_DIR);
        fs::create_dir_all(repo_dir.join("runs")).unwrap();
        fs::write(repo_dir.join("format"), FORMAT_VERSION).unwrap();

        // Create a database with the old schema (no duration_secs or exit_code)
        let db_path = repo_dir.join("metadata.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE schema_version (version INTEGER NOT NULL);
            INSERT INTO schema_version (version) VALUES (1);
            CREATE TABLE runs (
                id INTEGER PRIMARY KEY,
                timestamp TEXT NOT NULL,
                git_commit TEXT,
                command TEXT,
                concurrency INTEGER,
                total_tests INTEGER NOT NULL DEFAULT 0,
                failures INTEGER NOT NULL DEFAULT 0,
                errors INTEGER NOT NULL DEFAULT 0,
                skips INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE test_results (
                run_id INTEGER NOT NULL REFERENCES runs(id),
                test_id TEXT NOT NULL,
                status TEXT NOT NULL,
                duration_secs REAL,
                message TEXT,
                details TEXT,
                tags TEXT,
                PRIMARY KEY (run_id, test_id)
            );
            CREATE TABLE test_times (
                test_id TEXT PRIMARY KEY,
                duration_secs REAL NOT NULL
            );
            CREATE TABLE failing_tests (
                test_id TEXT PRIMARY KEY,
                run_id INTEGER NOT NULL REFERENCES runs(id),
                status TEXT NOT NULL,
                message TEXT,
                details TEXT
            );
            ",
        )
        .unwrap();
        drop(conn);

        // Open via the factory — migration should add the missing columns
        let factory = InquestRepositoryFactory;
        let mut repo = factory.open(temp.path()).unwrap();

        // Create a run and set metadata with the new fields
        let run = TestRun::new("0".to_string());
        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

        repo.set_run_metadata(
            &run_id,
            RunMetadata {
                git_commit: Some("def456".to_string()),
                git_dirty: Some(false),
                command: Some("cargo test".to_string()),
                concurrency: Some(2),
                duration_secs: Some(45.3),
                exit_code: Some(0),
            },
        )
        .unwrap();

        // Verify the new columns are populated
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (duration_secs, exit_code, git_dirty): (Option<f64>, Option<i32>, Option<bool>) = conn
            .query_row(
                "SELECT duration_secs, exit_code, git_dirty FROM runs WHERE id = ?",
                [&run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert_eq!(duration_secs, Some(45.3));
        assert_eq!(exit_code, Some(0));
        assert_eq!(git_dirty, Some(false));
    }

    #[test]
    fn test_test_results_stored_in_db() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new("0".to_string());
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::success("test1").with_duration(Duration::from_secs(1)));
        run.add_result(TestResult::failure("test2", "assertion failed"));
        run.add_result(TestResult::skip("test3"));

        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);
        run.id = run_id;
        repo.replace_failing_tests(&run).unwrap();

        // Verify test_results were stored
        let db_path = temp.path().join(REPO_DIR).join("metadata.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM test_results WHERE run_id = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);

        // Verify run summary was updated
        let (total, failures, skips): (i64, i64, i64) = conn
            .query_row(
                "SELECT total_tests, failures, skips FROM runs WHERE id = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(total, 3);
        assert_eq!(failures, 1);
        assert_eq!(skips, 1);
    }

    #[test]
    fn test_get_failing_tests_raw() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new("0".to_string());
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::success("test1"));
        run.add_result(TestResult::failure("test2", "Failed"));
        run.add_result(TestResult::failure("test3", "Also failed"));

        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);
        run.id = run_id;
        repo.replace_failing_tests(&run).unwrap();

        // Get raw failing stream and parse it
        let reader = repo.get_failing_tests_raw().unwrap();
        let parsed = subunit_stream::parse_stream(reader, "failing".to_string()).unwrap();

        assert_eq!(parsed.results.len(), 2);
        assert!(parsed.results.contains_key(&TestId::new("test2")));
        assert!(parsed.results.contains_key(&TestId::new("test3")));
    }
}
