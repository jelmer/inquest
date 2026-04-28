//! Drop older test runs from the repository

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::error::{Error, Result};
use crate::repository::{Repository, RunId};
use crate::ui::UI;
use std::time::Duration;

/// Selection rule for which runs to prune.
pub enum PruneSelection {
    /// Keep the most recent `n` runs, prune everything older.
    Keep(usize),
    /// Prune runs with a start timestamp older than the given duration.
    OlderThan(Duration),
    /// Prune the explicit list of run IDs.
    Explicit(Vec<String>),
    /// Prune every run in the repository.
    All,
}

/// Command that drops older test runs from a repository.
pub struct PruneCommand {
    base_path: Option<String>,
    selection: PruneSelection,
    dry_run: bool,
}

impl PruneCommand {
    /// Build a new prune command.
    pub fn new(base_path: Option<String>, selection: PruneSelection, dry_run: bool) -> Self {
        Self {
            base_path,
            selection,
            dry_run,
        }
    }
}

/// Resolve a [`PruneSelection`] against the current repository state into the
/// concrete list of run IDs that should be pruned.
fn select_run_ids(repo: &dyn Repository, selection: &PruneSelection) -> Result<Vec<RunId>> {
    let all_ids = repo.list_run_ids()?;

    match selection {
        PruneSelection::Keep(n) => {
            if all_ids.len() <= *n {
                return Ok(Vec::new());
            }
            let cutoff = all_ids.len() - *n;
            Ok(all_ids[..cutoff].to_vec())
        }
        PruneSelection::OlderThan(duration) => {
            let cutoff = chrono::Utc::now()
                - chrono::Duration::from_std(*duration)
                    .map_err(|e| Error::Other(format!("duration too large to convert: {}", e)))?;
            let mut to_prune = Vec::new();
            for run_id in &all_ids {
                if let Some(started) = repo.get_run_started_at(run_id)? {
                    if started < cutoff {
                        to_prune.push(run_id.clone());
                    }
                }
            }
            Ok(to_prune)
        }
        PruneSelection::Explicit(ids) => {
            // Resolve via list_run_ids so the diagnostic for an unknown ID
            // names the actual missing run instead of failing late inside
            // the backend.
            let known: std::collections::HashSet<&str> =
                all_ids.iter().map(|r| r.as_str()).collect();
            let mut to_prune = Vec::with_capacity(ids.len());
            for id in ids {
                if !known.contains(id.as_str()) {
                    return Err(Error::TestRunNotFound(id.clone()));
                }
                to_prune.push(RunId::new(id.clone()));
            }
            Ok(to_prune)
        }
        PruneSelection::All => Ok(all_ids),
    }
}

impl Command for PruneCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let mut repo = open_repository(self.base_path.as_deref())?;

        let to_prune = select_run_ids(&*repo, &self.selection)?;

        // Drop in-progress runs from the candidate list with a clear notice
        // so the user understands why they were skipped.
        let mut pruneable = Vec::with_capacity(to_prune.len());
        for run_id in to_prune {
            if repo.is_run_in_progress(&run_id)? {
                ui.warning(&format!("Skipping in-progress run {}", run_id))?;
                continue;
            }
            pruneable.push(run_id);
        }

        if pruneable.is_empty() {
            ui.output("No runs to prune.")?;
            return Ok(0);
        }

        if self.dry_run {
            ui.output(&format!("Would prune {} run(s):", pruneable.len()))?;
            for run_id in &pruneable {
                ui.output(&format!("  {}", run_id))?;
            }
            return Ok(0);
        }

        let pruned = repo.prune_runs(&pruneable)?;
        ui.output(&format!("Pruned {} run(s).", pruned.len()))?;
        for run_id in &pruned {
            ui.output(&format!("  {}", run_id))?;
        }

        Ok(0)
    }

    fn name(&self) -> &str {
        "prune"
    }

    fn help(&self) -> &str {
        "Drop older test runs from the repository"
    }
}

/// Parse a duration string accepted by `--older-than`.
///
/// Supports `s`, `m`, `h`, `d`, and `w` suffixes. Unlike
/// [`crate::config::parse_duration_string`], days and weeks are common units
/// for prune retention and are accepted here.
pub fn parse_age_string(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::Other("empty duration string".to_string()));
    }

    let mut total_secs: f64 = 0.0;
    let mut current_num = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            current_num.push(c);
            continue;
        }
        if current_num.is_empty() {
            return Err(Error::Other(format!("invalid duration string: '{}'", s)));
        }
        let num: f64 = current_num
            .parse()
            .map_err(|_| Error::Other(format!("invalid number in duration string: '{}'", s)))?;
        current_num.clear();

        let multiplier = match c {
            's' => 1.0,
            'm' => 60.0,
            'h' => 3600.0,
            'd' => 86_400.0,
            'w' => 7.0 * 86_400.0,
            _ => {
                return Err(Error::Other(format!(
                    "invalid duration suffix '{}' in '{}' (use 's', 'm', 'h', 'd', or 'w')",
                    c, s
                )));
            }
        };
        total_secs += num * multiplier;
    }

    if !current_num.is_empty() {
        let num: f64 = current_num
            .parse()
            .map_err(|_| Error::Other(format!("invalid number in duration string: '{}'", s)))?;
        // Trailing bare numbers default to seconds, matching parse_duration_string.
        total_secs += num;
    }

    if total_secs <= 0.0 {
        return Err(Error::Other(format!("duration must be positive: '{}'", s)));
    }

    Ok(Duration::from_secs_f64(total_secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::utils::init_repository;
    use crate::repository::{RunId, TestResult, TestRun};
    use crate::ui::test_ui::TestUI;
    use tempfile::TempDir;

    fn add_run(repo: &mut dyn Repository, id: &str, ts: i64) {
        let mut run = TestRun::new(RunId::new(id));
        run.timestamp = chrono::DateTime::from_timestamp(ts, 0).unwrap();
        run.add_result(TestResult::success(format!("test_{}", id)));
        repo.insert_test_run(run).unwrap();
    }

    fn pruned_ids(ui: &TestUI) -> Vec<&str> {
        ui.output
            .iter()
            .filter_map(|line| line.strip_prefix("  "))
            .collect()
    }

    #[test]
    fn parse_age_string_units() {
        assert_eq!(parse_age_string("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_age_string("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_age_string("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(
            parse_age_string("2d").unwrap(),
            Duration::from_secs(172_800)
        );
        assert_eq!(
            parse_age_string("1w").unwrap(),
            Duration::from_secs(604_800)
        );
        assert_eq!(
            parse_age_string("1d12h").unwrap(),
            Duration::from_secs(86_400 + 12 * 3600)
        );
    }

    #[test]
    fn parse_age_string_rejects_invalid() {
        assert!(parse_age_string("").is_err());
        assert!(parse_age_string("abc").is_err());
        assert!(parse_age_string("3y").is_err());
        assert!(parse_age_string("0s").is_err());
    }

    #[test]
    fn keep_retains_most_recent() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let mut repo = init_repository(Some(&path)).unwrap();
        for i in 0..5 {
            add_run(repo.as_mut(), &i.to_string(), 1_000_000_000 + i);
        }

        let cmd = PruneCommand::new(Some(path.clone()), PruneSelection::Keep(2), false);
        let mut ui = TestUI::new();
        let exit = cmd.execute(&mut ui).unwrap();
        assert_eq!(exit, 0);

        let repo = open_repository(Some(&path)).unwrap();
        let remaining: Vec<String> = repo
            .list_run_ids()
            .unwrap()
            .iter()
            .map(|r| r.as_str().to_string())
            .collect();
        assert_eq!(remaining, vec!["3".to_string(), "4".to_string()]);
    }

    #[test]
    fn keep_zero_drops_everything() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let mut repo = init_repository(Some(&path)).unwrap();
        for i in 0..3 {
            add_run(repo.as_mut(), &i.to_string(), 1_000_000_000 + i);
        }

        let cmd = PruneCommand::new(Some(path.clone()), PruneSelection::Keep(0), false);
        let mut ui = TestUI::new();
        cmd.execute(&mut ui).unwrap();

        let repo = open_repository(Some(&path)).unwrap();
        assert!(repo.list_run_ids().unwrap().is_empty());
    }

    #[test]
    fn keep_more_than_runs_is_noop() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let mut repo = init_repository(Some(&path)).unwrap();
        for i in 0..2 {
            add_run(repo.as_mut(), &i.to_string(), 1_000_000_000 + i);
        }

        let cmd = PruneCommand::new(Some(path.clone()), PruneSelection::Keep(10), false);
        let mut ui = TestUI::new();
        cmd.execute(&mut ui).unwrap();
        assert!(ui.output.iter().any(|l| l == "No runs to prune."));

        let repo = open_repository(Some(&path)).unwrap();
        assert_eq!(repo.list_run_ids().unwrap().len(), 2);
    }

    #[test]
    fn explicit_prunes_listed_ids() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let mut repo = init_repository(Some(&path)).unwrap();
        for i in 0..3 {
            add_run(repo.as_mut(), &i.to_string(), 1_000_000_000 + i);
        }

        let cmd = PruneCommand::new(
            Some(path.clone()),
            PruneSelection::Explicit(vec!["1".to_string()]),
            false,
        );
        let mut ui = TestUI::new();
        cmd.execute(&mut ui).unwrap();

        let repo = open_repository(Some(&path)).unwrap();
        let remaining: Vec<String> = repo
            .list_run_ids()
            .unwrap()
            .iter()
            .map(|r| r.as_str().to_string())
            .collect();
        assert_eq!(remaining, vec!["0".to_string(), "2".to_string()]);
    }

    #[test]
    fn explicit_unknown_id_errors() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let mut repo = init_repository(Some(&path)).unwrap();
        add_run(repo.as_mut(), "0", 1_000_000_000);

        let cmd = PruneCommand::new(
            Some(path.clone()),
            PruneSelection::Explicit(vec!["42".to_string()]),
            false,
        );
        let mut ui = TestUI::new();
        let err = cmd.execute(&mut ui).unwrap_err();
        assert!(matches!(err, Error::TestRunNotFound(_)));
    }

    #[test]
    fn all_drops_every_run() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let mut repo = init_repository(Some(&path)).unwrap();
        for i in 0..4 {
            add_run(repo.as_mut(), &i.to_string(), 1_000_000_000 + i);
        }

        let cmd = PruneCommand::new(Some(path.clone()), PruneSelection::All, false);
        let mut ui = TestUI::new();
        cmd.execute(&mut ui).unwrap();

        let repo = open_repository(Some(&path)).unwrap();
        assert!(repo.list_run_ids().unwrap().is_empty());
    }

    #[test]
    fn dry_run_changes_nothing() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let mut repo = init_repository(Some(&path)).unwrap();
        for i in 0..3 {
            add_run(repo.as_mut(), &i.to_string(), 1_000_000_000 + i);
        }

        let cmd = PruneCommand::new(Some(path.clone()), PruneSelection::Keep(1), true);
        let mut ui = TestUI::new();
        cmd.execute(&mut ui).unwrap();
        let listed = pruned_ids(&ui);
        assert_eq!(listed, vec!["0", "1"]);

        let repo = open_repository(Some(&path)).unwrap();
        assert_eq!(repo.list_run_ids().unwrap().len(), 3);
    }

    #[test]
    fn older_than_uses_started_at() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let mut repo = init_repository(Some(&path)).unwrap();
        // Insert three runs through the normal path so each one gets a
        // realistic start timestamp. Pause briefly between runs so the
        // recorded timestamps are distinct.
        for i in 0..3 {
            add_run(repo.as_mut(), &i.to_string(), 1_000_000_000 + i);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // Nothing is older than 1 hour yet.
        let cmd = PruneCommand::new(
            Some(path.clone()),
            PruneSelection::OlderThan(Duration::from_secs(3600)),
            false,
        );
        let mut ui = TestUI::new();
        cmd.execute(&mut ui).unwrap();
        assert!(ui.output.iter().any(|l| l == "No runs to prune."));

        let repo = open_repository(Some(&path)).unwrap();
        assert_eq!(repo.list_run_ids().unwrap().len(), 3);
    }

    #[test]
    fn prune_clears_failing_tests_and_flakiness() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let mut repo = init_repository(Some(&path)).unwrap();

        // Run 0: test_a fails, test_b passes
        let mut run0 = TestRun::new(RunId::new("0"));
        run0.timestamp = chrono::DateTime::from_timestamp(1_000_000_000, 0).unwrap();
        run0.add_result(TestResult::failure("test_a", "boom"));
        run0.add_result(TestResult::success("test_b"));
        repo.insert_test_run(run0).unwrap();

        // Run 1: test_a passes, test_b fails
        let mut run1 = TestRun::new(RunId::new("1"));
        run1.timestamp = chrono::DateTime::from_timestamp(1_000_000_001, 0).unwrap();
        run1.add_result(TestResult::success("test_a"));
        run1.add_result(TestResult::failure("test_b", "boom"));
        repo.insert_test_run(run1).unwrap();

        // After run 1, only test_b should be failing.
        assert_eq!(repo.get_failing_tests().unwrap().len(), 1);

        // Prune run 1; after that the only surviving failing-tests row
        // points to a deleted run and would be a dangling reference. We
        // expect prune to remove those rows.
        drop(repo);
        let cmd = PruneCommand::new(
            Some(path.clone()),
            PruneSelection::Explicit(vec!["1".to_string()]),
            false,
        );
        let mut ui = TestUI::new();
        cmd.execute(&mut ui).unwrap();

        let repo = open_repository(Some(&path)).unwrap();
        let failing_ids: Vec<String> = repo
            .get_failing_tests()
            .unwrap()
            .iter()
            .map(|t| t.as_str().to_string())
            .collect();
        assert!(
            !failing_ids.iter().any(|id| id == "test_b"),
            "test_b's failure should have been pruned along with run 1, got: {:?}",
            failing_ids
        );

        // Flakiness cache should now reflect only run 0: test_a failed once
        // (no transitions), test_b passed once (no failures, filtered out).
        let flakiness = repo.get_flakiness(1).unwrap();
        let test_a = flakiness
            .iter()
            .find(|f| f.test_id.as_str() == "test_a")
            .expect("test_a should still appear in flakiness output");
        assert_eq!(test_a.runs, 1);
        assert_eq!(test_a.failures, 1);
        assert_eq!(test_a.transitions, 0);
        assert!(
            !flakiness.iter().any(|f| f.test_id.as_str() == "test_b"),
            "test_b had no failures after pruning, should be filtered"
        );
    }

    #[test]
    fn prune_removes_run_files() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let mut repo = init_repository(Some(&path)).unwrap();
        add_run(repo.as_mut(), "0", 1_000_000_000);
        add_run(repo.as_mut(), "1", 1_000_000_001);
        drop(repo);

        let runs_dir = temp.path().join(".inquest").join("runs");
        assert!(runs_dir.join("0").exists());
        assert!(runs_dir.join("1").exists());

        let cmd = PruneCommand::new(
            Some(path.clone()),
            PruneSelection::Explicit(vec!["0".to_string()]),
            false,
        );
        let mut ui = TestUI::new();
        cmd.execute(&mut ui).unwrap();

        assert!(
            !runs_dir.join("0").exists(),
            "stream file for run 0 should be gone"
        );
        assert!(
            runs_dir.join("1").exists(),
            "run 1's stream should still be there"
        );
    }

    #[test]
    fn select_helper_drops_dangling_failing_test_id() {
        // Sanity-check: the failing_tests row for test_a (pointing at run 0)
        // is the kind of state that would dangle after prune. Verify
        // get_failing_tests after a prune doesn't return it.
        let temp = TempDir::new().unwrap();
        let path = temp.path().to_string_lossy().to_string();
        let mut repo = init_repository(Some(&path)).unwrap();
        let mut run0 = TestRun::new(RunId::new("0"));
        run0.timestamp = chrono::DateTime::from_timestamp(1_000_000_000, 0).unwrap();
        run0.add_result(TestResult::failure("test_a", "boom"));
        repo.insert_test_run(run0).unwrap();

        drop(repo);
        let cmd = PruneCommand::new(Some(path.clone()), PruneSelection::All, false);
        let mut ui = TestUI::new();
        cmd.execute(&mut ui).unwrap();

        let repo = open_repository(Some(&path)).unwrap();
        assert!(repo.get_failing_tests().unwrap().is_empty());
    }
}
