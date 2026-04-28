//! Upgrade a legacy .testrepository/ to the new .inquest/ format

use crate::commands::Command;
use crate::error::{Error, Result};
use crate::repository::inquest::InquestRepositoryFactory;
use crate::repository::testr::FileRepositoryFactory;
use crate::repository::{Repository, RepositoryFactory};
use crate::ui::UI;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;

/// Command to upgrade a legacy .testrepository/ repository to the new .inquest/ format.
///
/// Copies all test run data, failing tests, and timing information from the
/// old format into a new `.inquest/` directory with SQLite metadata storage.
pub struct UpgradeCommand {
    base_path: Option<String>,
}

impl UpgradeCommand {
    /// Creates a new upgrade command.
    ///
    /// # Arguments
    /// * `base_path` - Optional base directory path containing the repository
    pub fn new(base_path: Option<String>) -> Self {
        UpgradeCommand { base_path }
    }
}

impl Command for UpgradeCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = self
            .base_path
            .as_deref()
            .map(Path::new)
            .unwrap_or_else(|| Path::new("."));

        let old_path = base.join(".testrepository");
        let new_path = base.join(".inquest");

        // Check that old repository exists
        let old_factory = FileRepositoryFactory;
        let old_repo = old_factory
            .open(base)
            .map_err(|_| Error::RepositoryNotFound(old_path.clone()))?;

        // Check that new repository does not already exist
        if new_path.exists() {
            return Err(Error::RepositoryExists(new_path));
        }

        // Create new repository
        let new_factory = InquestRepositoryFactory;
        let mut new_repo = new_factory.initialise(base)?;

        // Migrate all test runs
        let run_ids = old_repo.list_run_ids()?;
        let total_runs = run_ids.len();
        ui.output(&format!(
            "Upgrading {} test run{} from {} to {}",
            total_runs,
            if total_runs == 1 { "" } else { "s" },
            old_path.display(),
            new_path.display(),
        ))?;

        let progress = make_progress_bar(total_runs as u64);
        for run_id in &run_ids {
            let test_run = old_repo.get_test_run(run_id)?;
            new_repo.insert_test_run(test_run)?;
            progress.inc(1);
        }
        progress.finish_and_clear();

        // Restore the correct failing tests state from the old repo.
        // During run migration, each insert_test_run called replace_failing_tests,
        // so the new repo's state reflects the last run. We need to overwrite with
        // the old repo's actual failing state.
        migrate_failing_tests(&*old_repo, new_repo.as_mut(), &run_ids)?;

        // Try to migrate any additional test times that the old repo may have
        // beyond what was extracted from individual runs.
        let times = old_repo.get_test_times()?;
        if !times.is_empty() {
            new_repo.update_test_times(&times)?;
        }

        ui.output(&format!(
            "Upgrade complete. You can remove {} when satisfied.",
            old_path.display(),
        ))?;

        Ok(0)
    }

    fn name(&self) -> &str {
        "upgrade"
    }

    fn help(&self) -> &str {
        "Upgrade a .testrepository/ to .inquest/ format"
    }
}

fn make_progress_bar(total: u64) -> ProgressBar {
    if total == 0 {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} runs ({eta} remaining)")
            .unwrap()
            .progress_chars("█▓▒░  "),
    );
    pb
}

/// Restore the old repo's failing tests state in the new repo
fn migrate_failing_tests(
    old_repo: &dyn Repository,
    new_repo: &mut dyn Repository,
    run_ids: &[crate::repository::RunId],
) -> Result<()> {
    let failing_ids = old_repo.get_failing_tests()?;
    let last_run_id = run_ids
        .last()
        .cloned()
        .unwrap_or_else(|| crate::repository::RunId::new("0"));

    if failing_ids.is_empty() {
        // Clear any failing state that was set during run migration
        let mut clear_run = crate::repository::TestRun::new(last_run_id);
        clear_run.timestamp = chrono::Utc::now();
        new_repo.replace_failing_tests(&clear_run)?;
        return Ok(());
    }

    // Construct a TestRun representing the failing state by looking up
    // each failing test's details from the runs that last recorded it.
    let mut failing_run = crate::repository::TestRun::new(last_run_id);
    failing_run.timestamp = chrono::Utc::now();

    for test_id in &failing_ids {
        let mut found = false;
        // Search runs in reverse order to find the most recent result
        for rid in run_ids.iter().rev() {
            if let Ok(run) = old_repo.get_test_run(rid) {
                if let Some(result) = run.results.get(test_id) {
                    failing_run.add_result(result.clone());
                    found = true;
                    break;
                }
            }
        }
        if !found {
            failing_run.add_result(crate::repository::TestResult::failure(
                test_id.clone(),
                "migrated from .testrepository",
            ));
        }
    }

    new_repo.replace_failing_tests(&failing_run)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::testr::FileRepositoryFactory;
    use crate::repository::{RepositoryFactory, RunId, TestResult, TestRun};
    use crate::ui::test_ui::TestUI;
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn test_upgrade_empty_repository() {
        let temp = TempDir::new().unwrap();

        // Create an old-format repository
        let factory = FileRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        let mut ui = TestUI::new();
        let cmd = UpgradeCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert!(temp.path().join(".inquest").exists());
        assert!(temp.path().join(".inquest/format").exists());
        assert!(temp.path().join(".inquest/metadata.db").exists());

        let output = ui.output.join("\n");
        assert!(output.contains("0 test runs"), "got: {}", output);
        assert!(output.contains("Upgrade complete"), "got: {}", output);
    }

    #[test]
    fn test_upgrade_with_runs() {
        let temp = TempDir::new().unwrap();

        // Create an old-format repository with test runs
        let factory = FileRepositoryFactory;
        let mut old_repo = factory.initialise(temp.path()).unwrap();

        let mut run1 = TestRun::new(RunId::new("0"));
        run1.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run1.add_result(TestResult::success("test1").with_duration(Duration::from_secs(1)));
        run1.add_result(TestResult::failure("test2", "Failed"));
        old_repo.insert_test_run(run1).unwrap();

        let mut run2 = TestRun::new(RunId::new("1"));
        run2.timestamp = chrono::DateTime::from_timestamp(1000000001, 0).unwrap();
        run2.add_result(TestResult::success("test1").with_duration(Duration::from_secs(2)));
        run2.add_result(TestResult::success("test2"));
        old_repo.insert_test_run(run2).unwrap();

        drop(old_repo);

        // Upgrade
        let mut ui = TestUI::new();
        let cmd = UpgradeCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);
        assert_eq!(result.unwrap(), 0);

        let output = ui.output.join("\n");
        assert!(output.contains("2 test runs"), "got: {}", output);

        // Verify new repository has both runs
        let new_factory = InquestRepositoryFactory;
        let new_repo = new_factory.open(temp.path()).unwrap();
        assert_eq!(new_repo.count().unwrap(), 2);
        assert_eq!(
            new_repo.list_run_ids().unwrap(),
            vec![RunId::new("0"), RunId::new("1")]
        );

        // Verify latest run contents
        let latest = new_repo.get_latest_run().unwrap();
        assert_eq!(latest.total_tests(), 2);
    }

    #[test]
    fn test_upgrade_preserves_failing_tests() {
        let temp = TempDir::new().unwrap();

        // Create old repo with failing tests
        let factory = FileRepositoryFactory;
        let mut old_repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::success("test1"));
        run.add_result(TestResult::failure("test2", "Failed"));
        run.add_result(TestResult::failure("test3", "Also failed"));
        old_repo.insert_test_run(run).unwrap();

        drop(old_repo);

        // Upgrade
        let mut ui = TestUI::new();
        let cmd = UpgradeCommand::new(Some(temp.path().to_string_lossy().to_string()));
        assert_eq!(cmd.execute(&mut ui).unwrap(), 0);

        // Verify failing tests were migrated
        let new_factory = InquestRepositoryFactory;
        let new_repo = new_factory.open(temp.path()).unwrap();
        let failing = new_repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 2);
        assert!(failing.iter().any(|id| id.as_str() == "test2"));
        assert!(failing.iter().any(|id| id.as_str() == "test3"));
    }

    #[test]
    fn test_upgrade_preserves_test_times() {
        let temp = TempDir::new().unwrap();

        // Create old repo with timed tests
        let factory = FileRepositoryFactory;
        let mut old_repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(TestResult::success("test1").with_duration(Duration::from_secs_f64(1.5)));
        run.add_result(TestResult::success("test2").with_duration(Duration::from_secs_f64(0.3)));
        old_repo.insert_test_run(run).unwrap();

        drop(old_repo);

        // Upgrade
        let mut ui = TestUI::new();
        let cmd = UpgradeCommand::new(Some(temp.path().to_string_lossy().to_string()));
        assert_eq!(cmd.execute(&mut ui).unwrap(), 0);

        // Verify times were migrated
        let new_factory = InquestRepositoryFactory;
        let new_repo = new_factory.open(temp.path()).unwrap();
        let test_ids = vec![
            crate::repository::TestId::new("test1"),
            crate::repository::TestId::new("test2"),
        ];
        let times = new_repo.get_test_times_for_ids(&test_ids).unwrap();
        assert_eq!(times.len(), 2);
        assert_eq!(
            times
                .get(&crate::repository::TestId::new("test1"))
                .unwrap()
                .as_secs_f64(),
            1.5
        );
    }

    #[test]
    fn test_upgrade_no_old_repository() {
        let temp = TempDir::new().unwrap();

        let mut ui = TestUI::new();
        let cmd = UpgradeCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);
        assert!(result.is_err());
    }

    #[test]
    fn test_upgrade_new_repository_already_exists() {
        let temp = TempDir::new().unwrap();

        // Create both old and new repos
        let old_factory = FileRepositoryFactory;
        old_factory.initialise(temp.path()).unwrap();

        let new_factory = InquestRepositoryFactory;
        new_factory.initialise(temp.path()).unwrap();

        let mut ui = TestUI::new();
        let cmd = UpgradeCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);
        assert!(result.is_err());
    }
}
