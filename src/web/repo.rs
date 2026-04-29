//! `GET /api/repo` — repository-wide overview: totals, recent failure rate,
//! slowest tests, and flakiest tests.

use super::{json_error, AppState};
use crate::error::Result;
use crate::repository::TestId;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Serialize)]
pub(super) struct RepoOverview {
    total_runs: usize,
    total_tests_known: usize,
    last_run_id: Option<String>,
    last_run_failures: Option<usize>,
    last_run_total: Option<usize>,
    avg_run_duration_secs: Option<f64>,
    recent_failure_rate: Option<f64>,
    slowest: Vec<TestStatLite>,
    flakiest: Vec<TestStatLite>,
}

#[derive(Serialize)]
pub(super) struct TestStatLite {
    test_id: String,
    runs: u32,
    failures: u32,
    avg_duration_secs: Option<f64>,
    flakiness_score: Option<f64>,
}

pub(super) async fn api_repo(State(state): State<AppState>) -> Response {
    match build_repo_overview(&state.base) {
        Ok(v) => Json(v).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

fn build_repo_overview(base: &std::path::Path) -> Result<RepoOverview> {
    let repo = crate::commands::utils::open_repository(Some(&base.to_string_lossy()))?;
    let run_ids = repo.list_run_ids()?;
    let total_runs = run_ids.len();

    // Walk runs once, capturing the per-test aggregates we need for "slowest"
    // and the per-run summary for the most-recent and average-duration data.
    struct Agg {
        runs: u32,
        failures: u32,
        duration_total: Duration,
        duration_count: u32,
    }
    let mut agg: HashMap<TestId, Agg> = HashMap::new();
    let mut total_duration_secs: f64 = 0.0;
    let mut runs_with_duration: u32 = 0;
    let mut last_run_total: Option<usize> = None;
    let mut last_run_failures: Option<usize> = None;
    let last_run_id = run_ids.last().map(|r| r.as_str().to_string());

    let recent_window = run_ids.len().min(10);
    let mut recent_total: usize = 0;
    let mut recent_failures: usize = 0;

    for (idx, rid) in run_ids.iter().enumerate() {
        let run = match repo.get_test_run(rid) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let meta = repo.get_run_metadata(rid).unwrap_or_default();
        if let Some(d) = meta.duration_secs {
            total_duration_secs += d;
            runs_with_duration += 1;
        }
        if idx + 1 == run_ids.len() {
            last_run_total = Some(run.total_tests());
            last_run_failures = Some(run.count_failures());
        }
        if idx + recent_window >= run_ids.len() {
            recent_total += run.total_tests();
            recent_failures += run.count_failures();
        }
        for (tid, result) in &run.results {
            let entry = agg.entry(tid.clone()).or_insert(Agg {
                runs: 0,
                failures: 0,
                duration_total: Duration::ZERO,
                duration_count: 0,
            });
            entry.runs += 1;
            if result.status.is_failure() {
                entry.failures += 1;
            }
            if let Some(d) = result.duration {
                entry.duration_total += d;
                entry.duration_count += 1;
            }
        }
    }

    // Top 10 slowest by mean duration. Require at least one timed run so we
    // don't surface tests that never reported a duration.
    let mut slowest: Vec<TestStatLite> = agg
        .iter()
        .filter(|(_, a)| a.duration_count > 0)
        .map(|(tid, a)| {
            let avg = a.duration_total.as_secs_f64() / a.duration_count as f64;
            TestStatLite {
                test_id: tid.as_str().to_string(),
                runs: a.runs,
                failures: a.failures,
                avg_duration_secs: Some(avg),
                flakiness_score: None,
            }
        })
        .collect();
    slowest.sort_by(|a, b| {
        b.avg_duration_secs
            .partial_cmp(&a.avg_duration_secs)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    slowest.truncate(10);

    // Top 10 flakiest. Reuse the repository-provided definition so the metric
    // matches `inq flaky`.
    let flakiest: Vec<TestStatLite> = repo
        .get_flakiness(2)
        .unwrap_or_default()
        .into_iter()
        .take(10)
        .map(|f| {
            let avg = agg.get(&f.test_id).and_then(|a| {
                if a.duration_count == 0 {
                    None
                } else {
                    Some(a.duration_total.as_secs_f64() / a.duration_count as f64)
                }
            });
            TestStatLite {
                test_id: f.test_id.as_str().to_string(),
                runs: f.runs,
                failures: f.failures,
                avg_duration_secs: avg,
                flakiness_score: Some(f.flakiness_score),
            }
        })
        .collect();

    let avg_run_duration_secs = if runs_with_duration > 0 {
        Some(total_duration_secs / runs_with_duration as f64)
    } else {
        None
    };
    let recent_failure_rate = if recent_total > 0 {
        Some(recent_failures as f64 / recent_total as f64)
    } else {
        None
    };

    Ok(RepoOverview {
        total_runs,
        total_tests_known: agg.len(),
        last_run_id,
        last_run_failures,
        last_run_total,
        avg_run_duration_secs,
        recent_failure_rate,
        slowest,
        flakiest,
    })
}

/// Per-run summary used by the timeline view.
#[derive(Serialize)]
pub(super) struct TimelineRun {
    id: String,
    timestamp: String,
    failures: usize,
    total: usize,
}

/// One row in the timeline grid: a test plus its status across each run in
/// the window. The `statuses` array is parallel to the response's `runs`
/// array (same length, same order). `null` means the test wasn't observed
/// in that run.
#[derive(Serialize)]
pub(super) struct TimelineRow {
    test_id: String,
    statuses: Vec<Option<String>>,
}

#[derive(Serialize)]
pub(super) struct TimelineResponse {
    runs: Vec<TimelineRun>,
    rows: Vec<TimelineRow>,
}

#[derive(serde::Deserialize)]
pub(super) struct TimelineQuery {
    /// How many of the most recent runs to include. Defaults to 50; the
    /// timeline gets unwieldy past that.
    #[serde(default = "default_timeline_limit")]
    limit: usize,
}

fn default_timeline_limit() -> usize {
    50
}

pub(super) async fn api_timeline(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<TimelineQuery>,
) -> Response {
    match build_timeline(&state.base, q.limit) {
        Ok(v) => Json(v).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

fn build_timeline(base: &std::path::Path, limit: usize) -> Result<TimelineResponse> {
    let repo = crate::commands::utils::open_repository(Some(&base.to_string_lossy()))?;
    let all_run_ids = repo.list_run_ids()?;

    // Take the most-recent N runs, then reverse so the response array goes
    // oldest → newest (left-to-right in the timeline view).
    let window: Vec<_> = all_run_ids
        .iter()
        .rev()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    // Walk the window once. For each run, record (test_id → status) and the
    // run summary. Tests never failing in the window are filtered out at
    // the end — they'd be a wall of green that adds no signal.
    let mut runs_meta: Vec<TimelineRun> = Vec::with_capacity(window.len());
    // Map test_id → vec of statuses indexed by position in the window.
    let mut per_test: HashMap<TestId, Vec<Option<String>>> = HashMap::new();
    let n = window.len();
    for (idx, rid) in window.iter().enumerate() {
        let run = match repo.get_test_run(rid) {
            Ok(r) => r,
            Err(_) => {
                runs_meta.push(TimelineRun {
                    id: rid.as_str().to_string(),
                    timestamp: String::new(),
                    failures: 0,
                    total: 0,
                });
                continue;
            }
        };
        runs_meta.push(TimelineRun {
            id: rid.as_str().to_string(),
            timestamp: run.timestamp.to_rfc3339(),
            failures: run.count_failures(),
            total: run.total_tests(),
        });
        for (tid, result) in &run.results {
            let row = per_test.entry(tid.clone()).or_insert_with(|| vec![None; n]);
            row[idx] = Some(result.status.to_string());
        }
    }

    // Filter to tests that ever failed in the window. A pure-pass row tells
    // the user nothing new and burns vertical space.
    let mut rows: Vec<TimelineRow> = per_test
        .into_iter()
        .filter(|(_, statuses)| {
            statuses.iter().any(|s| {
                matches!(
                    s.as_deref(),
                    Some("failure") | Some("error") | Some("uxsuccess")
                )
            })
        })
        .map(|(test_id, statuses)| TimelineRow {
            test_id: test_id.as_str().to_string(),
            statuses,
        })
        .collect();
    // Sort: tests that fail in the most recent run first, then by total
    // failure count descending, then alphabetically. Puts active fires at
    // the top.
    rows.sort_by(|a, b| {
        let last_a = a.statuses.last().and_then(|s| s.as_deref()).unwrap_or("");
        let last_b = b.statuses.last().and_then(|s| s.as_deref()).unwrap_or("");
        let active_a = matches!(last_a, "failure" | "error" | "uxsuccess");
        let active_b = matches!(last_b, "failure" | "error" | "uxsuccess");
        if active_a != active_b {
            return active_b.cmp(&active_a);
        }
        let count_a = a
            .statuses
            .iter()
            .filter(|s| {
                matches!(
                    s.as_deref(),
                    Some("failure") | Some("error") | Some("uxsuccess")
                )
            })
            .count();
        let count_b = b
            .statuses
            .iter()
            .filter(|s| {
                matches!(
                    s.as_deref(),
                    Some("failure") | Some("error") | Some("uxsuccess")
                )
            })
            .count();
        count_b
            .cmp(&count_a)
            .then_with(|| a.test_id.cmp(&b.test_id))
    });

    Ok(TimelineResponse {
        runs: runs_meta,
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn build_repo_overview_aggregates_runs() {
        let temp = TempDir::new().unwrap();
        let mut repo =
            crate::commands::utils::init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        // Two runs: the first with both tests passing, the second with one
        // flapping to failure. Gives us a non-zero failure rate, a flakiest
        // entry (1 transition), and a slowest entry.
        let mut run1 = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        run1.add_result(
            crate::repository::TestResult::success("a.b.fast")
                .with_duration(Duration::from_millis(5)),
        );
        run1.add_result(
            crate::repository::TestResult::success("a.b.slow")
                .with_duration(Duration::from_millis(500)),
        );
        repo.insert_test_run(run1).unwrap();

        let mut run2 = crate::repository::TestRun::new(crate::repository::RunId::new("1"));
        run2.add_result(
            crate::repository::TestResult::success("a.b.fast")
                .with_duration(Duration::from_millis(6)),
        );
        run2.add_result(crate::repository::TestResult::failure("a.b.slow", "boom"));
        repo.insert_test_run(run2).unwrap();
        drop(repo);

        let overview = build_repo_overview(temp.path()).unwrap();
        assert_eq!(overview.total_runs, 2);
        assert_eq!(overview.total_tests_known, 2);
        assert_eq!(overview.last_run_total, Some(2));
        assert_eq!(overview.last_run_failures, Some(1));
        // recent_failure_rate is failures-over-test-results across the
        // recent window; with one failure out of four results that's 0.25.
        assert_eq!(overview.recent_failure_rate, Some(0.25));
        // a.b.slow has the higher mean duration so it should head the list.
        assert_eq!(overview.slowest.first().unwrap().test_id, "a.b.slow");
        // a.b.slow flapped (success → failure) so it shows as flakiest.
        assert_eq!(overview.flakiest.first().unwrap().test_id, "a.b.slow");
    }

    #[test]
    fn build_timeline_filters_to_ever_failing_tests() {
        let temp = TempDir::new().unwrap();
        let mut repo =
            crate::commands::utils::init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        // Run 0: both pass. Run 1: a.flaky fails, a.steady passes.
        let mut run1 = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        run1.add_result(crate::repository::TestResult::success("a.flaky"));
        run1.add_result(crate::repository::TestResult::success("a.steady"));
        repo.insert_test_run(run1).unwrap();
        let mut run2 = crate::repository::TestRun::new(crate::repository::RunId::new("1"));
        run2.add_result(crate::repository::TestResult::failure("a.flaky", "boom"));
        run2.add_result(crate::repository::TestResult::success("a.steady"));
        repo.insert_test_run(run2).unwrap();
        drop(repo);

        let timeline = build_timeline(temp.path(), 50).unwrap();
        // Two runs in the window, oldest-first.
        assert_eq!(timeline.runs.len(), 2);
        assert_eq!(timeline.runs[0].id, "0");
        assert_eq!(timeline.runs[1].id, "1");
        // Only a.flaky surfaces — a.steady never failed so it's filtered.
        assert_eq!(timeline.rows.len(), 1);
        assert_eq!(timeline.rows[0].test_id, "a.flaky");
        // Statuses parallel runs[]: oldest = success, newest = failure.
        assert_eq!(timeline.rows[0].statuses[0].as_deref(), Some("success"));
        assert_eq!(timeline.rows[0].statuses[1].as_deref(), Some("failure"));
    }
}
