//! Show tests with the highest flakiness across recorded runs

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::error::Result;
use crate::ui::UI;

/// Command to show the flakiest tests across run history.
///
/// Flakiness here is measured by pass↔fail transitions in consecutive runs
/// in which the test was recorded — chronically broken tests rank low,
/// genuinely flapping tests rank high.
pub struct FlakyCommand {
    base_path: Option<String>,
    count: usize,
    min_runs: usize,
}

impl FlakyCommand {
    /// Creates a flaky command with custom display count and minimum-runs filter.
    pub fn new(base_path: Option<String>, count: usize, min_runs: usize) -> Self {
        FlakyCommand {
            base_path,
            count,
            min_runs,
        }
    }
}

impl Command for FlakyCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let repo = open_repository(self.base_path.as_deref())?;
        let stats = repo.get_flakiness(self.min_runs)?;

        if stats.is_empty() {
            ui.output(&format!(
                "No flaky tests found (need at least {} recorded runs per test)",
                self.min_runs
            ))?;
            return Ok(0);
        }

        let display_count = self.count.min(stats.len());
        ui.output(&format!(
            "Flakiest {} test(s) (min {} runs, {} candidate(s)):",
            display_count,
            self.min_runs,
            stats.len()
        ))?;
        ui.output("  flake%  fail%  transitions  runs  test")?;
        for entry in stats.iter().take(display_count) {
            ui.output(&format!(
                "  {:5.1}%  {:5.1}%  {:>11}  {:>4}  {}",
                entry.flakiness_score * 100.0,
                entry.failure_rate * 100.0,
                entry.transitions,
                entry.runs,
                entry.test_id,
            ))?;
        }

        Ok(0)
    }

    fn name(&self) -> &str {
        "flaky"
    }

    fn help(&self) -> &str {
        "Show tests that flip between pass and fail across recorded runs"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, RunId, TestId, TestResult, TestRun, TestStatus};
    use crate::ui::test_ui::TestUI;
    use tempfile::TempDir;

    /// Insert a sequence of runs whose results follow a pre-defined pattern,
    /// so each test case can describe what happened succinctly.
    fn build_history(
        repo: &mut dyn crate::repository::Repository,
        per_test_statuses: &[(&str, &[TestStatus])],
    ) {
        // Find how many runs we need (length of the longest pattern).
        let n_runs = per_test_statuses
            .iter()
            .map(|(_, s)| s.len())
            .max()
            .unwrap_or(0);
        for run_idx in 0..n_runs {
            let mut run = TestRun::new(RunId::new(run_idx.to_string()));
            run.timestamp =
                chrono::DateTime::from_timestamp(1_700_000_000 + run_idx as i64, 0).unwrap();
            for (test_id, statuses) in per_test_statuses {
                if let Some(status) = statuses.get(run_idx) {
                    let result = TestResult {
                        test_id: TestId::new(*test_id),
                        status: *status,
                        duration: None,
                        message: None,
                        details: None,
                        tags: vec![],
                    };
                    run.add_result(result);
                }
            }
            repo.insert_test_run(run).unwrap();
        }
    }

    #[test]
    fn empty_repository_reports_no_flaky() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        factory.initialise(temp.path()).unwrap();

        let mut ui = TestUI::new();
        let cmd = FlakyCommand::new(Some(temp.path().to_string_lossy().to_string()), 10, 5);
        assert_eq!(cmd.execute(&mut ui).unwrap(), 0);
        assert_eq!(ui.output.len(), 1);
        assert!(ui.output[0].contains("No flaky tests found"));
    }

    #[test]
    fn ranks_flapping_above_chronically_broken() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::{Failure, Success};
        // flapping_test: P F P F P F  → 5 transitions, 50% fail rate
        // broken_test:   F F F F F F  → 0 transitions, 100% fail rate
        // stable_test:   P P P P P P  → 0 transitions, 0% fail rate
        build_history(
            repo.as_mut(),
            &[
                (
                    "flapping_test",
                    &[Success, Failure, Success, Failure, Success, Failure],
                ),
                (
                    "broken_test",
                    &[Failure, Failure, Failure, Failure, Failure, Failure],
                ),
                (
                    "stable_test",
                    &[Success, Success, Success, Success, Success, Success],
                ),
            ],
        );

        let stats = repo.get_flakiness(5).unwrap();
        // stable_test never failed, so it must not appear in the flakiness report.
        assert_eq!(stats.len(), 2);
        assert!(!stats.iter().any(|s| s.test_id.as_str() == "stable_test"));
        assert_eq!(stats[0].test_id.as_str(), "flapping_test");
        assert_eq!(stats[0].transitions, 5);
        assert_eq!(stats[0].runs, 6);
        assert_eq!(stats[0].failures, 3);
        // Broken test should rank below flapping (more failures, no transitions).
        let broken = stats
            .iter()
            .find(|s| s.test_id.as_str() == "broken_test")
            .unwrap();
        assert_eq!(broken.transitions, 0);
        assert_eq!(broken.failures, 6);
    }

    #[test]
    fn never_failed_tests_are_excluded() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::Success;
        build_history(
            repo.as_mut(),
            &[(
                "always_passes",
                &[Success, Success, Success, Success, Success],
            )],
        );

        let stats = repo.get_flakiness(2).unwrap();
        assert!(stats.is_empty());
    }

    #[test]
    fn min_runs_filters_short_history() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::{Failure, Success};
        // short_test only ran twice; skip when min_runs > 2.
        build_history(
            repo.as_mut(),
            &[
                ("short_test", &[Success, Failure]),
                ("long_test", &[Success, Failure, Success, Failure, Success]),
            ],
        );

        let stats = repo.get_flakiness(5).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].test_id.as_str(), "long_test");

        // Lower the threshold and short_test should appear.
        let stats = repo.get_flakiness(2).unwrap();
        assert_eq!(stats.len(), 2);
    }

    #[test]
    fn flakiness_score_is_normalised() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::{Failure, Success};
        // Maximum possible flakiness: every consecutive pair flips.
        build_history(
            repo.as_mut(),
            &[("max_flaky", &[Success, Failure, Success, Failure, Success])],
        );

        let stats = repo.get_flakiness(5).unwrap();
        assert_eq!(stats.len(), 1);
        assert!((stats[0].flakiness_score - 1.0).abs() < 1e-9);
        assert!((stats[0].failure_rate - 0.4).abs() < 1e-9);
    }

    #[test]
    fn cli_outputs_table_header_and_rows() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        use TestStatus::{Failure, Success};
        build_history(
            repo.as_mut(),
            &[("flap", &[Success, Failure, Success, Failure, Success])],
        );

        let mut ui = TestUI::new();
        let cmd = FlakyCommand::new(Some(temp.path().to_string_lossy().to_string()), 10, 5);
        assert_eq!(cmd.execute(&mut ui).unwrap(), 0);
        assert!(ui.output[0].contains("Flakiest"));
        assert!(ui.output[1].contains("flake%"));
        assert!(ui.output[2].contains("flap"));
    }
}
