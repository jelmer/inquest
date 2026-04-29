//! `GET /api/tests` and `GET /api/tree` — surface the repository's known
//! tests and group them by the configured (or user-supplied) regex.

use super::{json_error, AppState};
use crate::error::Result;
use crate::repository::{TestId, TestStatus};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Serialize)]
pub(super) struct TestStats {
    test_id: String,
    runs: u32,
    failures: u32,
    last_status: Option<String>,
    last_run_id: Option<String>,
    runs_since_seen: u32,
    avg_duration_secs: Option<f64>,
    discovered: bool,
}

#[derive(Serialize)]
pub(super) struct TestsResponse {
    tests: Vec<TestStats>,
    group_regex: Option<String>,
    total_runs_in_repo: usize,
    /// True when the response includes the project's discovered-test list.
    /// When false the `discovered` flag on each test reflects only repo
    /// history (so it's effectively meaningless and should be ignored by UI).
    discovery_run: bool,
}

#[derive(Deserialize)]
pub(super) struct TestsQuery {
    /// When true, also invoke the project's test-list command to surface
    /// tests that have never been recorded. Off by default because for
    /// e.g. Cargo projects this triggers a (potentially slow) build.
    #[serde(default)]
    include_discovered: bool,
}

pub(super) async fn api_tests(
    State(state): State<AppState>,
    Query(q): Query<TestsQuery>,
) -> Response {
    match collect_test_stats(&state.base, q.include_discovered) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

fn collect_test_stats(base: &std::path::Path, include_discovered: bool) -> Result<TestsResponse> {
    let repo = crate::commands::utils::open_repository(Some(&base.to_string_lossy()))?;
    let run_ids = repo.list_run_ids()?;
    let total_runs_in_repo = run_ids.len();

    // Aggregate per-test stats over the full run history. We walk runs in
    // chronological order so `last_status`/`last_run_id`/`runs_since_seen`
    // reflect the most recent recorded outcome for each test.
    struct Agg {
        runs: u32,
        failures: u32,
        last_status: Option<TestStatus>,
        last_run_index: Option<usize>,
        duration_total: Duration,
        duration_count: u32,
    }
    let mut agg: HashMap<TestId, Agg> = HashMap::new();
    for (idx, rid) in run_ids.iter().enumerate() {
        let run = match repo.get_test_run(rid) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for (tid, result) in &run.results {
            let entry = agg.entry(tid.clone()).or_insert(Agg {
                runs: 0,
                failures: 0,
                last_status: None,
                last_run_index: None,
                duration_total: Duration::ZERO,
                duration_count: 0,
            });
            entry.runs += 1;
            if result.status.is_failure() {
                entry.failures += 1;
            }
            entry.last_status = Some(result.status);
            entry.last_run_index = Some(idx);
            if let Some(d) = result.duration {
                entry.duration_total += d;
                entry.duration_count += 1;
            }
        }
    }

    // Pull the discovered list from the test command only when the caller
    // asked for it. For Cargo projects this triggers `cargo build --tests`,
    // which can take seconds-to-minutes — we don't want that on every page
    // load, so the UI gates this behind an explicit "Discover tests" action.
    let discovered: std::collections::HashSet<TestId> = if include_discovered {
        match crate::testcommand::TestCommand::from_directory(base) {
            Ok(tc) => tc
                .list_tests()
                .map(|ts| ts.into_iter().collect())
                .unwrap_or_default(),
            Err(_) => Default::default(),
        }
    } else {
        Default::default()
    };

    let mut all_ids: std::collections::BTreeSet<TestId> = agg.keys().cloned().collect();
    for d in &discovered {
        all_ids.insert(d.clone());
    }

    let last_run_idx = run_ids.len().saturating_sub(1);
    let mut tests = Vec::with_capacity(all_ids.len());
    for tid in all_ids {
        let entry = agg.get(&tid);
        let avg = entry.and_then(|e| {
            if e.duration_count == 0 {
                None
            } else {
                Some(e.duration_total.as_secs_f64() / e.duration_count as f64)
            }
        });
        let runs_since_seen = match entry.and_then(|e| e.last_run_index) {
            Some(idx) => (last_run_idx - idx) as u32,
            None => total_runs_in_repo as u32,
        };
        tests.push(TestStats {
            test_id: tid.as_str().to_string(),
            runs: entry.map_or(0, |e| e.runs),
            failures: entry.map_or(0, |e| e.failures),
            last_status: entry.and_then(|e| e.last_status).map(|s| s.to_string()),
            last_run_id: entry
                .and_then(|e| e.last_run_index)
                .and_then(|i| run_ids.get(i))
                .map(|r| r.as_str().to_string()),
            runs_since_seen,
            avg_duration_secs: avg,
            discovered: discovered.contains(&tid),
        });
    }

    // Surface the configured group_regex so the UI can default to it.
    let group_regex = crate::config::TestrConfig::find_in_directory(base)
        .ok()
        .and_then(|(cfg, _)| cfg.group_regex);

    Ok(TestsResponse {
        tests,
        group_regex,
        total_runs_in_repo,
        discovery_run: include_discovered,
    })
}

#[derive(Deserialize)]
pub(super) struct TreeQuery {
    regex: Option<String>,
    #[serde(default)]
    include_discovered: bool,
}

#[derive(Serialize)]
pub(super) struct TreeResponse {
    regex_used: Option<String>,
    groups: Vec<TreeGroup>,
}

#[derive(Serialize)]
pub(super) struct TreeGroup {
    name: String,
    tests: Vec<String>,
}

pub(super) async fn api_tree(
    State(state): State<AppState>,
    Query(q): Query<TreeQuery>,
) -> Response {
    let regex = match q.regex.as_deref().filter(|s| !s.is_empty()) {
        Some(r) => Some(r.to_string()),
        None => crate::config::TestrConfig::find_in_directory(&state.base)
            .ok()
            .and_then(|(c, _)| c.group_regex),
    };

    let repo = match crate::commands::utils::open_repository(Some(&state.base.to_string_lossy())) {
        Ok(r) => r,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    // Collect every test we have ever seen, plus discovered tests. Without
    // this union, freshly-discovered tests that have not yet been recorded
    // wouldn't show up in the tree.
    let mut tests: std::collections::BTreeSet<TestId> = std::collections::BTreeSet::new();
    if let Ok(run_ids) = repo.list_run_ids() {
        for rid in run_ids {
            if let Ok(run) = repo.get_test_run(&rid) {
                tests.extend(run.results.into_keys());
            }
        }
    }
    if q.include_discovered {
        if let Ok(tc) = crate::testcommand::TestCommand::from_directory(&state.base) {
            if let Ok(ds) = tc.list_tests() {
                tests.extend(ds);
            }
        }
    }
    let tests: Vec<TestId> = tests.into_iter().collect();

    let groups: Vec<TreeGroup> = match &regex {
        Some(r) => match crate::grouping::group_tests(&tests, r) {
            Ok(map) => {
                let mut out: Vec<TreeGroup> = map
                    .into_iter()
                    .map(|(name, tids)| {
                        let mut t: Vec<String> =
                            tids.into_iter().map(|t| t.as_str().to_string()).collect();
                        t.sort();
                        TreeGroup { name, tests: t }
                    })
                    .collect();
                out.sort_by(|a, b| a.name.cmp(&b.name));
                out
            }
            Err(e) => {
                return json_error(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid group regex: {}", e),
                );
            }
        },
        None => {
            // No regex: a single flat group containing every test.
            let mut t: Vec<String> = tests.iter().map(|t| t.as_str().to_string()).collect();
            t.sort();
            vec![TreeGroup {
                name: String::new(),
                tests: t,
            }]
        }
    };

    Json(TreeResponse {
        regex_used: regex,
        groups,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn collect_test_stats_empty_repo_errors() {
        let temp = TempDir::new().unwrap();
        let result = collect_test_stats(temp.path(), false);
        assert!(result.is_err());
    }

    #[test]
    fn collect_test_stats_with_runs() {
        let temp = TempDir::new().unwrap();
        let mut repo =
            crate::commands::utils::init_repository(Some(&temp.path().to_string_lossy())).unwrap();

        let mut run = crate::repository::TestRun::new(crate::repository::RunId::new("0"));
        run.add_result(
            crate::repository::TestResult::success("a.b.test_one")
                .with_duration(Duration::from_millis(10)),
        );
        run.add_result(crate::repository::TestResult::failure(
            "a.b.test_two",
            "boom",
        ));
        repo.insert_test_run(run).unwrap();
        drop(repo);

        let resp = collect_test_stats(temp.path(), false).unwrap();
        assert_eq!(resp.total_runs_in_repo, 1);
        assert_eq!(resp.tests.len(), 2);

        let one = resp
            .tests
            .iter()
            .find(|t| t.test_id == "a.b.test_one")
            .unwrap();
        assert_eq!(one.runs, 1);
        assert_eq!(one.failures, 0);
        assert_eq!(one.last_status.as_deref(), Some("success"));

        let two = resp
            .tests
            .iter()
            .find(|t| t.test_id == "a.b.test_two")
            .unwrap();
        assert_eq!(two.runs, 1);
        assert_eq!(two.failures, 1);
        assert_eq!(two.last_status.as_deref(), Some("failure"));
    }
}
