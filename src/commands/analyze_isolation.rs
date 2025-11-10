//! Analyze test isolation issues using bisection
//!
//! This command helps identify test interactions by bisecting the test suite
//! to find which tests affect the outcome of a target test.

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::error::Result;
use crate::repository::TestId;
use crate::testcommand::TestCommand;
use crate::ui::UI;
use std::path::Path;
use std::process::{Command as ProcessCommand, Stdio};

/// Command to analyze test isolation issues using bisection.
///
/// This command helps identify test interactions by bisecting the test suite
/// to find which tests cause a target test to fail when run together but
/// pass when run in isolation.
pub struct AnalyzeIsolationCommand {
    base_path: Option<String>,
    target_test: String,
}

impl AnalyzeIsolationCommand {
    /// Creates a new analyze-isolation command.
    ///
    /// # Arguments
    /// * `base_path` - Optional base directory path for the repository
    /// * `target_test` - The test ID to analyze for isolation issues
    pub fn new(base_path: Option<String>, target_test: String) -> Self {
        AnalyzeIsolationCommand {
            base_path,
            target_test,
        }
    }

    /// Run a specific set of tests and return whether the target test failed
    fn run_tests_and_check_failure(
        &self,
        test_cmd: &TestCommand,
        tests: &[TestId],
        base: &Path,
    ) -> Result<bool> {
        let (cmd_str, _temp_file) = test_cmd.build_command(Some(tests), false)?;

        let output = ProcessCommand::new("sh")
            .arg("-c")
            .arg(&cmd_str)
            .current_dir(base)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| {
                crate::error::Error::CommandExecution(format!("Failed to run tests: {}", e))
            })?;

        // Parse results to check if target test failed
        let test_run =
            crate::subunit_stream::parse_stream(output.stdout.as_slice(), "analyze".to_string())?;

        // Check if the target test failed
        let target_id = TestId::new(&self.target_test);
        if let Some(result) = test_run.results.get(&target_id) {
            Ok(result.status.is_failure())
        } else {
            // Target test wasn't run or wasn't found
            Ok(false)
        }
    }

    /// Bisect to find the minimal set of tests that cause failure
    fn bisect(
        &self,
        ui: &mut dyn UI,
        test_cmd: &TestCommand,
        base: &Path,
        candidates: Vec<TestId>,
    ) -> Result<Vec<TestId>> {
        if candidates.is_empty() {
            return Ok(vec![]);
        }

        ui.output(&format!(
            "Bisecting {} candidate tests...",
            candidates.len()
        ))?;

        // Binary search to find minimal reproducer
        let mut left = 0;
        let mut right = candidates.len();
        let mut minimal_set = candidates.clone();

        while left < right {
            let mid = (left + right) / 2;
            let test_subset: Vec<_> = candidates[..=mid]
                .iter()
                .chain(std::iter::once(&TestId::new(&self.target_test)))
                .cloned()
                .collect();

            ui.output(&format!(
                "  Testing with {} tests (indices 0..={})",
                test_subset.len(),
                mid
            ))?;

            if self.run_tests_and_check_failure(test_cmd, &test_subset, base)? {
                // Failure reproduced with smaller set
                right = mid;
                minimal_set = candidates[..=mid].to_vec();
            } else {
                // Need more tests to reproduce
                left = mid + 1;
            }
        }

        // Try to minimize further by removing tests one by one
        let mut final_set = minimal_set.clone();
        for i in (0..minimal_set.len()).rev() {
            let test_subset: Vec<_> = minimal_set[..i]
                .iter()
                .chain(minimal_set[i + 1..].iter())
                .chain(std::iter::once(&TestId::new(&self.target_test)))
                .cloned()
                .collect();

            if self.run_tests_and_check_failure(test_cmd, &test_subset, base)? {
                // Still reproduces without this test
                final_set.remove(i);
                ui.output(&format!("  Removed unnecessary test at index {}", i))?;
            }
        }

        Ok(final_set)
    }
}

impl Command for AnalyzeIsolationCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = Path::new(self.base_path.as_deref().unwrap_or("."));

        // Open repository (to verify it exists)
        let _repo = open_repository(self.base_path.as_deref())?;

        // Load test command
        let test_cmd = TestCommand::from_directory(base)?;

        ui.output(&format!(
            "Analyzing test isolation for: {}",
            self.target_test
        ))?;

        // Step 1: Run target test in isolation
        ui.output("\nStep 1: Running target test in isolation...")?;
        let target_id = TestId::new(&self.target_test);
        let isolated_failed =
            self.run_tests_and_check_failure(&test_cmd, std::slice::from_ref(&target_id), base)?;

        if isolated_failed {
            ui.output("  Result: FAILED when run in isolation")?;
            ui.output("\nThe test fails even when run alone. This is not an isolation issue.")?;
            return Ok(1);
        }

        ui.output("  Result: PASSED when run in isolation")?;

        // Step 2: Get all tests
        ui.output("\nStep 2: Getting list of all tests...")?;
        let all_tests = test_cmd.list_tests()?;
        ui.output(&format!("  Found {} total tests", all_tests.len()))?;

        // Step 3: Run all tests together
        ui.output("\nStep 3: Running all tests together...")?;
        let all_failed = self.run_tests_and_check_failure(&test_cmd, &all_tests, base)?;

        if !all_failed {
            ui.output("  Result: PASSED when run with all tests")?;
            ui.output("\nThe test passes with all tests. No isolation issue found.")?;
            return Ok(0);
        }

        ui.output("  Result: FAILED when run with all tests")?;
        ui.output("\nIsolation issue confirmed! Beginning bisection...")?;

        // Step 4: Bisect to find culprit tests
        let candidates: Vec<_> = all_tests
            .into_iter()
            .filter(|t| t.as_str() != self.target_test)
            .collect();

        let culprits = self.bisect(ui, &test_cmd, base, candidates)?;

        // Step 5: Report results
        ui.output("\n=== Analysis Complete ===")?;
        if culprits.is_empty() {
            ui.output("No specific tests found that cause the failure.")?;
            ui.output("The issue may be environmental or timing-related.")?;
        } else {
            ui.output(&format!(
                "\nFound {} test(s) that interact with {}:",
                culprits.len(),
                self.target_test
            ))?;
            for test in &culprits {
                ui.output(&format!("  - {}", test))?;
            }

            ui.output("\nTo reproduce the failure, run:")?;
            ui.output(&format!(
                "  testr run --load-list <(echo '{}'; echo '{}')",
                culprits
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join("'; echo '"),
                self.target_test
            ))?;
        }

        Ok(0)
    }

    fn name(&self) -> &str {
        "analyze-isolation"
    }

    fn help(&self) -> &str {
        "Analyze test isolation issues using bisection"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_analyze_isolation_command_name() {
        let cmd = AnalyzeIsolationCommand::new(None, "test_example".to_string());
        assert_eq!(cmd.name(), "analyze-isolation");
    }
}
