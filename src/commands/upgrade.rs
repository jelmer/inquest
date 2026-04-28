//! Upgrade a legacy .testrepository/ to the new .inquest/ format

use crate::commands::Command;
use crate::config::TestrConfig;
use crate::error::{Error, Result};
use crate::repository::inquest::InquestRepositoryFactory;
use crate::repository::testr::FileRepositoryFactory;
use crate::repository::{Repository, RepositoryFactory};
use crate::ui::UI;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};

/// Command to upgrade a legacy `.testrepository/` repository and/or
/// `.testr.conf` file to the new `.inquest/` directory and `inquest.toml`.
///
/// Copies all test run data, failing tests, and timing information from the
/// old format into a new `.inquest/` directory with SQLite metadata storage.
/// If a legacy `.testr.conf` is present and no TOML config file already
/// exists, it is rewritten as `inquest.toml`.
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
        let testr_conf_path = base.join(".testr.conf");

        let has_old_repo = old_path.exists();
        let has_testr_conf = testr_conf_path.exists();

        if !has_old_repo && !has_testr_conf {
            return Err(Error::RepositoryNotFound(old_path));
        }

        let mut did_anything = false;

        if has_old_repo {
            upgrade_repository(base, &old_path, &new_path, ui)?;
            did_anything = true;
        }

        if has_testr_conf {
            if let Some(written) = upgrade_config(base, &testr_conf_path, ui)? {
                ui.output(&format!(
                    "Rewrote {} as {}. You can remove {} when satisfied.",
                    testr_conf_path.display(),
                    written.display(),
                    testr_conf_path.display(),
                ))?;
                did_anything = true;
            }
        }

        if !did_anything {
            // Old repo was absent and the .testr.conf rewrite was skipped
            // because a TOML config already existed. Nothing to do.
            ui.output("Nothing to upgrade.")?;
        }

        Ok(0)
    }

    fn name(&self) -> &str {
        "upgrade"
    }

    fn help(&self) -> &str {
        "Upgrade legacy .testrepository/ and/or .testr.conf to the new format"
    }
}

/// Migrate `.testrepository/` to `.inquest/`.
fn upgrade_repository(
    base: &Path,
    old_path: &Path,
    new_path: &Path,
    ui: &mut dyn UI,
) -> Result<()> {
    let old_factory = FileRepositoryFactory;
    let old_repo = old_factory
        .open(base)
        .map_err(|_| Error::RepositoryNotFound(old_path.to_path_buf()))?;

    if new_path.exists() {
        return Err(Error::RepositoryExists(new_path.to_path_buf()));
    }

    let new_factory = InquestRepositoryFactory;
    let mut new_repo = new_factory.initialise(base)?;

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

    Ok(())
}

/// Rewrite `.testr.conf` as `inquest.toml`. Returns the path written, or
/// `None` if a TOML config already exists and the rewrite was skipped.
fn upgrade_config(base: &Path, testr_conf_path: &Path, ui: &mut dyn UI) -> Result<Option<PathBuf>> {
    let toml_path = base.join("inquest.toml");
    let dot_toml_path = base.join(".inquest.toml");

    if toml_path.exists() {
        ui.warning(&format!(
            "{} already exists; skipping rewrite of {}",
            toml_path.display(),
            testr_conf_path.display(),
        ))?;
        return Ok(None);
    }
    if dot_toml_path.exists() {
        ui.warning(&format!(
            "{} already exists; skipping rewrite of {}",
            dot_toml_path.display(),
            testr_conf_path.display(),
        ))?;
        return Ok(None);
    }

    let config = TestrConfig::load_from_file(testr_conf_path)?;
    let toml_content = config.to_toml()?;
    std::fs::write(&toml_path, toml_content)
        .map_err(|e| Error::Config(format!("Failed to write {}: {}", toml_path.display(), e)))?;

    Ok(Some(toml_path))
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

    #[test]
    fn test_upgrade_only_testr_conf() {
        let temp = TempDir::new().unwrap();

        // Only a .testr.conf, no .testrepository/
        std::fs::write(
            temp.path().join(".testr.conf"),
            "[DEFAULT]\n\
             test_command=python -m subunit.run discover\n\
             test_id_option=--load-list $IDFILE\n",
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = UpgradeCommand::new(Some(temp.path().to_string_lossy().to_string()));
        assert_eq!(cmd.execute(&mut ui).unwrap(), 0);

        // .inquest/ should not be created (no .testrepository/ to migrate)
        assert!(!temp.path().join(".inquest").exists());

        // inquest.toml should be created with the same settings
        let toml_path = temp.path().join("inquest.toml");
        assert!(toml_path.exists());

        let parsed = TestrConfig::load_from_file(&toml_path).unwrap();
        assert_eq!(parsed.test_command, "python -m subunit.run discover");
        assert_eq!(
            parsed.test_id_option.as_deref(),
            Some("--load-list $IDFILE")
        );

        let output = ui.output.join("\n");
        assert!(output.contains("Rewrote"), "got: {}", output);
        assert!(output.contains(".testr.conf"), "got: {}", output);
    }

    #[test]
    fn test_upgrade_both_testr_conf_and_testrepository() {
        let temp = TempDir::new().unwrap();

        // Old-format repository AND .testr.conf
        let factory = FileRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        std::fs::write(
            temp.path().join(".testr.conf"),
            "[DEFAULT]\ntest_command=cargo test\n",
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = UpgradeCommand::new(Some(temp.path().to_string_lossy().to_string()));
        assert_eq!(cmd.execute(&mut ui).unwrap(), 0);

        // Both .inquest/ and inquest.toml should now exist
        assert!(temp.path().join(".inquest").exists());
        let toml_path = temp.path().join("inquest.toml");
        assert!(toml_path.exists());

        let parsed = TestrConfig::load_from_file(&toml_path).unwrap();
        assert_eq!(parsed.test_command, "cargo test");
    }

    #[test]
    fn test_upgrade_testr_conf_skipped_when_toml_exists() {
        let temp = TempDir::new().unwrap();

        let factory = FileRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        std::fs::write(
            temp.path().join(".testr.conf"),
            "[DEFAULT]\ntest_command=from-testr\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("inquest.toml"),
            "test_command = \"from-toml\"\n",
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = UpgradeCommand::new(Some(temp.path().to_string_lossy().to_string()));
        assert_eq!(cmd.execute(&mut ui).unwrap(), 0);

        // inquest.toml must be untouched
        let parsed = TestrConfig::load_from_file(&temp.path().join("inquest.toml")).unwrap();
        assert_eq!(parsed.test_command, "from-toml");

        // A warning should have been emitted about the skipped rewrite
        let warnings = ui.errors.join("\n");
        assert!(
            warnings.contains("inquest.toml") && warnings.contains("skipping"),
            "got: {}",
            warnings
        );
    }

    #[test]
    fn test_upgrade_only_testr_conf_with_existing_inquest_toml() {
        let temp = TempDir::new().unwrap();

        // Only the legacy config exists, but a TOML config is also present.
        // Nothing to migrate (no .testrepository/) and the rewrite is skipped.
        std::fs::write(
            temp.path().join(".testr.conf"),
            "[DEFAULT]\ntest_command=from-testr\n",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("inquest.toml"),
            "test_command = \"from-toml\"\n",
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = UpgradeCommand::new(Some(temp.path().to_string_lossy().to_string()));
        assert_eq!(cmd.execute(&mut ui).unwrap(), 0);

        let output = ui.output.join("\n");
        assert!(output.contains("Nothing to upgrade"), "got: {}", output);
    }
}
