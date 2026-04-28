//! Re-run the tests of a previous run, in the same order with the same args.

use crate::commands::utils::{open_repository, resolve_run_id};
use crate::commands::Command;
use crate::error::{Error, Result};
use crate::ordering::TestOrder;
use crate::ui::UI;

/// Command to re-run the tests of a previous run.
///
/// Reads the test IDs of `run_id` in their recorded execution order and the
/// extra `--`-style args captured in the run's metadata, then dispatches to
/// [`crate::commands::RunCommand`] with `TestOrder::Discovery` so the
/// recorded order is preserved.
pub struct RerunCommand {
    base_path: Option<String>,
    run_id: Option<String>,
}

impl RerunCommand {
    /// Build a rerun command for the given run ID. If `run_id` is `None`,
    /// the latest run is used.
    pub fn new(base_path: Option<String>, run_id: Option<String>) -> Self {
        RerunCommand { base_path, run_id }
    }
}

impl Command for RerunCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let repo = open_repository(self.base_path.as_deref())?;
        let resolved_run_id = resolve_run_id(&*repo, self.run_id.as_deref())?;

        let raw = repo.get_test_run_raw(&resolved_run_id)?;
        let ordered_tests = crate::subunit_stream::parse_stream_test_order(raw)?;
        let test_args = repo.get_run_metadata(&resolved_run_id)?.test_args;
        drop(repo);

        if ordered_tests.is_empty() {
            return Err(Error::Other(format!(
                "Run {} has no recorded tests to re-run",
                resolved_run_id
            )));
        }

        ui.output(&format!(
            "Re-running {} test(s) from run {}",
            ordered_tests.len(),
            resolved_run_id
        ))?;
        if let Some(args) = &test_args {
            if !args.is_empty() {
                ui.output(&format!("With test args: {}", args.join(" ")))?;
            }
        }

        let run_cmd = crate::commands::RunCommand {
            base_path: self.base_path.clone(),
            test_ids_override: Some(ordered_tests),
            test_args,
            // Preserve the recorded order: Discovery leaves the override list
            // untouched (other strategies would re-sort it).
            test_order: Some(TestOrder::Discovery),
            ..Default::default()
        };
        run_cmd.execute(ui)
    }

    fn name(&self) -> &str {
        "rerun"
    }

    fn help(&self) -> &str {
        "Re-run exactly the tests of a previous run, in the same order with the same args"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::inquest::InquestRepositoryFactory;
    use crate::repository::{RepositoryFactory, RunId, RunMetadata, TestResult, TestRun};
    use crate::ui::test_ui::TestUI;
    use tempfile::TempDir;

    #[test]
    fn test_rerun_command_name_and_help() {
        let cmd = RerunCommand::new(None, None);
        assert_eq!(cmd.name(), "rerun");
        assert!(!cmd.help().is_empty());
    }

    #[test]
    fn test_rerun_no_recorded_tests() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let run = TestRun::new(RunId::new("0"));
        let run_id = repo.insert_test_run(run).unwrap();
        drop(repo);

        let mut ui = TestUI::new();
        let cmd = RerunCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            Some(run_id.as_str().to_string()),
        );
        let err = cmd.execute(&mut ui).unwrap_err();
        assert_eq!(err.to_string(), "Run 0 has no recorded tests to re-run");
    }

    #[test]
    fn test_rerun_run_not_found() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let _repo = factory.initialise(temp.path()).unwrap();

        let mut ui = TestUI::new();
        let cmd = RerunCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            Some("999".to_string()),
        );
        // No such run on disk: get_test_run_raw bubbles up an IO error.
        assert!(cmd.execute(&mut ui).is_err());
    }

    #[test]
    fn test_rerun_loads_order_and_args() {
        let temp = TempDir::new().unwrap();
        let factory = InquestRepositoryFactory;
        let mut repo = factory.initialise(temp.path()).unwrap();

        let mut run = TestRun::new(RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(
            TestResult::success("test_alpha").with_duration(std::time::Duration::from_millis(10)),
        );
        run.add_result(
            TestResult::success("test_beta").with_duration(std::time::Duration::from_millis(20)),
        );
        let run_id = repo.insert_test_run(run).unwrap();
        repo.set_run_metadata(
            &run_id,
            RunMetadata {
                test_args: Some(vec!["--verbose".to_string()]),
                ..RunMetadata::default()
            },
        )
        .unwrap();
        drop(repo);

        // Need a config so the inner RunCommand can find a test command.
        std::fs::write(
            temp.path().join(".testr.conf"),
            "[DEFAULT]\ntest_command=true\ntest_list_command=echo\n",
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = RerunCommand::new(
            Some(temp.path().to_string_lossy().to_string()),
            Some(run_id.as_str().to_string()),
        );
        // We don't assert on the exit code: the inner test command may not
        // produce a real subunit stream. We only check that the prelude
        // reports the correct counts and args.
        let _ = cmd.execute(&mut ui);

        let combined = ui.output.join("\n");
        assert!(
            combined.contains("Re-running 2 test(s) from run 0"),
            "got: {}",
            combined
        );
        assert!(
            combined.contains("With test args: --verbose"),
            "got: {}",
            combined
        );
    }
}
