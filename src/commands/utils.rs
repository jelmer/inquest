//! Utility functions for command implementation

use crate::config::CONFIG_FILE_NAMES;
use crate::error::Result;
use crate::repository::inquest::InquestRepositoryFactory;
use crate::repository::testr::FileRepositoryFactory;
use crate::repository::{Repository, RepositoryFactory, TestRun};
use crate::ui::UI;
use std::path::Path;

/// Open a repository at the given path (or current directory if None)
///
/// Tries the inquest-native format (`.inquest/`) first, then falls back
/// to the legacy format (`.testrepository/`).
pub fn open_repository(base_path: Option<&str>) -> Result<Box<dyn Repository>> {
    let base = base_path.map(Path::new).unwrap_or_else(|| Path::new("."));

    // Try inquest-native format first
    let inquest_factory = InquestRepositoryFactory;
    if let Ok(repo) = inquest_factory.open(base) {
        return Ok(repo);
    }

    // Fall back to legacy format
    let file_factory = FileRepositoryFactory;
    file_factory.open(base)
}

/// Initialize a repository at the given path (or current directory if None)
///
/// Uses the inquest-native format (`.inquest/`) by default.
pub fn init_repository(base_path: Option<&str>) -> Result<Box<dyn Repository>> {
    let base = base_path.map(Path::new).unwrap_or_else(|| Path::new("."));

    let factory = InquestRepositoryFactory;
    factory.initialise(base)
}

/// Check whether a configuration file exists in the given directory.
pub fn has_config_file(base_path: Option<&str>) -> bool {
    let base = base_path.map(Path::new).unwrap_or_else(|| Path::new("."));
    CONFIG_FILE_NAMES
        .iter()
        .any(|name| base.join(name).exists())
}

/// Open a repository, auto-initializing if a configuration file is present.
///
/// If `force_init` is true, always initializes on failure.
/// Otherwise, if a configuration file exists in the base directory,
/// initializes and prints a notification via `ui`.
pub fn open_or_init_repository(
    base_path: Option<&str>,
    force_init: bool,
    ui: &mut dyn UI,
) -> Result<Box<dyn Repository>> {
    match open_repository(base_path) {
        Ok(repo) => Ok(repo),
        Err(e) => {
            if force_init || has_config_file(base_path) {
                let base = base_path.unwrap_or(".");
                let repo_path = Path::new(base).join(".inquest");
                ui.output(&format!("Creating repository in {}", repo_path.display()))?;
                init_repository(base_path)
            } else {
                Err(e)
            }
        }
    }
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

/// Capture and store run metadata (git commit, command, concurrency, duration, exit code)
pub fn store_run_metadata(
    repo: &mut Box<dyn Repository>,
    run_id: &str,
    command: Option<&str>,
    concurrency: Option<u32>,
    duration: Option<std::time::Duration>,
    exit_code: Option<i32>,
) -> Result<()> {
    let git_commit = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        });

    let metadata = crate::repository::RunMetadata {
        git_commit,
        command: command.map(|s| s.to_string()),
        concurrency,
        duration_secs: duration.map(|d| d.as_secs_f64()),
        exit_code,
    };

    repo.set_run_metadata(run_id, metadata)
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

    if let Some(interruption) = &test_run.interruption {
        ui.output(&format!(
            "  WARNING: Stream interrupted ({}), results may be incomplete",
            interruption
        ))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_has_config_file_none() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        assert!(!has_config_file(Some(&path)));
    }

    #[test]
    fn test_has_config_file_inquest_toml() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("inquest.toml"), r#"test_command = "echo""#).unwrap();
        let path = temp.path().to_string_lossy().to_string();
        assert!(has_config_file(Some(&path)));
    }

    #[test]
    fn test_has_config_file_testr_conf() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join(".testr.conf"),
            "[DEFAULT]\ntest_command=echo\n",
        )
        .unwrap();
        let path = temp.path().to_string_lossy().to_string();
        assert!(has_config_file(Some(&path)));
    }

    #[test]
    fn test_open_or_init_auto_creates_when_config_exists() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("inquest.toml"), r#"test_command = "echo""#).unwrap();
        let path = temp.path().to_string_lossy().to_string();

        let mut ui = crate::ui::test_ui::TestUI::new();
        let repo = open_or_init_repository(Some(&path), false, &mut ui);
        assert!(repo.is_ok());
        assert!(temp.path().join(".inquest").exists());
        let output = ui.output.join("\n");
        assert!(output.contains("Creating repository"), "got: {}", output);
    }

    #[test]
    fn test_open_or_init_no_config_no_force() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();

        let mut ui = crate::ui::test_ui::TestUI::new();
        let repo = open_or_init_repository(Some(&path), false, &mut ui);
        assert!(repo.is_err());
        assert!(!temp.path().join(".inquest").exists());
    }

    #[test]
    fn test_open_or_init_force_init_without_config() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();

        let mut ui = crate::ui::test_ui::TestUI::new();
        let repo = open_or_init_repository(Some(&path), true, &mut ui);
        assert!(repo.is_ok());
        assert!(temp.path().join(".inquest").exists());
    }

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
