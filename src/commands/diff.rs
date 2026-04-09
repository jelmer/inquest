//! Compare two test runs and show what changed

use crate::commands::utils::{open_repository, resolve_run_id};
use crate::commands::Command;
use crate::error::Result;
use crate::repository::{TestId, TestRun, TestStatus};
use crate::ui::UI;
use std::collections::BTreeSet;

/// Command to compare two test runs and show differences.
///
/// Shows new failures, new passes, added tests, and removed tests.
/// Defaults to comparing the last two runs.
pub struct DiffCommand {
    base_path: Option<String>,
    run1: Option<String>,
    run2: Option<String>,
}

impl DiffCommand {
    /// Creates a new diff command.
    ///
    /// If both `run1` and `run2` are `None`, compares the last two runs.
    /// If only `run1` is given, compares it against the latest run.
    pub fn new(base_path: Option<String>, run1: Option<String>, run2: Option<String>) -> Self {
        DiffCommand {
            base_path,
            run1,
            run2,
        }
    }
}

/// Categorized differences between two test runs.
struct RunDiff<'a> {
    /// Tests that were passing in run1 but now fail in run2.
    new_failures: Vec<(&'a TestId, &'a TestStatus)>,
    /// Tests that were failing in run1 but now pass in run2.
    new_passes: Vec<&'a TestId>,
    /// Tests present in run2 but not in run1.
    added: Vec<(&'a TestId, &'a TestStatus)>,
    /// Tests present in run1 but not in run2.
    removed: Vec<(&'a TestId, &'a TestStatus)>,
    /// Tests whose status changed in some other way (e.g. error -> failure).
    status_changed: Vec<(&'a TestId, &'a TestStatus, &'a TestStatus)>,
}

fn compute_diff<'a>(run1: &'a TestRun, run2: &'a TestRun) -> RunDiff<'a> {
    let ids1: BTreeSet<&TestId> = run1.results.keys().collect();
    let ids2: BTreeSet<&TestId> = run2.results.keys().collect();

    let mut new_failures = Vec::new();
    let mut new_passes = Vec::new();
    let mut status_changed = Vec::new();

    // Tests in both runs: check for status changes
    for id in ids1.intersection(&ids2) {
        let r1 = &run1.results[*id];
        let r2 = &run2.results[*id];
        if r1.status == r2.status {
            continue;
        }
        if r2.status.is_failure() && r1.status.is_success() {
            new_failures.push((&r2.test_id, &r2.status));
        } else if r2.status.is_success() && r1.status.is_failure() {
            new_passes.push(&r2.test_id);
        } else {
            status_changed.push((&r2.test_id, &r1.status, &r2.status));
        }
    }

    // Tests only in run2: added
    let mut added: Vec<_> = ids2
        .difference(&ids1)
        .map(|id| {
            let r = &run2.results[*id];
            (&r.test_id, &r.status)
        })
        .collect();

    // Tests only in run1: removed
    let mut removed: Vec<_> = ids1
        .difference(&ids2)
        .map(|id| {
            let r = &run1.results[*id];
            (&r.test_id, &r.status)
        })
        .collect();

    // Sort all lists by test ID for stable output
    new_failures.sort_by(|a, b| a.0.cmp(b.0));
    new_passes.sort();
    added.sort_by(|a, b| a.0.cmp(b.0));
    removed.sort_by(|a, b| a.0.cmp(b.0));
    status_changed.sort_by(|a, b| a.0.cmp(b.0));

    RunDiff {
        new_failures,
        new_passes,
        added,
        removed,
        status_changed,
    }
}

impl Command for DiffCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let repo = open_repository(self.base_path.as_deref())?;

        // Resolve run IDs. If neither is given, compare the last two runs.
        let (id1, id2) = match (&self.run1, &self.run2) {
            (Some(r1), Some(r2)) => (
                resolve_run_id(&*repo, Some(r1))?,
                resolve_run_id(&*repo, Some(r2))?,
            ),
            (Some(r1), None) => {
                // One run given: compare it against the latest
                let id1 = resolve_run_id(&*repo, Some(r1))?;
                let id2 = resolve_run_id(&*repo, None)?;
                (id1, id2)
            }
            (None, None) => {
                // No runs given: compare the last two
                let ids = repo.list_run_ids()?;
                if ids.len() < 2 {
                    return Err(crate::error::Error::Other(
                        "Need at least 2 test runs to diff".to_string(),
                    ));
                }
                (ids[ids.len() - 2].clone(), ids[ids.len() - 1].clone())
            }
            (None, Some(_)) => unreachable!("clap ensures run1 is provided before run2"),
        };

        let run1 = repo.get_test_run(&id1)?;
        let run2 = repo.get_test_run(&id2)?;

        let diff = compute_diff(&run1, &run2);

        ui.output(&format!("Comparing run {} → {}", id1, id2))?;

        let has_changes = !diff.new_failures.is_empty()
            || !diff.new_passes.is_empty()
            || !diff.added.is_empty()
            || !diff.removed.is_empty()
            || !diff.status_changed.is_empty();

        if !has_changes {
            ui.output("No changes.")?;
            return Ok(0);
        }

        if !diff.new_failures.is_empty() {
            ui.output(&format!("\nNew failures ({}):", diff.new_failures.len()))?;
            for (id, status) in &diff.new_failures {
                ui.output(&format!("  {} ({})", id, status))?;
            }
        }

        if !diff.new_passes.is_empty() {
            ui.output(&format!("\nNew passes ({}):", diff.new_passes.len()))?;
            for id in &diff.new_passes {
                ui.output(&format!("  {}", id))?;
            }
        }

        if !diff.status_changed.is_empty() {
            ui.output(&format!(
                "\nStatus changed ({}):",
                diff.status_changed.len()
            ))?;
            for (id, old, new) in &diff.status_changed {
                ui.output(&format!("  {} ({} → {})", id, old, new))?;
            }
        }

        if !diff.added.is_empty() {
            ui.output(&format!("\nAdded tests ({}):", diff.added.len()))?;
            for (id, status) in &diff.added {
                ui.output(&format!("  {} ({})", id, status))?;
            }
        }

        if !diff.removed.is_empty() {
            ui.output(&format!("\nRemoved tests ({}):", diff.removed.len()))?;
            for (id, status) in &diff.removed {
                ui.output(&format!("  {} ({})", id, status))?;
            }
        }

        // Exit code 1 if there are new failures
        if diff.new_failures.is_empty() {
            Ok(0)
        } else {
            Ok(1)
        }
    }

    fn name(&self) -> &str {
        "diff"
    }

    fn help(&self) -> &str {
        "Compare two test runs and show what changed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, RunId, TestResult};
    use crate::ui::test_ui::TestUI;
    use tempfile::TempDir;

    fn setup_repo(temp: &TempDir) -> Box<dyn crate::repository::Repository> {
        let factory = InquestRepositoryFactory;
        factory.initialise(temp.path()).unwrap()
    }

    fn make_run(id: &str, timestamp_offset: i64) -> TestRun {
        let mut run = TestRun::new(RunId::new(id));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000 + timestamp_offset, 0).unwrap();
        run
    }

    #[test]
    fn test_diff_no_changes() {
        let temp = TempDir::new().unwrap();
        let mut repo = setup_repo(&temp);

        let mut run1 = make_run("0", 0);
        run1.add_result(TestResult::success("test1"));
        run1.add_result(TestResult::success("test2"));
        repo.insert_test_run(run1).unwrap();

        let mut run2 = make_run("1", 1);
        run2.add_result(TestResult::success("test1"));
        run2.add_result(TestResult::success("test2"));
        repo.insert_test_run(run2).unwrap();

        let mut ui = TestUI::new();
        let cmd = DiffCommand::new(Some(temp.path().to_string_lossy().to_string()), None, None);
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert!(ui.output.iter().any(|s| s == "No changes."));
    }

    #[test]
    fn test_diff_new_failure() {
        let temp = TempDir::new().unwrap();
        let mut repo = setup_repo(&temp);

        let mut run1 = make_run("0", 0);
        run1.add_result(TestResult::success("test1"));
        run1.add_result(TestResult::success("test2"));
        repo.insert_test_run(run1).unwrap();

        let mut run2 = make_run("1", 1);
        run2.add_result(TestResult::success("test1"));
        run2.add_result(TestResult::failure("test2", "broke"));
        repo.insert_test_run(run2).unwrap();

        let mut ui = TestUI::new();
        let cmd = DiffCommand::new(Some(temp.path().to_string_lossy().to_string()), None, None);
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 1);
        let output = ui.output.join("\n");
        assert!(output.contains("New failures (1)"), "got: {}", output);
        assert!(output.contains("test2 (failure)"), "got: {}", output);
    }

    #[test]
    fn test_diff_new_pass() {
        let temp = TempDir::new().unwrap();
        let mut repo = setup_repo(&temp);

        let mut run1 = make_run("0", 0);
        run1.add_result(TestResult::failure("test1", "broke"));
        repo.insert_test_run(run1).unwrap();

        let mut run2 = make_run("1", 1);
        run2.add_result(TestResult::success("test1"));
        repo.insert_test_run(run2).unwrap();

        let mut ui = TestUI::new();
        let cmd = DiffCommand::new(Some(temp.path().to_string_lossy().to_string()), None, None);
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        let output = ui.output.join("\n");
        assert!(output.contains("New passes (1)"), "got: {}", output);
        assert!(output.contains("test1"), "got: {}", output);
    }

    #[test]
    fn test_diff_added_and_removed() {
        let temp = TempDir::new().unwrap();
        let mut repo = setup_repo(&temp);

        let mut run1 = make_run("0", 0);
        run1.add_result(TestResult::success("test1"));
        run1.add_result(TestResult::success("old_test"));
        repo.insert_test_run(run1).unwrap();

        let mut run2 = make_run("1", 1);
        run2.add_result(TestResult::success("test1"));
        run2.add_result(TestResult::success("new_test"));
        repo.insert_test_run(run2).unwrap();

        let mut ui = TestUI::new();
        let cmd = DiffCommand::new(Some(temp.path().to_string_lossy().to_string()), None, None);
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        let output = ui.output.join("\n");
        assert!(output.contains("Added tests (1)"), "got: {}", output);
        assert!(output.contains("new_test"), "got: {}", output);
        assert!(output.contains("Removed tests (1)"), "got: {}", output);
        assert!(output.contains("old_test"), "got: {}", output);
    }

    #[test]
    fn test_diff_status_changed() {
        let temp = TempDir::new().unwrap();
        let mut repo = setup_repo(&temp);

        // Use success → skip: both are success-like but different statuses,
        // so this tests the status_changed path (not new_failures or new_passes).
        let mut run1 = make_run("0", 0);
        run1.add_result(TestResult::success("test1"));
        repo.insert_test_run(run1).unwrap();

        let mut run2 = make_run("1", 1);
        run2.add_result(TestResult::skip("test1"));
        repo.insert_test_run(run2).unwrap();

        let mut ui = TestUI::new();
        let cmd = DiffCommand::new(Some(temp.path().to_string_lossy().to_string()), None, None);
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        let output = ui.output.join("\n");
        assert!(output.contains("Status changed (1)"), "got: {}", output);
        assert!(output.contains("test1 (success → skip)"), "got: {}", output);
    }

    #[test]
    fn test_diff_explicit_run_ids() {
        let temp = TempDir::new().unwrap();
        let mut repo = setup_repo(&temp);

        let mut run0 = make_run("0", 0);
        run0.add_result(TestResult::success("test1"));
        repo.insert_test_run(run0).unwrap();

        let mut run1 = make_run("1", 1);
        run1.add_result(TestResult::failure("test1", "broke"));
        repo.insert_test_run(run1).unwrap();

        let mut run2 = make_run("2", 2);
        run2.add_result(TestResult::success("test1"));
        repo.insert_test_run(run2).unwrap();

        // Compare run 0 and run 1 explicitly
        let mut ui = TestUI::new();
        let cmd = DiffCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            Some("0".to_string()),
            Some("1".to_string()),
        );
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 1);
        let output = ui.output.join("\n");
        assert!(output.contains("Comparing run 0 → 1"), "got: {}", output);
        assert!(output.contains("New failures"), "got: {}", output);
    }

    #[test]
    fn test_diff_needs_two_runs() {
        let temp = TempDir::new().unwrap();
        let mut repo = setup_repo(&temp);

        let mut run = make_run("0", 0);
        run.add_result(TestResult::success("test1"));
        repo.insert_test_run(run).unwrap();

        let mut ui = TestUI::new();
        let cmd = DiffCommand::new(Some(temp.path().to_string_lossy().to_string()), None, None);
        let result = cmd.execute(&mut ui);

        assert!(result.is_err());
    }
}
