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

/// Resolve a run ID: if given, use it; otherwise get the latest run's ID.
///
/// Supports negative indices like Python: -1 is the latest run, -2 is the
/// second-to-latest, etc.
pub fn resolve_run_id(repo: &dyn Repository, run_id: Option<&str>) -> Result<String> {
    match run_id {
        Some(id) => {
            if let Some(neg) = id.strip_prefix('-') {
                if let Ok(offset) = neg.parse::<usize>() {
                    if offset == 0 {
                        return Err(crate::error::Error::Other(
                            "Run index -0 is not valid; use -1 for the latest run".to_string(),
                        ));
                    }
                    let ids = repo.list_run_ids()?;
                    if offset > ids.len() {
                        return Err(crate::error::Error::TestRunNotFound(id.to_string()));
                    }
                    return Ok(ids[ids.len() - offset].clone());
                }
            }
            Ok(id.to_string())
        }
        None => {
            let latest = repo.get_latest_run()?;
            Ok(latest.id)
        }
    }
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

/// Store metadata for a test run.
///
/// Detects git information automatically and combines it with the
/// provided run-specific metadata (command, concurrency, duration, exit code).
pub fn store_run_metadata(
    repo: &mut Box<dyn Repository>,
    run_id: &str,
    working_dir: Option<&str>,
    command: Option<&str>,
    concurrency: Option<usize>,
    duration: Option<std::time::Duration>,
    exit_code: Option<i32>,
) -> Result<()> {
    let mut metadata = crate::repository::RunMetadata {
        command: command.map(|s| s.to_string()),
        concurrency,
        duration_secs: duration.map(|d| d.as_secs_f64()),
        exit_code,
        ..Default::default()
    };

    if let Some(commit) = detect_git_commit(working_dir) {
        metadata.git_dirty = Some(detect_git_dirty(working_dir));
        metadata.git_commit = Some(commit);
    }

    repo.set_run_metadata(run_id, &metadata)
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

/// Detect the current git HEAD commit SHA.
///
/// Returns `None` if not in a git repository or if `git` is not available.
pub fn detect_git_commit(working_dir: Option<&str>) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["rev-parse", "HEAD"]);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8(output.stdout).ok()?;
    let commit = commit.trim();
    if commit.is_empty() {
        None
    } else {
        Some(commit.to_string())
    }
}

/// Detect whether the git working tree has uncommitted changes.
///
/// Returns `true` if the tree is dirty, `false` if pristine.
/// If git is not available or not in a repo, returns `false`.
pub fn detect_git_dirty(working_dir: Option<&str>) -> bool {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["status", "--porcelain"]);
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let output = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    !output.stdout.is_empty()
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

    #[test]
    fn test_resolve_run_id_none() {
        let temp = TempDir::new().unwrap();
        let mut repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();
        let mut run = crate::repository::TestRun::new("0".to_string());
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(crate::repository::TestResult::success("test1"));
        repo.insert_test_run(run).unwrap();

        assert_eq!(resolve_run_id(&*repo, None).unwrap(), "0");
    }

    #[test]
    fn test_resolve_run_id_positive() {
        let temp = TempDir::new().unwrap();
        let repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        assert_eq!(resolve_run_id(&*repo, Some("42")).unwrap(), "42");
    }

    #[test]
    fn test_resolve_run_id_negative() {
        let temp = TempDir::new().unwrap();
        let mut repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        for i in 0..3 {
            let mut run = crate::repository::TestRun::new(i.to_string());
            run.timestamp = chrono::DateTime::from_timestamp(1000000000 + i, 0).unwrap();
            run.add_result(crate::repository::TestResult::success("test1"));
            repo.insert_test_run(run).unwrap();
        }

        assert_eq!(resolve_run_id(&*repo, Some("-1")).unwrap(), "2");
        assert_eq!(resolve_run_id(&*repo, Some("-2")).unwrap(), "1");
        assert_eq!(resolve_run_id(&*repo, Some("-3")).unwrap(), "0");
    }

    #[test]
    fn test_resolve_run_id_negative_out_of_range() {
        let temp = TempDir::new().unwrap();
        let mut repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        let mut run = crate::repository::TestRun::new("0".to_string());
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(crate::repository::TestResult::success("test1"));
        repo.insert_test_run(run).unwrap();

        assert!(resolve_run_id(&*repo, Some("-2")).is_err());
    }

    #[test]
    fn test_detect_git_commit_in_git_repo() {
        // This test runs inside the inquest git repo, so it should find a commit
        let commit = detect_git_commit(None);
        assert!(commit.is_some(), "Expected to find a git commit");
        let commit = commit.unwrap();
        assert_eq!(commit.len(), 40, "Git commit SHA should be 40 hex chars");
        assert!(
            commit.chars().all(|c| c.is_ascii_hexdigit()),
            "Git commit should be hex"
        );
    }

    #[test]
    fn test_detect_git_commit_not_git_repo() {
        let temp = TempDir::new().unwrap();
        let commit = detect_git_commit(Some(&temp.path().to_string_lossy()));
        assert_eq!(commit, None);
    }

    #[test]
    fn test_store_run_metadata() {
        let temp = TempDir::new().unwrap();
        let mut repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        let mut run = crate::repository::TestRun::new("0".to_string());
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(crate::repository::TestResult::success("test1"));
        repo.insert_test_run(run).unwrap();

        // Store metadata using a non-git directory (git fields should be None)
        store_run_metadata(
            &mut repo,
            "0",
            Some(&temp.path().to_string_lossy()),
            Some("pytest"),
            Some(2),
            Some(std::time::Duration::from_secs_f64(1.5)),
            Some(0),
        )
        .unwrap();
        let metadata = repo.get_run_metadata("0").unwrap().unwrap();
        assert_eq!(metadata.git_commit, None);
        assert_eq!(metadata.git_dirty, None);
        assert_eq!(metadata.command, Some("pytest".to_string()));
        assert_eq!(metadata.concurrency, Some(2));
        assert_eq!(metadata.duration_secs, Some(1.5));
        assert_eq!(metadata.exit_code, Some(0));
    }

    #[test]
    fn test_resolve_run_id_negative_zero() {
        let temp = TempDir::new().unwrap();
        let mut repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        let mut run = crate::repository::TestRun::new("0".to_string());
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(crate::repository::TestResult::success("test1"));
        repo.insert_test_run(run).unwrap();

        assert!(resolve_run_id(&*repo, Some("-0")).is_err());
    }
}
