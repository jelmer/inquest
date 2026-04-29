//! ETA computation for the test-runner progress bar.
//!
//! The display has three halves: an [`EtaModel`] that resolves a per-test
//! historical duration (with a mean fallback for tests that have no recorded
//! history), an [`EtaState`] that aggregates progress across one or more
//! workers and renders the formatted ETA string for indicatif's `{eta_hist}`
//! template key, and a calibration helper that learns a multiplicative
//! correction from past `(predicted, actual)` pairs so the displayed ETA
//! accounts for systematic biases (parallel overhead, discovery time, etc.).

use crate::repository::{Repository, RunId, TestId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How many recent runs to inspect when computing the calibration factor.
/// Older runs are ignored entirely; bounding the lookback keeps the factor
/// responsive to environment changes (CI host swap, machine upgrade, …).
const CALIBRATION_LOOKBACK: usize = 20;

/// Minimum number of usable samples in the lookback window before we apply
/// any correction. Below this threshold a single anomalous run would
/// dominate, so we fall back to the raw historical-sum prediction.
const CALIBRATION_MIN_SAMPLES: usize = 3;

/// EWMA decay coefficient. `α = 0.3` weights the most recent sample at 30%
/// and decays older samples geometrically — recent enough to react to a
/// regression, smooth enough to ignore one-off noise.
const CALIBRATION_ALPHA: f64 = 0.3;

/// Clamp range for the resulting factor. A factor outside this range is
/// almost always a bug or a degenerate prediction (e.g. predicted ≈ 0 due
/// to missing history) rather than a real signal worth honouring.
const CALIBRATION_MIN: f64 = 0.25;
const CALIBRATION_MAX: f64 = 4.0;

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

/// One observation of a past run's predicted-vs-actual duration, narrowed to
/// the fields the calibration math actually uses.
#[derive(Debug, Clone, Copy)]
pub struct CalibrationSample {
    /// Concurrency the run executed at. Used as the bucket key — a parallel
    /// run's overhead profile differs sharply from a serial run's, so they
    /// must not feed the same factor.
    pub concurrency: u32,
    /// Wall-clock prediction recorded at run start (before calibration).
    pub predicted_secs: f64,
    /// Wall-clock duration the run actually took.
    pub actual_secs: f64,
}

/// Compute a multiplicative correction for the raw historical-sum prediction
/// at the given concurrency, derived from the most recent
/// [`CALIBRATION_LOOKBACK`] runs at that same concurrency.
///
/// Returns `1.0` (i.e. "no correction") when fewer than
/// [`CALIBRATION_MIN_SAMPLES`] usable samples are available — one or two
/// runs is too little signal to override a known prediction with.
///
/// `samples` is expected in chronological order (oldest first); the EWMA
/// weights the most recent sample most heavily.
pub fn calibration_factor(samples: &[CalibrationSample], concurrency: u32) -> f64 {
    let bucket: Vec<f64> = samples
        .iter()
        .filter(|s| s.concurrency == concurrency)
        .filter(|s| s.predicted_secs > 0.0 && s.actual_secs > 0.0)
        .rev()
        .take(CALIBRATION_LOOKBACK)
        .map(|s| s.actual_secs / s.predicted_secs)
        .collect();

    if bucket.len() < CALIBRATION_MIN_SAMPLES {
        return 1.0;
    }

    // `bucket` is newest-first; walk oldest-first to apply EWMA, so the
    // newest sample lands with full α weight at the end.
    let mut ewma: Option<f64> = None;
    for ratio in bucket.iter().rev() {
        ewma = Some(match ewma {
            None => *ratio,
            Some(prev) => CALIBRATION_ALPHA * ratio + (1.0 - CALIBRATION_ALPHA) * prev,
        });
    }
    ewma.unwrap_or(1.0).clamp(CALIBRATION_MIN, CALIBRATION_MAX)
}

/// Why a calibration factor took the value it did. Built alongside the
/// factor for `--debug` output so the user can see why the displayed ETA
/// is (or isn't) being adjusted.
#[derive(Debug, Clone)]
pub struct CalibrationDebug {
    /// Total samples loaded across all concurrencies.
    pub total_samples: usize,
    /// Samples that matched this run's concurrency and had usable values.
    /// Only samples within the lookback window count.
    pub bucket_samples: usize,
    /// Concurrency used as the bucket key.
    pub concurrency: u32,
    /// Final calibration factor (clamped). `1.0` when the bucket was below
    /// the minimum-samples threshold or the EWMA saturated against the
    /// configured clamp.
    pub factor: f64,
    /// Most recent (oldest-first) `actual / predicted` ratios that fed the
    /// EWMA. Capped at [`CALIBRATION_LOOKBACK`].
    pub recent_ratios: Vec<f64>,
    /// Minimum samples needed before any correction is applied.
    pub min_samples: usize,
}

/// Compute the calibration factor *and* a structured trace of how it was
/// derived. Equivalent to [`calibration_factor`] but with the inputs and
/// intermediate values surfaced for debug output.
pub fn calibration_debug(samples: &[CalibrationSample], concurrency: u32) -> CalibrationDebug {
    let bucket: Vec<f64> = samples
        .iter()
        .filter(|s| s.concurrency == concurrency)
        .filter(|s| s.predicted_secs > 0.0 && s.actual_secs > 0.0)
        .rev()
        .take(CALIBRATION_LOOKBACK)
        .map(|s| s.actual_secs / s.predicted_secs)
        .collect();

    // Restore chronological (oldest-first) order for display and EWMA.
    let mut recent_ratios = bucket;
    recent_ratios.reverse();

    let factor = calibration_factor(samples, concurrency);
    CalibrationDebug {
        total_samples: samples.len(),
        bucket_samples: recent_ratios.len(),
        concurrency,
        factor,
        recent_ratios,
        min_samples: CALIBRATION_MIN_SAMPLES,
    }
}

/// Render a one-block summary of how the displayed wall-clock ETA was
/// derived: how many tests have history, the raw historical sum, the
/// concurrency divisor (for parallel runs), and the final calibrated
/// number the user will see decay on the progress bar.
///
/// `tests` is the resolved test list; pass an empty slice to skip the
/// per-test counts (e.g. when discovery-driven, the executor will compute
/// these itself and the caller can't pre-empt them cheaply).
pub fn format_prediction_debug(
    tests: &[TestId],
    historical_times: &HashMap<TestId, Duration>,
    concurrency: u32,
    calibration_factor: f64,
) -> Vec<String> {
    if tests.is_empty() {
        return vec![
            "ETA debug: prediction breakdown unavailable (tests will be \
                     discovered by the runner)."
                .to_string(),
        ];
    }
    let known: usize = tests
        .iter()
        .filter(|t| historical_times.contains_key(*t))
        .count();
    let unknown = tests.len().saturating_sub(known);
    let restricted: HashMap<TestId, Duration> = historical_times
        .iter()
        .filter(|(id, _)| tests.iter().any(|t| t == *id))
        .map(|(id, d)| (id.clone(), *d))
        .collect();
    let model = EtaModel::new(Arc::new(restricted));
    let raw_total = model.estimated_total(tests.iter());
    let mut lines = Vec::new();
    lines.push(format!(
        "ETA debug: {} test(s) selected — {} with history, {} new (mean fallback {}).",
        tests.len(),
        known,
        unknown,
        format_duration_short(model.mean_known.unwrap_or(Duration::ZERO)),
    ));
    let serial_eq = format_duration_short(raw_total);
    if concurrency > 1 {
        let per_worker = raw_total / concurrency;
        let calibrated = per_worker.mul_f64(calibration_factor);
        lines.push(format!(
            "  Raw sequential estimate: {} → {}-way parallel: {} → calibrated (×{:.2}): {}",
            serial_eq,
            concurrency,
            format_duration_short(per_worker),
            calibration_factor,
            format_duration_short(calibrated),
        ));
    } else {
        let calibrated = raw_total.mul_f64(calibration_factor);
        lines.push(format!(
            "  Raw estimate: {} → calibrated (×{:.2}): {}",
            serial_eq,
            calibration_factor,
            format_duration_short(calibrated),
        ));
    }
    lines
}

/// Render the calibration debug as the lines to print under `--debug`.
/// Lines come without a leading newline so the caller can decide framing.
pub fn format_calibration_debug(debug: &CalibrationDebug) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "ETA debug: {} sample(s) total, {} at concurrency={} (need ≥{}).",
        debug.total_samples, debug.bucket_samples, debug.concurrency, debug.min_samples,
    ));
    if debug.bucket_samples < debug.min_samples {
        lines.push(format!(
            "  Calibration factor: {:.2} (too few samples — using raw historical sum).",
            debug.factor,
        ));
    } else {
        lines.push(format!(
            "  Calibration factor: {:.2} (EWMA α={}, clamp [{:.2}, {:.2}]).",
            debug.factor, CALIBRATION_ALPHA, CALIBRATION_MIN, CALIBRATION_MAX,
        ));
    }
    if !debug.recent_ratios.is_empty() {
        let formatted: Vec<String> = debug
            .recent_ratios
            .iter()
            .map(|r| format!("{:.2}", r))
            .collect();
        lines.push(format!(
            "  Recent actual/predicted ratios (oldest→newest): [{}]",
            formatted.join(", "),
        ));
    }
    lines
}

/// Load calibration samples from the repository's recent runs. Pulls only
/// what `calibration_factor` will actually consume — the most recent
/// `CALIBRATION_LOOKBACK × 4` runs across all concurrencies, so a busy
/// repo with many small concurrencies still gets enough samples in any one
/// bucket. Reads are best-effort: a missing or malformed run is silently
/// skipped, since calibration is an optimisation rather than a correctness
/// requirement.
pub fn load_calibration_samples(repo: &dyn Repository) -> Vec<CalibrationSample> {
    let Ok(ids) = repo.list_run_ids() else {
        return Vec::new();
    };
    let scan_limit = CALIBRATION_LOOKBACK.saturating_mul(4);
    let recent_ids: Vec<&RunId> = ids.iter().rev().take(scan_limit).collect();

    let mut samples: Vec<CalibrationSample> = Vec::with_capacity(recent_ids.len());
    // Walk oldest-first within the recent window so the EWMA sees newest last.
    for id in recent_ids.iter().rev() {
        let Ok(meta) = repo.get_run_metadata(id) else {
            continue;
        };
        let (Some(predicted_secs), Some(actual_secs), Some(concurrency)) = (
            meta.predicted_duration_secs,
            meta.duration_secs,
            meta.concurrency,
        ) else {
            continue;
        };
        samples.push(CalibrationSample {
            concurrency,
            predicted_secs,
            actual_secs,
        });
    }
    samples
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

    fn sample(concurrency: u32, predicted: f64, actual: f64) -> CalibrationSample {
        CalibrationSample {
            concurrency,
            predicted_secs: predicted,
            actual_secs: actual,
        }
    }

    #[test]
    fn calibration_returns_neutral_with_too_few_samples() {
        let samples = vec![sample(4, 60.0, 90.0), sample(4, 60.0, 90.0)];
        assert_eq!(calibration_factor(&samples, 4), 1.0);
    }

    #[test]
    fn calibration_filters_to_matching_concurrency() {
        let samples = vec![
            sample(1, 60.0, 90.0),
            sample(1, 60.0, 90.0),
            sample(1, 60.0, 90.0),
            sample(4, 60.0, 30.0),
        ];
        // Only one sample at concurrency=4 — too few, so factor is 1.0.
        assert_eq!(calibration_factor(&samples, 4), 1.0);
        // Three samples at concurrency=1 with ratio 1.5 — factor should
        // converge there (EWMA on a constant series collapses to that value).
        let f = calibration_factor(&samples, 1);
        assert!((f - 1.5).abs() < 1e-9, "got {}", f);
    }

    #[test]
    fn calibration_skips_zero_or_negative_predictions() {
        let samples = vec![
            sample(2, 0.0, 90.0),  // predicted=0: skipped
            sample(2, 60.0, 0.0),  // actual=0: skipped
            sample(2, -1.0, 50.0), // negative: skipped
            sample(2, 60.0, 60.0),
            sample(2, 60.0, 60.0),
        ];
        // Only two valid samples — below threshold, so 1.0.
        assert_eq!(calibration_factor(&samples, 2), 1.0);
    }

    #[test]
    fn calibration_weights_recent_samples_more() {
        // Old runs were 2x slower than predicted, recent runs are accurate.
        // EWMA should land closer to 1.0 than to 2.0.
        let samples = vec![
            sample(4, 60.0, 120.0), // ratio 2.0
            sample(4, 60.0, 120.0), // 2.0
            sample(4, 60.0, 120.0), // 2.0
            sample(4, 60.0, 60.0),  // 1.0
            sample(4, 60.0, 60.0),  // 1.0
            sample(4, 60.0, 60.0),  // 1.0
        ];
        let f = calibration_factor(&samples, 4);
        // Closer to recent (1.0) than to old (2.0).
        assert!(f < 1.4, "expected EWMA < 1.4, got {}", f);
        assert!(f > 1.0, "expected EWMA > 1.0, got {}", f);
    }

    #[test]
    fn calibration_clamps_outliers() {
        // A pathological 100x slowdown should still be bounded.
        let samples = vec![
            sample(2, 1.0, 100.0),
            sample(2, 1.0, 100.0),
            sample(2, 1.0, 100.0),
        ];
        let f = calibration_factor(&samples, 2);
        assert_eq!(f, CALIBRATION_MAX);
    }

    #[test]
    fn calibration_clamps_underestimate_outliers() {
        let samples = vec![
            sample(2, 100.0, 1.0),
            sample(2, 100.0, 1.0),
            sample(2, 100.0, 1.0),
        ];
        let f = calibration_factor(&samples, 2);
        assert_eq!(f, CALIBRATION_MIN);
    }

    #[test]
    fn load_calibration_samples_pulls_runs_with_full_metadata() {
        use crate::repository::{
            inquest::InquestRepositoryFactory, RepositoryFactory, RunMetadata, TestResult, TestRun,
        };

        let temp = tempfile::TempDir::new().unwrap();
        let mut repo = InquestRepositoryFactory.initialise(temp.path()).unwrap();

        let mut run0 = TestRun::new(RunId::new("0"));
        run0.timestamp = chrono::DateTime::from_timestamp(1_000_000_000, 0).unwrap();
        run0.add_result(TestResult::success("a"));
        let id0 = repo.insert_test_run(run0).unwrap();
        repo.set_run_metadata(
            &id0,
            RunMetadata {
                concurrency: Some(2),
                duration_secs: Some(45.0),
                predicted_duration_secs: Some(30.0),
                ..RunMetadata::default()
            },
        )
        .unwrap();

        // A run missing predicted_duration — should be skipped silently.
        let mut run1 = TestRun::new(RunId::new("1"));
        run1.timestamp = chrono::DateTime::from_timestamp(1_000_000_001, 0).unwrap();
        run1.add_result(TestResult::success("a"));
        let id1 = repo.insert_test_run(run1).unwrap();
        repo.set_run_metadata(
            &id1,
            RunMetadata {
                concurrency: Some(2),
                duration_secs: Some(20.0),
                predicted_duration_secs: None,
                ..RunMetadata::default()
            },
        )
        .unwrap();

        let samples = load_calibration_samples(repo.as_ref());
        assert_eq!(samples.len(), 1, "got: {:?}", samples);
        assert_eq!(samples[0].concurrency, 2);
        assert!((samples[0].predicted_secs - 30.0).abs() < 1e-9);
        assert!((samples[0].actual_secs - 45.0).abs() < 1e-9);
    }

    #[test]
    fn calibration_lookback_caps_history_window() {
        // Many old "1.0" runs followed by a few recent "2.0" runs. With
        // CALIBRATION_LOOKBACK=20 the recent runs dominate completely once
        // there are enough; if the cap weren't applied, the long tail of
        // 1.0s would pull the EWMA down further than it should.
        let mut samples = Vec::new();
        for _ in 0..50 {
            samples.push(sample(1, 10.0, 10.0)); // ratio 1.0
        }
        for _ in 0..10 {
            samples.push(sample(1, 10.0, 20.0)); // ratio 2.0
        }
        let f = calibration_factor(&samples, 1);
        // With the cap, only the last 20 samples count: 10 of ratio 1.0 then
        // 10 of ratio 2.0. Without the cap, it'd be ~1.0. Verify we're at
        // least clearly above 1.0.
        assert!(
            f > 1.5,
            "expected factor > 1.5 with lookback cap, got {}",
            f
        );
    }

    #[test]
    fn format_calibration_debug_too_few_samples() {
        let samples = vec![sample(4, 60.0, 90.0), sample(4, 60.0, 90.0)];
        let debug = calibration_debug(&samples, 4);
        let lines = format_calibration_debug(&debug);
        assert_eq!(
            lines,
            vec![
                "ETA debug: 2 sample(s) total, 2 at concurrency=4 (need ≥3).".to_string(),
                "  Calibration factor: 1.00 (too few samples — using raw historical sum)."
                    .to_string(),
                "  Recent actual/predicted ratios (oldest→newest): [1.50, 1.50]".to_string(),
            ]
        );
    }

    #[test]
    fn format_calibration_debug_with_factor() {
        let samples = vec![
            sample(2, 60.0, 90.0),
            sample(2, 60.0, 90.0),
            sample(2, 60.0, 90.0),
        ];
        let debug = calibration_debug(&samples, 2);
        let lines = format_calibration_debug(&debug);
        assert_eq!(
            lines,
            vec![
                "ETA debug: 3 sample(s) total, 3 at concurrency=2 (need ≥3).".to_string(),
                "  Calibration factor: 1.50 (EWMA α=0.3, clamp [0.25, 4.00]).".to_string(),
                "  Recent actual/predicted ratios (oldest→newest): [1.50, 1.50, 1.50]".to_string(),
            ]
        );
    }

    #[test]
    fn format_calibration_debug_no_samples() {
        let debug = calibration_debug(&[], 4);
        let lines = format_calibration_debug(&debug);
        assert_eq!(
            lines,
            vec![
                "ETA debug: 0 sample(s) total, 0 at concurrency=4 (need ≥3).".to_string(),
                "  Calibration factor: 1.00 (too few samples — using raw historical sum)."
                    .to_string(),
            ]
        );
    }

    #[test]
    fn format_prediction_debug_serial() {
        let mut historical = HashMap::new();
        historical.insert(TestId::new("a"), Duration::from_secs(10));
        historical.insert(TestId::new("b"), Duration::from_secs(20));
        let tests = vec![TestId::new("a"), TestId::new("b"), TestId::new("c")];
        let lines = format_prediction_debug(&tests, &historical, 1, 1.0);
        // Known: a, b. Unknown: c → mean fallback 15s. Total 10+20+15 = 45s.
        assert_eq!(
            lines,
            vec![
                "ETA debug: 3 test(s) selected — 2 with history, 1 new (mean fallback 15s)."
                    .to_string(),
                "  Raw estimate: 45s → calibrated (×1.00): 45s".to_string(),
            ]
        );
    }

    #[test]
    fn format_prediction_debug_parallel_with_calibration() {
        let mut historical = HashMap::new();
        historical.insert(TestId::new("a"), Duration::from_secs(60));
        historical.insert(TestId::new("b"), Duration::from_secs(60));
        historical.insert(TestId::new("c"), Duration::from_secs(60));
        historical.insert(TestId::new("d"), Duration::from_secs(60));
        let tests = vec![
            TestId::new("a"),
            TestId::new("b"),
            TestId::new("c"),
            TestId::new("d"),
        ];
        let lines = format_prediction_debug(&tests, &historical, 4, 1.5);
        // Raw 4×60 = 240s. /4 = 60s. ×1.5 = 90s.
        assert_eq!(
            lines,
            vec![
                "ETA debug: 4 test(s) selected — 4 with history, 0 new (mean fallback 1m 00s)."
                    .to_string(),
                "  Raw sequential estimate: 4m 00s → 4-way parallel: 1m 00s → calibrated \
                 (×1.50): 1m 30s"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn format_prediction_debug_unknown_tests() {
        let lines = format_prediction_debug(&[], &HashMap::new(), 2, 1.0);
        assert_eq!(
            lines,
            vec![
                "ETA debug: prediction breakdown unavailable (tests will be discovered \
                 by the runner)."
                    .to_string()
            ]
        );
    }
}
