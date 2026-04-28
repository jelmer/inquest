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
    Repository, RepositoryFactory, RunId, RunMetadata, TestId, TestResult, TestRun, TestStatus,
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

/// Check whether a process with the given PID is still running.
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Check whether a process with the given PID is still running.
#[cfg(not(unix))]
fn is_process_alive(pid: u32) -> bool {
    // On non-Unix platforms, try tasklist to check if the PID exists.
    // Falls back to assuming alive if tasklist is unavailable.
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH"])
        .output()
        .map(|o| {
            let output = String::from_utf8_lossy(&o.stdout);
            output.contains(&pid.to_string())
        })
        .unwrap_or(true)
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
            skips INTEGER NOT NULL DEFAULT 0,
            test_args TEXT
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

        CREATE INDEX test_results_test_id_run_id ON test_results (test_id, run_id);

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

        CREATE TABLE test_flakiness (
            test_id TEXT PRIMARY KEY,
            runs INTEGER NOT NULL,
            failures INTEGER NOT NULL,
            transitions INTEGER NOT NULL,
            last_run_id INTEGER NOT NULL,
            last_failed INTEGER NOT NULL
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
    add_column_if_missing(conn, "runs", "test_args", "TEXT");
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS test_results_test_id_run_id \
         ON test_results (test_id, run_id);
         CREATE TABLE IF NOT EXISTS test_flakiness (
             test_id TEXT PRIMARY KEY,
             runs INTEGER NOT NULL,
             failures INTEGER NOT NULL,
             transitions INTEGER NOT NULL,
             last_run_id INTEGER NOT NULL,
             last_failed INTEGER NOT NULL
         );",
    )?;
    backfill_flakiness_if_empty(conn)?;
    Ok(())
}

/// Populate `test_flakiness` from `test_results` if the cache is empty but
/// results exist. Runs once after migration on a pre-cache database.
fn backfill_flakiness_if_empty(conn: &rusqlite::Connection) -> Result<()> {
    let cache_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM test_flakiness", [], |r| r.get(0))?;
    if cache_count > 0 {
        return Ok(());
    }
    let results_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM test_results", [], |r| r.get(0))?;
    if results_count == 0 {
        return Ok(());
    }
    rebuild_flakiness_cache(conn)
}

/// Recompute the entire `test_flakiness` cache from `test_results`. Used for
/// initial backfill and as a defensive fallback if the cache is suspected
/// stale. The query is a single sequential scan, ordered by the
/// `(test_id, run_id)` index so transitions can be detected with a window.
fn rebuild_flakiness_cache(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute("DELETE FROM test_flakiness", [])?;
    let mut stmt = conn.prepare(
        "SELECT test_id, run_id, status FROM test_results \
         ORDER BY test_id ASC, run_id ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        let test_id: String = row.get(0)?;
        let run_id: i64 = row.get(1)?;
        let status: String = row.get(2)?;
        Ok((test_id, run_id, status))
    })?;
    let mut current: Option<(String, FlakinessAccum)> = None;
    let mut insert = conn.prepare(
        "INSERT INTO test_flakiness \
         (test_id, runs, failures, transitions, last_run_id, last_failed) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )?;
    for row in rows {
        let (test_id, run_id, status_str) = row?;
        let failed = str_to_status(&status_str).is_failure();
        match &mut current {
            Some((cur_id, accum)) if *cur_id == test_id => {
                accum.add(run_id, failed);
            }
            _ => {
                if let Some((cur_id, accum)) = current.take() {
                    insert.execute(params![
                        cur_id,
                        accum.runs,
                        accum.failures,
                        accum.transitions,
                        accum.last_run_id,
                        accum.last_failed as i64,
                    ])?;
                }
                current = Some((test_id, FlakinessAccum::new(run_id, failed)));
            }
        }
    }
    if let Some((cur_id, accum)) = current {
        insert.execute(params![
            cur_id,
            accum.runs,
            accum.failures,
            accum.transitions,
            accum.last_run_id,
            accum.last_failed as i64,
        ])?;
    }
    Ok(())
}

/// Per-test running aggregate used while scanning `test_results` ordered by
/// `(test_id, run_id)`. `last_failed` lets us detect transitions in O(1).
struct FlakinessAccum {
    runs: u32,
    failures: u32,
    transitions: u32,
    last_run_id: i64,
    last_failed: bool,
}

impl FlakinessAccum {
    fn new(run_id: i64, failed: bool) -> Self {
        Self {
            runs: 1,
            failures: if failed { 1 } else { 0 },
            transitions: 0,
            last_run_id: run_id,
            last_failed: failed,
        }
    }

    fn add(&mut self, run_id: i64, failed: bool) {
        self.runs += 1;
        if failed {
            self.failures += 1;
        }
        if failed != self.last_failed {
            self.transitions += 1;
        }
        self.last_run_id = run_id;
        self.last_failed = failed;
    }
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

    fn run_file_path(&self, run_id: &RunId) -> PathBuf {
        self.runs_path().join(run_id.as_str())
    }

    fn run_lock_path(&self, run_id: &RunId) -> PathBuf {
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
    fn get_test_run(&self, run_id: &RunId) -> Result<TestRun> {
        let path = self.run_file_path(run_id);
        if !path.exists() {
            return Err(Error::TestRunNotFound(run_id.to_string()));
        }

        let file = File::open(&path)?;
        let metadata = file.metadata()?;
        let test_run = if metadata.len() > MMAP_THRESHOLD_BYTES {
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            subunit_stream::parse_stream_bytes(&mmap, run_id.clone())
        } else {
            subunit_stream::parse_stream(file, run_id.clone())
        }?;

        Ok(test_run)
    }

    fn begin_test_run_raw(&mut self) -> Result<(RunId, Box<dyn std::io::Write + Send>)> {
        let next_id = self.get_next_run_id()?;
        let timestamp = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO runs (id, timestamp) VALUES (?, ?)",
            params![
                next_id
                    .as_str()
                    .parse::<i64>()
                    .map_err(|e| Error::Other(format!("Invalid run ID: {}", e)))?,
                &timestamp
            ],
        )?;

        let lock_path = self.run_lock_path(&next_id);
        fs::write(&lock_path, std::process::id().to_string())?;

        let path = self.run_file_path(&next_id);
        let file = File::create(&path)?;

        Ok((
            next_id,
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

        self.get_test_run(&RunId::new(run_id.to_string()))
    }

    fn get_test_run_raw(&self, run_id: &RunId) -> Result<Box<dyn std::io::Read>> {
        let path = self.run_file_path(run_id);
        let file = File::open(&path)?;
        Ok(Box::new(file))
    }

    fn replace_failing_tests(&mut self, run: &TestRun) -> Result<()> {
        let run_id: i64 = run
            .id
            .as_str()
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
            .as_str()
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

        let mut test_run = TestRun::new(RunId::new("failing"));
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

    fn get_next_run_id(&self) -> Result<RunId> {
        let id: i64 =
            self.conn
                .query_row("SELECT COALESCE(MAX(id) + 1, 0) FROM runs", [], |row| {
                    row.get(0)
                })?;
        Ok(RunId::new(id.to_string()))
    }

    fn list_run_ids(&self) -> Result<Vec<RunId>> {
        let mut stmt = self.conn.prepare("SELECT id FROM runs ORDER BY id ASC")?;
        let ids = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                Ok(RunId::new(id.to_string()))
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

    fn get_run_metadata(&self, run_id: &RunId) -> Result<RunMetadata> {
        let result = self.conn.query_row(
            "SELECT git_commit, git_dirty, command, concurrency, duration_secs, exit_code, test_args FROM runs WHERE id = ?",
            [run_id.as_str()],
            |row| {
                let test_args_json: Option<String> = row.get(6)?;
                let test_args = test_args_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok());
                Ok(RunMetadata {
                    git_commit: row.get(0)?,
                    git_dirty: row.get(1)?,
                    command: row.get(2)?,
                    concurrency: row.get::<_, Option<i64>>(3)?.map(|v| v as u32),
                    duration_secs: row.get(4)?,
                    exit_code: row.get(5)?,
                    test_args,
                })
            },
        );
        match result {
            Ok(metadata) => Ok(metadata),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(RunMetadata::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn is_run_in_progress(&self, run_id: &RunId) -> Result<bool> {
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
        let alive = is_process_alive(pid);
        if !alive {
            let _ = fs::remove_file(&lock_path);
        }
        Ok(alive)
    }

    fn get_running_run_ids(&self) -> Result<Vec<RunId>> {
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
                let run_id = RunId::new(path.file_stem().unwrap().to_string_lossy().to_string());
                if self.is_run_in_progress(&run_id)? {
                    result.push(run_id);
                }
            }
        }
        result.sort();
        Ok(result)
    }

    fn set_run_metadata(&mut self, run_id: &RunId, metadata: RunMetadata) -> Result<()> {
        let test_args_json = metadata
            .test_args
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| Error::Other(format!("Failed to serialize test_args: {}", e)))?;
        self.conn.execute(
            "UPDATE runs SET git_commit = ?, git_dirty = ?, command = ?, concurrency = ?, duration_secs = ?, exit_code = ?, test_args = ? WHERE id = ?",
            params![
                metadata.git_commit,
                metadata.git_dirty,
                metadata.command,
                metadata.concurrency,
                metadata.duration_secs,
                metadata.exit_code,
                test_args_json,
                run_id.as_str(),
            ],
        )?;
        Ok(())
    }

    fn get_flakiness(&self, min_runs: usize) -> Result<Vec<crate::repository::TestFlakiness>> {
        // Read directly from the materialized cache. Maintained incrementally
        // by `insert_test_results`, so this is O(distinct_tests) instead of
        // O(runs × tests).
        let mut stmt = self.conn.prepare(
            "SELECT test_id, runs, failures, transitions FROM test_flakiness \
             WHERE runs >= ? AND failures > 0",
        )?;
        let rows = stmt.query_map(params![min_runs as i64], |row| {
            let test_id: String = row.get(0)?;
            let runs: u32 = row.get(1)?;
            let failures: u32 = row.get(2)?;
            let transitions: u32 = row.get(3)?;
            Ok((test_id, runs, failures, transitions))
        })?;
        let mut out: Vec<crate::repository::TestFlakiness> = rows
            .map(|r| {
                r.map(|(test_id, runs, failures, transitions)| {
                    let denom = runs.saturating_sub(1).max(1) as f64;
                    let flakiness_score = transitions as f64 / denom;
                    let failure_rate = if runs == 0 {
                        0.0
                    } else {
                        failures as f64 / runs as f64
                    };
                    crate::repository::TestFlakiness {
                        test_id: TestId::new(test_id),
                        runs,
                        failures,
                        transitions,
                        flakiness_score,
                        failure_rate,
                    }
                })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        out.sort_by(|a, b| {
            b.transitions
                .cmp(&a.transitions)
                .then_with(|| {
                    b.failure_rate
                        .partial_cmp(&a.failure_rate)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.test_id.as_str().cmp(b.test_id.as_str()))
        });
        Ok(out)
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
    let mut existing_stmt =
        conn.prepare("SELECT status FROM test_results WHERE run_id = ? AND test_id = ?")?;

    // Track tests where a prior `(run_id, test_id)` row was overwritten — for
    // these we can't incrementally update the cache, since the change could
    // have happened anywhere in the test's history. Recompute them at the end.
    let mut needs_rebuild: Vec<TestId> = Vec::new();

    for result in run.results.values() {
        let tags = if result.tags.is_empty() {
            None
        } else {
            Some(result.tags.join(","))
        };

        let prior_status: Option<String> = existing_stmt
            .query_row(params![run_id, result.test_id.as_str()], |row| row.get(0))
            .ok();

        stmt.execute(params![
            run_id,
            result.test_id.as_str(),
            status_to_str(result.status),
            result.duration.map(|d| d.as_secs_f64()),
            result.message,
            result.details,
            tags,
        ])?;

        if prior_status.is_some() {
            needs_rebuild.push(result.test_id.clone());
        } else {
            update_flakiness_cache(
                conn,
                result.test_id.as_str(),
                run_id,
                result.status.is_failure(),
            )?;
        }
    }

    drop(stmt);
    drop(existing_stmt);
    for test_id in needs_rebuild {
        rebuild_flakiness_for_test(conn, test_id.as_str())?;
    }

    Ok(())
}

/// Apply a single new `(run_id, test_id)` result to the flakiness cache,
/// without re-scanning history. Assumes no prior row existed at this run_id —
/// callers must check that and fall back to [`rebuild_flakiness_for_test`]
/// otherwise.
fn update_flakiness_cache(
    conn: &rusqlite::Connection,
    test_id: &str,
    run_id: i64,
    failed: bool,
) -> Result<()> {
    let existing: Option<(u32, u32, u32, i64, i64)> = conn
        .query_row(
            "SELECT runs, failures, transitions, last_run_id, last_failed \
             FROM test_flakiness WHERE test_id = ?",
            params![test_id],
            |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .ok();

    match existing {
        None => {
            conn.execute(
                "INSERT INTO test_flakiness \
                 (test_id, runs, failures, transitions, last_run_id, last_failed) \
                 VALUES (?, 1, ?, 0, ?, ?)",
                params![test_id, failed as i64, run_id, failed as i64],
            )?;
        }
        Some((runs, failures, transitions, last_run_id, last_failed)) => {
            if run_id > last_run_id {
                let prev_failed = last_failed != 0;
                let new_transitions = transitions + if failed != prev_failed { 1 } else { 0 };
                let new_failures = failures + if failed { 1 } else { 0 };
                conn.execute(
                    "UPDATE test_flakiness \
                     SET runs = ?, failures = ?, transitions = ?, \
                         last_run_id = ?, last_failed = ? \
                     WHERE test_id = ?",
                    params![
                        runs + 1,
                        new_failures,
                        new_transitions,
                        run_id,
                        failed as i64,
                        test_id,
                    ],
                )?;
            } else {
                // Out-of-order insert: cheaper to just recompute this one test
                // than to rederive transitions piecewise.
                rebuild_flakiness_for_test(conn, test_id)?;
            }
        }
    }
    Ok(())
}

/// Recompute the cache row for a single test by scanning its rows in
/// `test_results`. Used when an in-place update or out-of-order insert makes
/// incremental maintenance unsound.
fn rebuild_flakiness_for_test(conn: &rusqlite::Connection, test_id: &str) -> Result<()> {
    let mut stmt = conn
        .prepare("SELECT run_id, status FROM test_results WHERE test_id = ? ORDER BY run_id ASC")?;
    let rows = stmt.query_map(params![test_id], |row| {
        let run_id: i64 = row.get(0)?;
        let status: String = row.get(1)?;
        Ok((run_id, status))
    })?;
    let mut accum: Option<FlakinessAccum> = None;
    for row in rows {
        let (run_id, status_str) = row?;
        let failed = str_to_status(&status_str).is_failure();
        match &mut accum {
            Some(a) => a.add(run_id, failed),
            None => accum = Some(FlakinessAccum::new(run_id, failed)),
        }
    }
    drop(stmt);
    match accum {
        None => {
            conn.execute(
                "DELETE FROM test_flakiness WHERE test_id = ?",
                params![test_id],
            )?;
        }
        Some(a) => {
            conn.execute(
                "INSERT OR REPLACE INTO test_flakiness \
                 (test_id, runs, failures, transitions, last_run_id, last_failed) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    test_id,
                    a.runs,
                    a.failures,
                    a.transitions,
                    a.last_run_id,
                    a.last_failed as i64,
                ],
            )?;
        }
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
        assert_eq!(repo.get_next_run_id().unwrap(), RunId::new("0"));

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
        assert_eq!(repo.get_next_run_id().unwrap(), RunId::new("0"));
    }

    #[test]
    fn test_insert_test_run() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;

        let mut repo = factory.initialise(temp.path()).unwrap();

        let run = TestRun::new(RunId::new("0"));
        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();

        subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

        assert_eq!(run_id, RunId::new("0"));
        assert_eq!(repo.get_next_run_id().unwrap(), RunId::new("1"));

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
        let run = TestRun::new(RunId::new("0"));
        let (_, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

        let run = TestRun::new(RunId::new("1"));
        let (_, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run, &mut writer).unwrap();
        drop(writer);

        let ids = repo.list_run_ids().unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids, vec![RunId::new("0"), RunId::new("1")]);
    }

    #[test]
    fn test_count() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;

        let mut repo = factory.initialise(temp.path()).unwrap();
        assert_eq!(repo.count().unwrap(), 0);

        let run = TestRun::new(RunId::new("0"));
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
        let mut run1 = TestRun::new(RunId::new("0"));
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
        let mut run2 = TestRun::new(RunId::new("1"));
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
        let mut run1 = TestRun::new(RunId::new("0"));
        run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run1.add_result(TestResult::failure("test1", "Failed"));
        run1.add_result(TestResult::success("test2"));

        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run1, &mut writer).unwrap();
        drop(writer);
        run1.id = run_id;
        repo.replace_failing_tests(&run1).unwrap();

        // Second partial run: test1 passes, test3 fails
        let mut run2 = TestRun::new(RunId::new("1"));
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
        let mut run1 = TestRun::new(RunId::new("0"));
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
        let mut run2 = TestRun::new(RunId::new("1"));
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
        let run = TestRun::new(RunId::new("0"));
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
                test_args: Some(vec!["-x".to_string(), "--maxfail=3".to_string()]),
            },
        )
        .unwrap();

        // Verify metadata was stored (open as new connection to verify)
        let db_path = temp.path().join(REPO_DIR).join("metadata.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        type MetadataRow = (
            Option<String>,
            Option<bool>,
            Option<String>,
            Option<i64>,
            Option<f64>,
            Option<i32>,
        );
        let (git_commit, git_dirty, command, concurrency, duration_secs, exit_code): MetadataRow =
            conn.query_row(
                "SELECT git_commit, git_dirty, command, concurrency, duration_secs, exit_code FROM runs WHERE id = ?",
                [run_id.as_str()],
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

        let run = TestRun::new(RunId::new("0"));
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
        let run = TestRun::new(RunId::new("0"));
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
                test_args: None,
            },
        )
        .unwrap();

        // Verify the new columns are populated
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (duration_secs, exit_code, git_dirty): (Option<f64>, Option<i32>, Option<bool>) = conn
            .query_row(
                "SELECT duration_secs, exit_code, git_dirty FROM runs WHERE id = ?",
                [run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert_eq!(duration_secs, Some(45.3));
        assert_eq!(exit_code, Some(0));
        assert_eq!(git_dirty, Some(false));
    }

    #[test]
    fn test_migrate_backfills_flakiness_cache() {
        let temp = TempDir::new().unwrap();
        let repo_dir = temp.path().join(REPO_DIR);
        fs::create_dir_all(repo_dir.join("runs")).unwrap();
        fs::write(repo_dir.join("format"), FORMAT_VERSION).unwrap();

        // Pre-cache schema: same as the migration test, no test_flakiness table.
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

            INSERT INTO runs (id, timestamp) VALUES (0, '2024-01-01T00:00:00Z');
            INSERT INTO runs (id, timestamp) VALUES (1, '2024-01-01T00:01:00Z');
            INSERT INTO runs (id, timestamp) VALUES (2, '2024-01-01T00:02:00Z');
            INSERT INTO test_results (run_id, test_id, status) VALUES (0, 'flap', 'success');
            INSERT INTO test_results (run_id, test_id, status) VALUES (1, 'flap', 'failure');
            INSERT INTO test_results (run_id, test_id, status) VALUES (2, 'flap', 'success');
            INSERT INTO test_results (run_id, test_id, status) VALUES (0, 'broken', 'failure');
            INSERT INTO test_results (run_id, test_id, status) VALUES (1, 'broken', 'failure');
            INSERT INTO test_results (run_id, test_id, status) VALUES (2, 'broken', 'failure');
            INSERT INTO test_results (run_id, test_id, status) VALUES (0, 'stable', 'success');
            INSERT INTO test_results (run_id, test_id, status) VALUES (1, 'stable', 'success');
            ",
        )
        .unwrap();
        drop(conn);

        let factory = InquestRepositoryFactory;
        let repo = factory.open(temp.path()).unwrap();

        let stats = repo.get_flakiness(2).unwrap();
        // stable never failed, so it must not appear.
        assert_eq!(stats.len(), 2);
        let flap = stats.iter().find(|s| s.test_id.as_str() == "flap").unwrap();
        assert_eq!(flap.runs, 3);
        assert_eq!(flap.failures, 1);
        assert_eq!(flap.transitions, 2);
        let broken = stats
            .iter()
            .find(|s| s.test_id.as_str() == "broken")
            .unwrap();
        assert_eq!(broken.runs, 3);
        assert_eq!(broken.failures, 3);
        assert_eq!(broken.transitions, 0);
    }

    #[test]
    fn test_flakiness_cache_handles_replaced_result() {
        // If the same (run_id, test_id) is written twice — e.g., a partial
        // re-run — the cache must reflect the latest status, not the sum of
        // both writes.
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run0 = TestRun::new(RunId::new("0"));
        run0.timestamp = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        run0.add_result(TestResult::success("t"));
        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run0, &mut writer).unwrap();
        drop(writer);
        run0.id = run_id;
        repo.replace_failing_tests(&run0).unwrap();

        let mut run1 = TestRun::new(RunId::new("1"));
        run1.timestamp = chrono::DateTime::from_timestamp(1_700_000_001, 0).unwrap();
        run1.add_result(TestResult::failure("t", "boom"));
        let (run_id, mut writer) = repo.begin_test_run_raw().unwrap();
        subunit_stream::write_stream(&run1, &mut writer).unwrap();
        drop(writer);
        run1.id = run_id;
        repo.replace_failing_tests(&run1).unwrap();

        // Now overwrite run 1's result with a success — single transition and
        // no failures left.
        let mut run1b = TestRun::new(run1.id.clone());
        run1b.timestamp = run1.timestamp;
        run1b.add_result(TestResult::success("t"));
        repo.replace_failing_tests(&run1b).unwrap();

        let stats = repo.get_flakiness(2).unwrap();
        // Both runs are now success; no failures means flakiness omits it.
        assert!(stats.is_empty());
    }

    #[test]
    fn test_test_results_stored_in_db() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
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

        let mut run = TestRun::new(RunId::new("0"));
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
        let parsed = subunit_stream::parse_stream(reader, RunId::new("failing")).unwrap();

        assert_eq!(parsed.results.len(), 2);
        assert!(parsed.results.contains_key(&TestId::new("test2")));
        assert!(parsed.results.contains_key(&TestId::new("test3")));
    }
}
