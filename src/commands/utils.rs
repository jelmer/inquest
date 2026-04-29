//! Utility functions for command implementation

use crate::config::{
    CONFIG_FILE_NAMES, SLOW_TEST_WARNING_MIN_DURATION, SLOW_TEST_WARNING_MULTIPLIER,
};
use crate::error::Result;
use crate::repository::inquest::InquestRepositoryFactory;
#[cfg(feature = "testr")]
use crate::repository::testr::FileRepositoryFactory;
use crate::repository::{Repository, RepositoryFactory, RunId, TestRun};
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
    #[cfg(not(feature = "testr"))]
    {
        inquest_factory.open(base)
    }

    #[cfg(feature = "testr")]
    {
        if let Ok(repo) = inquest_factory.open(base) {
            return Ok(repo);
        }

        // Fall back to legacy format
        let file_factory = FileRepositoryFactory;
        file_factory.open(base)
    }
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
pub fn resolve_run_id(repo: &dyn Repository, run_id: Option<&str>) -> Result<RunId> {
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
            Ok(RunId::new(id))
        }
        None => {
            let latest = repo.get_latest_run()?;
            Ok(latest.id)
        }
    }
}

/// Extract test durations from a test run and update the repository's times database
pub fn update_test_times_from_run(repo: &mut dyn Repository, test_run: &TestRun) -> Result<()> {
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

/// Capture and store run metadata (git commit, command, concurrency, duration, exit code).
#[allow(clippy::too_many_arguments)]
pub fn store_run_metadata(
    repo: &mut dyn Repository,
    run_id: &RunId,
    command: Option<&str>,
    concurrency: Option<u32>,
    duration: Option<std::time::Duration>,
    exit_code: Option<i32>,
    test_args: Option<Vec<String>>,
    profile: Option<String>,
    predicted_duration: Option<std::time::Duration>,
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

    let git_dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                Some(!output.stdout.is_empty())
            } else {
                None
            }
        });

    let metadata = crate::repository::RunMetadata {
        git_commit,
        git_dirty,
        command: command.map(|s| s.to_string()),
        concurrency,
        duration_secs: duration.map(|d| d.as_secs_f64()),
        exit_code,
        test_args,
        profile,
        predicted_duration_secs: predicted_duration.map(|d| d.as_secs_f64()),
    };

    repo.set_run_metadata(run_id, metadata)
}

/// Update repository failing tests based on partial mode
pub fn update_repository_failing_tests(
    repo: &mut dyn Repository,
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

/// Print a one-line comparison of the run's ETA prediction against its
/// actual wall-clock duration. The raw historical-sum prediction is
/// reported alongside its calibrated form (when the calibration factor
/// differs meaningfully from `1.0`), then the actual duration and the
/// delta-vs-calibrated-prediction.
///
/// Skipped when the raw prediction was zero (no useful signal to
/// compare against).
pub fn report_eta_accuracy(
    ui: &mut dyn UI,
    predicted: std::time::Duration,
    actual: std::time::Duration,
    calibration_factor: f64,
) -> Result<()> {
    let predicted_secs = predicted.as_secs_f64();
    if predicted_secs <= 0.0 {
        return Ok(());
    }
    let calibrated = predicted.mul_f64(calibration_factor);
    let calibrated_secs = calibrated.as_secs_f64();
    let actual_secs = actual.as_secs_f64();
    // Compare against the calibrated prediction — that's what the user
    // actually saw on-screen during the run.
    let delta_secs = actual_secs - calibrated_secs;
    let pct = if calibrated_secs > 0.0 {
        (delta_secs / calibrated_secs) * 100.0
    } else {
        0.0
    };
    let direction = if delta_secs >= 0.0 {
        "slower"
    } else {
        "faster"
    };
    // Only mention calibration when it materially shifted the prediction;
    // a no-op factor is just noise.
    let calibration_note = if (calibration_factor - 1.0).abs() >= 0.05 {
        format!(
            ", calibrated {} (×{:.2})",
            format_short_duration(calibrated),
            calibration_factor
        )
    } else {
        String::new()
    };
    ui.output(&format!(
        "  ETA accuracy: predicted {}{}, actual {} ({:.0}% {})",
        format_short_duration(predicted),
        calibration_note,
        format_short_duration(actual),
        pct.abs(),
        direction,
    ))?;
    Ok(())
}

/// Format a duration as a short human-readable string (e.g., "1m 23s").
fn format_short_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{}h {:02}m", hours, mins)
    } else if secs >= 60 {
        let mins = secs / 60;
        let remaining = secs % 60;
        format!("{}m {:02}s", mins, remaining)
    } else {
        format!("{}s", secs)
    }
}

/// Display a test run summary, optionally restricting the counts to a tag
/// filter.
pub fn display_test_summary(
    ui: &mut dyn UI,
    run_id: &RunId,
    test_run: &TestRun,
    filter_tags: &[String],
) -> Result<()> {
    let (total, successes, failures) = if filter_tags.is_empty() {
        (
            test_run.total_tests(),
            test_run.count_successes(),
            test_run.count_failures(),
        )
    } else {
        (
            test_run.total_tests_filtered(filter_tags),
            test_run.count_successes_filtered(filter_tags),
            test_run.count_failures_filtered(filter_tags),
        )
    };

    ui.output(&format!("\nTest run {}:", run_id))?;
    if !filter_tags.is_empty() {
        ui.output(&format!("  Tag filter: {}", filter_tags.join(", ")))?;
    }
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

/// Warn about tests that ran significantly slower than their historical average.
pub fn warn_slow_tests(
    ui: &mut dyn UI,
    test_run: &TestRun,
    historical_times: &std::collections::HashMap<crate::repository::TestId, std::time::Duration>,
) -> Result<()> {
    if historical_times.is_empty() {
        return Ok(());
    }

    let mut slow_tests: Vec<_> = test_run
        .results
        .values()
        .filter_map(|result| {
            let actual = result.duration?;
            if actual < SLOW_TEST_WARNING_MIN_DURATION {
                return None;
            }
            let historical = historical_times.get(&result.test_id)?;
            let threshold = std::time::Duration::from_secs_f64(
                historical.as_secs_f64() * SLOW_TEST_WARNING_MULTIPLIER,
            );
            if actual > threshold {
                Some((&result.test_id, actual, *historical))
            } else {
                None
            }
        })
        .collect();

    if slow_tests.is_empty() {
        return Ok(());
    }

    slow_tests.sort_by_key(|b| std::cmp::Reverse(b.1));

    ui.output(&format!(
        "\n  {} test(s) ran significantly slower than historical average ({:.0}x threshold):",
        slow_tests.len(),
        SLOW_TEST_WARNING_MULTIPLIER,
    ))?;

    for (test_id, actual, historical) in slow_tests.iter().take(10) {
        let ratio = actual.as_secs_f64() / historical.as_secs_f64();
        ui.output(&format!(
            "    {}: {:.1}s (was {:.1}s, {:.1}x slower)",
            test_id,
            actual.as_secs_f64(),
            historical.as_secs_f64(),
            ratio,
        ))?;
    }

    Ok(())
}

/// Persist a completed test run to the repository and display summary.
///
/// Returns `(exit_code, run_id, totals)`. `run_id` is returned because
/// `output` is consumed, and `totals` is returned so the caller can pass
/// them on to [`mirror_run_to_git`] without recomputing.
pub fn persist_and_display_run(
    ui: &mut dyn UI,
    repo: &mut dyn Repository,
    output: crate::test_executor::RunOutput,
    partial: bool,
    historical_times: &std::collections::HashMap<crate::repository::TestId, std::time::Duration>,
    filter_tags: &[String],
    profile: Option<String>,
    eta_debug: bool,
) -> Result<(i32, RunId, crate::git_notes::RunTotals)> {
    let exit_code = output.exit_code();
    let crate::test_executor::RunOutput {
        run_id,
        results,
        duration,
        test_command,
        concurrency,
        test_args,
        predicted_duration,
        calibration_factor,
        ..
    } = output;

    let mut combined_run = TestRun::new(run_id.clone());
    combined_run.timestamp = chrono::Utc::now();
    for (_, result) in results {
        combined_run.add_result(result);
    }

    update_repository_failing_tests(repo, &combined_run, partial)?;
    update_test_times_from_run(repo, &combined_run)?;
    store_run_metadata(
        repo,
        &run_id,
        Some(&test_command),
        Some(concurrency),
        Some(duration),
        Some(exit_code),
        test_args,
        profile.clone(),
        predicted_duration,
    )?;

    let totals = crate::git_notes::RunTotals::from_run(&combined_run);
    display_test_summary(ui, &run_id, &combined_run, filter_tags)?;
    if eta_debug {
        if let Some(predicted) = predicted_duration {
            report_eta_accuracy(ui, predicted, duration, calibration_factor)?;
        }
    }
    warn_slow_tests(ui, &combined_run, historical_times)?;

    Ok((exit_code, run_id, totals))
}

/// Mirror a freshly-persisted run into the inquest git side ref.
///
/// Reads run metadata, captured stderr, and timestamp back from `repo` so
/// the mirror sees exactly what was just persisted. Errors are returned to
/// the caller, who is expected to log-and-swallow — mirroring failure must
/// never fail a run.
pub fn mirror_run_to_git(
    repo: &dyn Repository,
    run_id: &RunId,
    totals: crate::git_notes::RunTotals,
) -> Result<()> {
    let metadata = repo.get_run_metadata(run_id)?;
    let started_at = repo.get_run_started_at(run_id)?;
    let timestamp = started_at.map(|t| t.to_rfc3339());
    let stderr = repo.get_run_stderr(run_id)?;
    crate::git_notes::mirror_run(
        repo,
        run_id,
        &metadata,
        totals,
        timestamp.as_deref(),
        stderr.as_deref(),
        crate::git_notes::DEFAULT_MIRROR_REF,
    )
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
        let mut run = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(crate::repository::TestResult::success("test1"));
        repo.insert_test_run(run).unwrap();

        assert_eq!(resolve_run_id(&*repo, None).unwrap().as_str(), "0");
    }

    #[test]
    fn test_resolve_run_id_positive() {
        let temp = TempDir::new().unwrap();
        let repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        assert_eq!(resolve_run_id(&*repo, Some("42")).unwrap().as_str(), "42");
    }

    #[test]
    fn test_resolve_run_id_negative() {
        let temp = TempDir::new().unwrap();
        let mut repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        for i in 0..3 {
            let mut run =
                crate::repository::TestRun::new(crate::repository::RunId::new(i.to_string()));
            run.timestamp = chrono::DateTime::from_timestamp(1000000000 + i, 0).unwrap();
            run.add_result(crate::repository::TestResult::success("test1"));
            repo.insert_test_run(run).unwrap();
        }

        assert_eq!(resolve_run_id(&*repo, Some("-1")).unwrap().as_str(), "2");
        assert_eq!(resolve_run_id(&*repo, Some("-2")).unwrap().as_str(), "1");
        assert_eq!(resolve_run_id(&*repo, Some("-3")).unwrap().as_str(), "0");
    }

    #[test]
    fn test_resolve_run_id_negative_out_of_range() {
        let temp = TempDir::new().unwrap();
        let mut repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        let mut run = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(crate::repository::TestResult::success("test1"));
        repo.insert_test_run(run).unwrap();

        assert!(resolve_run_id(&*repo, Some("-2")).is_err());
    }

    #[test]
    fn test_resolve_run_id_negative_zero() {
        let temp = TempDir::new().unwrap();
        let mut repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        let mut run = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        run.add_result(crate::repository::TestResult::success("test1"));
        repo.insert_test_run(run).unwrap();

        assert!(resolve_run_id(&*repo, Some("-0")).is_err());
    }

    #[test]
    fn test_warn_slow_tests_no_history() {
        let mut ui = crate::ui::test_ui::TestUI::new();
        let run = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        let historical = std::collections::HashMap::new();
        warn_slow_tests(&mut ui, &run, &historical).unwrap();
        assert!(ui.output.is_empty());
    }

    #[test]
    fn test_warn_slow_tests_no_slow_tests() {
        let mut ui = crate::ui::test_ui::TestUI::new();
        let mut run = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        run.add_result(
            crate::repository::TestResult::success("test1")
                .with_duration(std::time::Duration::from_secs(1)),
        );
        let mut historical = std::collections::HashMap::new();
        historical.insert(
            crate::repository::TestId::new("test1"),
            std::time::Duration::from_secs(1),
        );
        warn_slow_tests(&mut ui, &run, &historical).unwrap();
        assert!(ui.output.is_empty());
    }

    #[test]
    fn test_warn_slow_tests_detects_slow_test() {
        let mut ui = crate::ui::test_ui::TestUI::new();
        let mut run = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        // Test took 10s but historical is 1s (10x slower, above 3x threshold)
        run.add_result(
            crate::repository::TestResult::success("slow_test")
                .with_duration(std::time::Duration::from_secs(10)),
        );
        let mut historical = std::collections::HashMap::new();
        historical.insert(
            crate::repository::TestId::new("slow_test"),
            std::time::Duration::from_secs(1),
        );
        warn_slow_tests(&mut ui, &run, &historical).unwrap();
        assert!(!ui.output.is_empty());
        let output = ui.output.join("\n");
        assert!(output.contains("slow_test"), "got: {}", output);
        assert!(output.contains("slower"), "got: {}", output);
    }

    #[test]
    fn test_warn_slow_tests_ignores_very_fast_tests() {
        let mut ui = crate::ui::test_ui::TestUI::new();
        let mut run = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        run.add_result(
            crate::repository::TestResult::success("fast_test")
                .with_duration(std::time::Duration::from_micros(500)),
        );
        let mut historical = std::collections::HashMap::new();
        historical.insert(
            crate::repository::TestId::new("fast_test"),
            std::time::Duration::from_micros(10),
        );
        warn_slow_tests(&mut ui, &run, &historical).unwrap();
        assert!(ui.output.is_empty());
    }

    #[test]
    fn test_report_eta_accuracy_slower() {
        let mut ui = crate::ui::test_ui::TestUI::new();
        report_eta_accuracy(
            &mut ui,
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(75),
            1.0,
        )
        .unwrap();
        assert_eq!(
            ui.output,
            vec!["  ETA accuracy: predicted 1m 00s, actual 1m 15s (25% slower)".to_string()]
        );
    }

    #[test]
    fn test_report_eta_accuracy_faster() {
        let mut ui = crate::ui::test_ui::TestUI::new();
        report_eta_accuracy(
            &mut ui,
            std::time::Duration::from_secs(100),
            std::time::Duration::from_secs(80),
            1.0,
        )
        .unwrap();
        assert_eq!(
            ui.output,
            vec!["  ETA accuracy: predicted 1m 40s, actual 1m 20s (20% faster)".to_string()]
        );
    }

    #[test]
    fn test_report_eta_accuracy_skips_zero_prediction() {
        let mut ui = crate::ui::test_ui::TestUI::new();
        report_eta_accuracy(
            &mut ui,
            std::time::Duration::ZERO,
            std::time::Duration::from_secs(5),
            1.0,
        )
        .unwrap();
        assert_eq!(ui.output, Vec::<String>::new());
    }

    #[test]
    fn test_report_eta_accuracy_with_calibration() {
        // Predicted 60s raw, calibration ×1.5 → calibrated 90s. Actual 90s.
        // Delta is computed against the calibrated prediction (what the
        // user actually saw on-screen), so this lands at 0%.
        let mut ui = crate::ui::test_ui::TestUI::new();
        report_eta_accuracy(
            &mut ui,
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(90),
            1.5,
        )
        .unwrap();
        assert_eq!(
            ui.output,
            vec![
                "  ETA accuracy: predicted 1m 00s, calibrated 1m 30s (×1.50), \
                 actual 1m 30s (0% slower)"
                    .to_string()
            ]
        );
    }

    #[test]
    fn test_report_eta_accuracy_calibration_near_one_omits_note() {
        // A factor of 1.02 isn't worth surfacing — the user would just see
        // noise. The line drops the calibration clause entirely. The
        // delta is still measured against the (calibrated) prediction the
        // user saw — 60s vs 61.2s rounds to 2% faster.
        let mut ui = crate::ui::test_ui::TestUI::new();
        report_eta_accuracy(
            &mut ui,
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
            1.02,
        )
        .unwrap();
        assert_eq!(
            ui.output,
            vec!["  ETA accuracy: predicted 1m 00s, actual 1m 00s (2% faster)".to_string()]
        );
    }

    fn build_run_output_with_prediction(
        predicted: Option<std::time::Duration>,
        actual: std::time::Duration,
    ) -> crate::test_executor::RunOutput {
        let mut results = std::collections::HashMap::new();
        results.insert(
            crate::repository::TestId::new("test1"),
            crate::repository::TestResult::success("test1").with_duration(actual),
        );
        crate::test_executor::RunOutput {
            run_id: crate::repository::RunId::new("1"),
            results,
            any_command_failed: false,
            duration: actual,
            test_command: "echo test".to_string(),
            concurrency: 1,
            test_args: None,
            predicted_duration: predicted,
            calibration_factor: 1.0,
        }
    }

    #[test]
    fn test_persist_omits_eta_accuracy_without_eta_debug() {
        let temp = TempDir::new().unwrap();
        let mut repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();
        let output = build_run_output_with_prediction(
            Some(std::time::Duration::from_secs(60)),
            std::time::Duration::from_secs(75),
        );
        let mut ui = crate::ui::test_ui::TestUI::new();
        let historical = std::collections::HashMap::new();
        persist_and_display_run(
            &mut ui,
            repo.as_mut(),
            output,
            false,
            &historical,
            &[],
            None,
            false,
        )
        .unwrap();
        // Without --eta-debug the summary stops at the pass/fail counts;
        // the ETA-accuracy line is suppressed.
        assert_eq!(
            ui.output,
            vec![
                "\nTest run 1:".to_string(),
                "  Total:   1".to_string(),
                "  Passed:  1".to_string(),
                "  Failed:  0".to_string(),
            ]
        );
    }

    #[test]
    fn test_persist_includes_eta_accuracy_with_eta_debug() {
        let temp = TempDir::new().unwrap();
        let mut repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();
        let output = build_run_output_with_prediction(
            Some(std::time::Duration::from_secs(60)),
            std::time::Duration::from_secs(75),
        );
        let mut ui = crate::ui::test_ui::TestUI::new();
        let historical = std::collections::HashMap::new();
        persist_and_display_run(
            &mut ui,
            repo.as_mut(),
            output,
            false,
            &historical,
            &[],
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            ui.output,
            vec![
                "\nTest run 1:".to_string(),
                "  Total:   1".to_string(),
                "  Passed:  1".to_string(),
                "  Failed:  0".to_string(),
                "  ETA accuracy: predicted 1m 00s, actual 1m 15s (25% slower)".to_string(),
            ]
        );
    }

    #[test]
    fn test_persist_and_display_run_success() {
        let temp = TempDir::new().unwrap();
        let mut repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        // Insert an initial run so get_latest_run works
        let mut initial_run = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        initial_run.timestamp = chrono::DateTime::from_timestamp(1000000000, 0).unwrap();
        initial_run.add_result(crate::repository::TestResult::success("test1"));
        repo.insert_test_run(initial_run).unwrap();

        let mut results = std::collections::HashMap::new();
        results.insert(
            crate::repository::TestId::new("test1"),
            crate::repository::TestResult::success("test1")
                .with_duration(std::time::Duration::from_secs(1)),
        );

        let output = crate::test_executor::RunOutput {
            run_id: crate::repository::RunId::new("1"),
            results,
            any_command_failed: false,
            duration: std::time::Duration::from_secs(2),
            test_command: "echo test".to_string(),
            concurrency: 1,
            test_args: None,
            predicted_duration: None,
            calibration_factor: 1.0,
        };

        let mut ui = crate::ui::test_ui::TestUI::new();
        let historical = std::collections::HashMap::new();
        let (exit_code, run_id, _totals) = persist_and_display_run(
            &mut ui,
            repo.as_mut(),
            output,
            false,
            &historical,
            &[],
            None,
            false,
        )
        .unwrap();

        assert_eq!(exit_code, 0);
        assert_eq!(run_id.as_str(), "1");
        let ui_text = ui.output.join("\n");
        assert!(ui_text.contains("Passed:  1"), "got: {}", ui_text);
    }

    #[test]
    fn test_display_test_summary_with_filter_tags() {
        let mut run = TestRun::new(RunId::new("7"));
        run.add_result(crate::repository::TestResult::success("test1").with_tag("worker-0"));
        run.add_result(crate::repository::TestResult::success("test2").with_tag("worker-1"));
        run.add_result(
            crate::repository::TestResult::failure("test3", "boom").with_tag("worker-0"),
        );

        let mut ui = crate::ui::test_ui::TestUI::new();
        display_test_summary(&mut ui, &RunId::new("7"), &run, &["worker-0".to_string()]).unwrap();

        assert_eq!(
            ui.output,
            vec![
                "\nTest run 7:".to_string(),
                "  Tag filter: worker-0".to_string(),
                "  Total:   2".to_string(),
                "  Passed:  1".to_string(),
                "  Failed:  1".to_string(),
            ]
        );
    }

    #[test]
    fn test_display_test_summary_with_excluded_tag() {
        let mut run = TestRun::new(RunId::new("8"));
        run.add_result(crate::repository::TestResult::success("fast1"));
        run.add_result(crate::repository::TestResult::success("slow1").with_tag("slow"));

        let mut ui = crate::ui::test_ui::TestUI::new();
        display_test_summary(&mut ui, &RunId::new("8"), &run, &["!slow".to_string()]).unwrap();

        assert_eq!(
            ui.output,
            vec![
                "\nTest run 8:".to_string(),
                "  Tag filter: !slow".to_string(),
                "  Total:   1".to_string(),
                "  Passed:  1".to_string(),
                "  Failed:  0".to_string(),
            ]
        );
    }

    #[test]
    fn test_persist_and_display_run_with_failures() {
        let temp = TempDir::new().unwrap();
        let mut repo = init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        let mut results = std::collections::HashMap::new();
        results.insert(
            crate::repository::TestId::new("test_pass"),
            crate::repository::TestResult::success("test_pass"),
        );
        results.insert(
            crate::repository::TestId::new("test_fail"),
            crate::repository::TestResult::failure("test_fail", "assertion error"),
        );

        let output = crate::test_executor::RunOutput {
            run_id: crate::repository::RunId::new("0"),
            results,
            any_command_failed: false,
            duration: std::time::Duration::from_secs(3),
            test_command: "cargo test".to_string(),
            concurrency: 1,
            test_args: None,
            predicted_duration: None,
            calibration_factor: 1.0,
        };

        let mut ui = crate::ui::test_ui::TestUI::new();
        let historical = std::collections::HashMap::new();
        let (exit_code, run_id, _totals) = persist_and_display_run(
            &mut ui,
            repo.as_mut(),
            output,
            false,
            &historical,
            &[],
            None,
            false,
        )
        .unwrap();

        assert_eq!(exit_code, 1);
        assert_eq!(run_id.as_str(), "0");
        let ui_text = ui.output.join("\n");
        assert!(ui_text.contains("Failed:  1"), "got: {}", ui_text);

        // Verify failing tests were persisted
        let failing = repo.get_failing_tests().unwrap();
        assert_eq!(failing.len(), 1);
        assert_eq!(failing[0].as_str(), "test_fail");
    }
}
