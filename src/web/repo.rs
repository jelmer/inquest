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
}
