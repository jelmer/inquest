//! ETA computation for the test-runner progress bar.
//!
//! The display has two halves: an [`EtaModel`] that resolves a per-test
//! historical duration (with a mean fallback for tests that have no recorded
//! history), and an [`EtaState`] that aggregates progress across one or more
//! workers and renders the formatted ETA string for indicatif's `{eta_hist}`
//! template key.

use crate::repository::TestId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Resolves a per-test estimated duration, falling back to the mean of known
/// durations for tests that have no recorded history. This keeps
/// `estimated_total` and `completed_duration` in consistent units, so the ETA
/// math doesn't get skewed by tests we've never run before.
#[derive(Clone)]
pub struct EtaModel {
    durations: Arc<HashMap<TestId, Duration>>,
    mean_known: Option<Duration>,
}

impl EtaModel {
    /// Build a model from historical times restricted to the tests in this run.
    pub fn new(durations: Arc<HashMap<TestId, Duration>>) -> Self {
        let mean_known = if durations.is_empty() {
            None
        } else {
            let total: Duration = durations.values().sum();
            Some(total / durations.len() as u32)
        };
        EtaModel {
            durations,
            mean_known,
        }
    }

    /// Per-test estimate: historical if known, otherwise the mean across known
    /// tests. Returns `Duration::ZERO` only when no history exists at all.
    pub fn duration_for(&self, test_id: &TestId) -> Duration {
        self.durations
            .get(test_id)
            .copied()
            .or(self.mean_known)
            .unwrap_or(Duration::ZERO)
    }

    /// Total estimated wall time for a set of tests.
    pub fn estimated_total<'a, I>(&self, tests: I) -> Duration
    where
        I: IntoIterator<Item = &'a TestId>,
    {
        tests.into_iter().map(|id| self.duration_for(id)).sum()
    }

    /// Whether we have any historical signal to base an ETA on.
    pub fn has_history(&self) -> bool {
        self.mean_known.is_some()
    }
}

/// Format an ETA string based on historical test times.
fn format_eta(
    estimated_total: Duration,
    completed_duration: Duration,
    elapsed: Duration,
) -> String {
    if estimated_total.is_zero() || elapsed.is_zero() {
        return String::new();
    }

    // Clamp fraction_done to (0, 1] so the ETA stays visible even when the run
    // outpaces the historical estimate. With clamping, "ahead of schedule"
    // produces a small remaining value rather than a blank.
    let raw_fraction = completed_duration.as_secs_f64() / estimated_total.as_secs_f64();
    if raw_fraction <= 0.0 {
        return String::new();
    }
    let fraction_done = raw_fraction.min(1.0);

    let projected_total = elapsed.as_secs_f64() / fraction_done;
    let remaining = (projected_total - elapsed.as_secs_f64()).max(0.0);

    format!(
        " ETA: {}",
        format_duration_short(Duration::from_secs_f64(remaining))
    )
}

/// Format a duration as a human-readable string (e.g., "1m 23s", "45s", "2h 05m").
fn format_duration_short(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{}h {:02}m", hours, mins)
    } else if secs >= 60 {
        let mins = secs / 60;
        let remaining_secs = secs % 60;
        format!("{}m {:02}s", mins, remaining_secs)
    } else {
        format!("{}s", secs)
    }
}

/// Shared state behind the `{eta_hist}` template key. Workers update
/// `completed_duration` as tests finish; indicatif reads it on each draw, so
/// the displayed ETA decays with elapsed time even when no test is completing.
///
/// The elapsed clock is rebased to the *first test start* (the first subunit
/// `InProgress` event), not the moment we constructed the EtaState. Test
/// commands typically spend a chunk of wall-clock time on discovery / import
/// before emitting any subunit events; counting that warmup as elapsed would
/// inflate the projected total and make the ETA collapse rapidly once tests
/// finally start running. Anchoring at first-start (rather than
/// first-completion) keeps the very first test's runtime in the denominator,
/// so the rate signal is honest from the moment any ETA is displayed.
///
/// Credit math is "wall-clock-equivalent" rather than "expected duration":
///
/// * In-flight tests contribute `min(time-since-start, expected)` — capped so
///   an over-running test doesn't push fraction_done past 1.
/// * On completion we credit the *actual* runtime (smoothly continuing where
///   in-flight credit left off — no jump) and adjust `estimated_total` by the
///   delta between actual and expected. A test that ran 2s faster than
///   predicted shrinks the remaining target by 2s; one that ran 2s slower
///   grows it by 2s.
///
/// The result: `completed_duration` and `estimated_total` are both denominated
/// in wall-clock seconds, so the ratio is a stable progress signal that
/// doesn't lurch at completion events. For parallel runs this is essential —
/// with `c` workers, in-flight credit grows ~`c×` faster than elapsed, so
/// projected_total ≈ sequential_total / c, matching real wall-clock.
pub struct EtaState {
    progress: Mutex<EtaProgress>,
}

struct EtaProgress {
    /// Live target. Starts at the sum of historical durations and gets
    /// nudged by completions whose actual runtime differs from the
    /// historical prediction.
    estimated_total: Duration,
    /// Sum of *actual* runtimes for tests that have finished.
    completed_duration: Duration,
    /// Tests currently running: started_at + their historical duration, used
    /// to credit partial progress between completion events.
    in_flight: HashMap<TestId, (Instant, Duration)>,
    /// Set on the first `mark_started` call (any test).
    first_start_at: Option<Instant>,
}

impl EtaState {
    /// Build a fresh state with the given starting `estimated_total` (sum of
    /// historical durations for the tests this state will track).
    pub fn new(estimated_total: Duration) -> Arc<Self> {
        Arc::new(EtaState {
            progress: Mutex::new(EtaProgress {
                estimated_total,
                completed_duration: Duration::ZERO,
                in_flight: HashMap::new(),
                first_start_at: None,
            }),
        })
    }

    /// Record that `test_id` has started; `expected` is its historical
    /// duration (or the model's mean fallback for unknown tests). The first
    /// call also pins the elapsed-clock reference point.
    pub fn mark_started(&self, test_id: &TestId, expected: Duration) {
        let mut progress = self.progress.lock().unwrap();
        if progress.first_start_at.is_none() {
            progress.first_start_at = Some(Instant::now());
        }
        progress
            .in_flight
            .insert(test_id.clone(), (Instant::now(), expected));
    }

    /// Move a test from in-flight to completed. `expected` is its historical
    /// duration (used to refine `estimated_total`); the actual runtime is
    /// derived from the recorded start instant.
    pub fn add_completed(&self, test_id: &TestId, expected: Duration) {
        let mut progress = self.progress.lock().unwrap();
        let actual = progress
            .in_flight
            .remove(test_id)
            .map(|(started_at, _)| started_at.elapsed())
            .unwrap_or(expected);
        progress.completed_duration += actual;
        // Refine the target: if the test ran faster than predicted, the
        // remaining work shrinks; if slower, it grows. Without this the
        // ratio would lurch at every completion whose actual differed from
        // expected, which is the vast majority of completions.
        if actual >= expected {
            progress.estimated_total += actual - expected;
        } else {
            progress.estimated_total = progress.estimated_total.saturating_sub(expected - actual);
        }
    }

    /// Render the ETA string for indicatif's `{eta_hist}` template key.
    /// Empty when no test has started yet, or when the current credit pool
    /// is below the noise threshold for a meaningful projection.
    pub fn render(&self) -> String {
        let progress = self.progress.lock().unwrap();
        let Some(first) = progress.first_start_at else {
            return String::new();
        };
        let now = Instant::now();
        let in_flight_credit: Duration = progress
            .in_flight
            .values()
            .map(|(started_at, expected)| {
                let running_for = now.saturating_duration_since(*started_at);
                running_for.min(*expected)
            })
            .sum();
        format_eta(
            progress.estimated_total,
            progress.completed_duration + in_flight_credit,
            first.elapsed(),
        )
    }

    /// Snapshot of the current credit pool (`completed_duration +
    /// in_flight_credit`). Used by tests to verify that completion events
    /// don't introduce step changes in displayed progress.
    #[cfg(test)]
    fn credit_snapshot(&self) -> Duration {
        let progress = self.progress.lock().unwrap();
        let now = Instant::now();
        let in_flight: Duration = progress
            .in_flight
            .values()
            .map(|(s, e)| now.saturating_duration_since(*s).min(*e))
            .sum();
        progress.completed_duration + in_flight
    }

    /// Current `estimated_total` value. Tests use this to verify the dynamic
    /// adjustment logic in [`add_completed`].
    #[cfg(test)]
    fn estimated_total(&self) -> Duration {
        self.progress.lock().unwrap().estimated_total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_short_seconds() {
        assert_eq!(format_duration_short(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn format_duration_short_minutes() {
        assert_eq!(format_duration_short(Duration::from_secs(90)), "1m 30s");
    }

    #[test]
    fn format_duration_short_hours() {
        assert_eq!(format_duration_short(Duration::from_secs(3661)), "1h 01m");
    }

    #[test]
    fn format_duration_short_zero() {
        assert_eq!(format_duration_short(Duration::ZERO), "0s");
    }

    #[test]
    fn format_eta_with_history() {
        let eta = format_eta(
            Duration::from_secs(100),
            Duration::from_secs(50),
            Duration::from_secs(60),
        );
        assert_eq!(eta, " ETA: 1m 00s");
    }

    #[test]
    fn format_eta_no_history() {
        let eta = format_eta(Duration::ZERO, Duration::ZERO, Duration::from_secs(10));
        assert_eq!(eta, "");
    }

    #[test]
    fn format_eta_no_elapsed() {
        let eta = format_eta(
            Duration::from_secs(100),
            Duration::from_secs(10),
            Duration::ZERO,
        );
        assert_eq!(eta, "");
    }

    #[test]
    fn format_eta_run_outpaces_estimate() {
        // When the actual run is faster than predicted, we clamp fraction_done
        // to 1.0 so the ETA stays visible (and shrinks to zero) rather than
        // disappearing.
        let eta = format_eta(
            Duration::from_secs(100),
            Duration::from_secs(120),
            Duration::from_secs(90),
        );
        assert_eq!(eta, " ETA: 0s");
    }

    #[test]
    fn eta_model_empty_history() {
        let model = EtaModel::new(Arc::new(HashMap::new()));
        assert!(!model.has_history());
        assert_eq!(model.duration_for(&TestId::new("anything")), Duration::ZERO);
    }

    #[test]
    fn eta_model_uses_mean_for_unknown() {
        let mut durations = HashMap::new();
        durations.insert(TestId::new("a"), Duration::from_secs(2));
        durations.insert(TestId::new("b"), Duration::from_secs(4));
        let model = EtaModel::new(Arc::new(durations));
        assert!(model.has_history());
        assert_eq!(
            model.duration_for(&TestId::new("a")),
            Duration::from_secs(2)
        );
        // Unknown test uses the mean of known durations (3s).
        assert_eq!(
            model.duration_for(&TestId::new("c")),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn eta_model_estimated_total_mixes_known_and_unknown() {
        let mut durations = HashMap::new();
        durations.insert(TestId::new("a"), Duration::from_secs(10));
        durations.insert(TestId::new("b"), Duration::from_secs(20));
        let model = EtaModel::new(Arc::new(durations));
        let tests = [TestId::new("a"), TestId::new("b"), TestId::new("c")];
        // Known tests sum to 30s; unknown gets 15s mean -> 45s total.
        assert_eq!(model.estimated_total(tests.iter()), Duration::from_secs(45));
    }

    #[test]
    fn eta_state_no_render_until_first_test_starts() {
        let state = EtaState::new(Duration::from_secs(100));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(state.render(), "");
        // A spurious completion without a prior start still does not render.
        state.add_completed(&TestId::new("a"), Duration::from_secs(50));
        assert_eq!(state.render(), "");
    }

    #[test]
    fn eta_state_renders_after_first_start() {
        let state = EtaState::new(Duration::from_secs(100));
        state.mark_started(&TestId::new("a"), Duration::from_secs(50));
        state.add_completed(&TestId::new("a"), Duration::from_secs(50));
        std::thread::sleep(Duration::from_millis(20));
        let rendered = state.render();
        assert!(rendered.starts_with(" ETA: "), "got: {:?}", rendered);
    }

    #[test]
    fn eta_state_in_flight_partial_credit() {
        // While a test is running, render() should credit it for time
        // already elapsed (capped at its expected duration). Without this,
        // completed_duration would stay flat between completion events.
        let state = EtaState::new(Duration::from_secs(100));
        state.mark_started(&TestId::new("a"), Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(40));
        // No completion yet, but the in-flight credit + non-zero elapsed
        // should produce a finite ETA string.
        let rendered = state.render();
        assert!(rendered.starts_with(" ETA: "), "got: {:?}", rendered);
    }

    #[test]
    fn eta_state_in_flight_credit_capped_at_expected() {
        // A test that runs longer than its history shouldn't accumulate
        // credit past its expected duration, otherwise an over-running test
        // would push completed past estimated_total and hide the ETA.
        let state = EtaState::new(Duration::from_secs(10));
        state.mark_started(&TestId::new("a"), Duration::from_millis(5));
        std::thread::sleep(Duration::from_millis(50));
        let rendered = state.render();
        // Expected credit is 5ms (capped); fraction_done = 5ms/10s ≈ 0.0005.
        // After ~50ms elapsed, projected = 50ms / 0.0005 = 100s, remaining
        // ≈ 100s. We don't assert exact number — just that the ETA is
        // present and finite (large remaining is fine; what we want to
        // avoid is the empty-string return that you'd get if credit had
        // exceeded estimated_total).
        assert!(rendered.starts_with(" ETA: "), "got: {:?}", rendered);
    }

    #[test]
    fn eta_state_completion_does_not_snap() {
        // The classic staircase: test runs, in-flight credit ticks up, then
        // completion snaps credit by `expected - actual_running_time`.
        // With wall-clock-equivalent semantics that snap should be zero —
        // completion just promotes the in-flight credit to completed without
        // changing the (completed + in_flight_credit) sum.
        let state = EtaState::new(Duration::from_secs(100));
        state.mark_started(&TestId::new("a"), Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(20));

        let credit_before = state.credit_snapshot();
        state.add_completed(&TestId::new("a"), Duration::from_secs(60));
        let credit_after = state.credit_snapshot();

        // Should be within ~1ms (just the time spent acquiring the lock and
        // measuring `started_at.elapsed()`). The pre-fix code would have
        // snapped by ~60s here (expected - actual ≈ 60s - 20ms).
        let drift = credit_after.saturating_sub(credit_before);
        assert!(
            drift < Duration::from_millis(10),
            "credit jumped by {:?} at completion (expected near zero)",
            drift
        );
    }

    #[test]
    fn eta_state_estimated_total_shrinks_when_test_is_fast() {
        let state = EtaState::new(Duration::from_secs(100));
        state.mark_started(&TestId::new("a"), Duration::from_secs(60));
        // No sleep: actual ≈ 0, expected = 60s. estimated_total should
        // shrink by ~60s.
        state.add_completed(&TestId::new("a"), Duration::from_secs(60));
        let total = state.estimated_total();
        assert!(
            total < Duration::from_secs(41),
            "estimated_total = {:?}, expected ≈40s",
            total
        );
        assert!(
            total > Duration::from_secs(39),
            "estimated_total = {:?}, expected ≈40s",
            total
        );
    }

    #[test]
    fn eta_state_completion_removes_from_in_flight() {
        // After completion, the test no longer contributes via in-flight
        // credit; only via completed_duration. Two starts then one
        // completion should leave one in-flight + one completed.
        let state = EtaState::new(Duration::from_secs(100));
        state.mark_started(&TestId::new("a"), Duration::from_secs(40));
        state.mark_started(&TestId::new("b"), Duration::from_secs(40));
        state.add_completed(&TestId::new("a"), Duration::from_secs(40));
        std::thread::sleep(Duration::from_millis(20));
        // Should still render — first_start_at is set and we have
        // completed + in-flight credit.
        let rendered = state.render();
        assert!(rendered.starts_with(" ETA: "), "got: {:?}", rendered);
    }
}
