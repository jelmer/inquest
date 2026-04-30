//! Print the test IDs assigned to one shard of a balanced split.
//!
//! `inq shard N/M` discovers the suite, partitions it across M shards using
//! the same duration-aware greedy algorithm as parallel runs, and prints the
//! IDs for shard N. Distributed CI nodes call this once each to claim a
//! disjoint, load-balanced slice of the suite. The historical durations
//! recorded in the repository are used when available; without history, the
//! split degrades to round-robin.

use crate::commands::utils::open_repository;
use crate::commands::Command;
use crate::error::{Error, Result};
use crate::partition::partition_tests_with_grouping;
use crate::repository::TestId;
use crate::testcommand::TestCommand;
use crate::ui::UI;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// Print the test IDs assigned to one shard of an `M`-way balanced split.
///
/// The partition is deterministic given the same suite and history, so two
/// nodes calling `inq shard 1/4` and `inq shard 2/4` produce disjoint shards
/// whose union is the full suite.
pub struct ShardCommand {
    base_path: Option<String>,
    /// 0-based shard index (resolved from the user-supplied `N/M`).
    shard: usize,
    /// Total number of shards.
    total: usize,
    /// Override for the config's `group_regex`. Empty string disables grouping.
    group_regex: Option<String>,
}

impl ShardCommand {
    /// Build a shard command from a parsed shard index, total count, and an
    /// optional group-regex override.
    pub fn new(
        base_path: Option<String>,
        shard: usize,
        total: usize,
        group_regex: Option<String>,
    ) -> Self {
        ShardCommand {
            base_path,
            shard,
            total,
            group_regex,
        }
    }

    /// Parse the `N/M` spec. Accepts 1-based input by default; with
    /// `zero_indexed = true` the first shard is `0/M`.
    ///
    /// Returns the 0-based shard index along with the total.
    pub fn parse_spec(spec: &str, zero_indexed: bool) -> Result<(usize, usize)> {
        let (n_str, m_str) = spec.split_once('/').ok_or_else(|| {
            Error::Config(format!(
                "invalid shard spec '{}': expected N/M (e.g. 1/4)",
                spec
            ))
        })?;
        let n: usize = n_str.trim().parse().map_err(|_| {
            Error::Config(format!(
                "invalid shard spec '{}': N must be a non-negative integer",
                spec
            ))
        })?;
        let m: usize = m_str.trim().parse().map_err(|_| {
            Error::Config(format!(
                "invalid shard spec '{}': M must be a non-negative integer",
                spec
            ))
        })?;
        if m == 0 {
            return Err(Error::Config(format!(
                "invalid shard spec '{}': M must be at least 1",
                spec
            )));
        }
        let index = if zero_indexed {
            if n >= m {
                return Err(Error::Config(format!(
                    "invalid shard spec '{}': N must be in 0..{} when --zero-indexed",
                    spec, m
                )));
            }
            n
        } else {
            if n == 0 || n > m {
                return Err(Error::Config(format!(
                    "invalid shard spec '{}': N must be in 1..={}",
                    spec, m
                )));
            }
            n - 1
        };
        Ok((index, m))
    }
}

/// Compute the test IDs assigned to one shard of an `M`-way balanced split.
///
/// Sorts `test_ids` deterministically before partitioning so distributed
/// callers see the same layout. Returns an error for an out-of-range shard
/// index or an invalid `group_regex`.
pub fn compute_shard(
    test_ids: &[TestId],
    durations: &HashMap<TestId, Duration>,
    shard: usize,
    total: usize,
    group_regex: Option<&str>,
) -> Result<Vec<TestId>> {
    let mut sorted = test_ids.to_vec();
    sorted.sort();
    let partitions = partition_tests_with_grouping(&sorted, durations, total, group_regex)
        .map_err(|e| Error::Config(format!("invalid group_regex: {}", e)))?;
    partitions
        .into_iter()
        .nth(shard)
        .ok_or_else(|| Error::Config(format!("shard index {} out of range", shard)))
}

impl Command for ShardCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = self
            .base_path
            .as_deref()
            .map_or_else(|| Path::new("."), Path::new);

        let test_cmd = TestCommand::from_directory(base)?;
        let test_ids = test_cmd.list_tests()?;

        // Pull historical durations if a repository exists. Sharding works
        // without one — the partition just degrades to round-robin.
        let durations: HashMap<TestId, Duration> = open_repository(self.base_path.as_deref())
            .ok()
            .and_then(|repo| repo.get_test_times().ok())
            .unwrap_or_default();

        // CLI override (including empty string to disable) wins over config.
        let configured_group_regex = test_cmd.config().group_regex.clone();
        let group_regex: Option<String> = match &self.group_regex {
            Some(r) if r.is_empty() => None,
            Some(r) => Some(r.clone()),
            None => configured_group_regex,
        };

        let shard = compute_shard(
            &test_ids,
            &durations,
            self.shard,
            self.total,
            group_regex.as_deref(),
        )?;

        for id in shard {
            ui.output(id.as_str())?;
        }
        Ok(0)
    }

    fn name(&self) -> &str {
        "shard"
    }

    fn help(&self) -> &str {
        "Print the test IDs assigned to one shard of a balanced split"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec_one_based() {
        assert_eq!(ShardCommand::parse_spec("1/4", false).unwrap(), (0, 4));
        assert_eq!(ShardCommand::parse_spec("4/4", false).unwrap(), (3, 4));
    }

    #[test]
    fn parse_spec_zero_based() {
        assert_eq!(ShardCommand::parse_spec("0/4", true).unwrap(), (0, 4));
        assert_eq!(ShardCommand::parse_spec("3/4", true).unwrap(), (3, 4));
    }

    #[test]
    fn parse_spec_rejects_zero_when_one_based() {
        assert!(ShardCommand::parse_spec("0/4", false).is_err());
    }

    #[test]
    fn parse_spec_rejects_index_at_or_above_total_when_zero_based() {
        assert!(ShardCommand::parse_spec("4/4", true).is_err());
    }

    #[test]
    fn parse_spec_rejects_index_above_total_one_based() {
        assert!(ShardCommand::parse_spec("5/4", false).is_err());
    }

    #[test]
    fn parse_spec_rejects_zero_total() {
        assert!(ShardCommand::parse_spec("1/0", false).is_err());
        assert!(ShardCommand::parse_spec("0/0", true).is_err());
    }

    #[test]
    fn parse_spec_rejects_missing_slash() {
        assert!(ShardCommand::parse_spec("4", false).is_err());
    }

    #[test]
    fn parse_spec_rejects_non_numeric() {
        assert!(ShardCommand::parse_spec("a/4", false).is_err());
        assert!(ShardCommand::parse_spec("1/b", false).is_err());
    }

    fn make_tests(ids: &[&str]) -> Vec<TestId> {
        ids.iter().map(|s| TestId::new(*s)).collect()
    }

    #[test]
    fn compute_shard_union_covers_all_tests_no_duplicates() {
        let tests = make_tests(&[
            "a::t1", "a::t2", "a::t3", "b::t1", "b::t2", "c::t1", "d::t1", "e::t1",
        ]);
        let durations = HashMap::new();
        let total = 3;

        let mut union: Vec<TestId> = Vec::new();
        for shard in 0..total {
            let part = compute_shard(&tests, &durations, shard, total, None).unwrap();
            union.extend(part);
        }
        union.sort();

        let mut expected = tests.clone();
        expected.sort();
        assert_eq!(union, expected);
    }

    #[test]
    fn compute_shard_is_deterministic_under_input_reordering() {
        // The same suite given in two different orders should produce the
        // same shard for any given index. This is what makes `inq shard`
        // safe to call from CI nodes whose discovery order may vary.
        let tests_a = make_tests(&["alpha::a", "beta::b", "gamma::c", "delta::d"]);
        let tests_b = make_tests(&["delta::d", "gamma::c", "beta::b", "alpha::a"]);
        let durations = HashMap::new();

        for shard in 0..2 {
            let part_a = compute_shard(&tests_a, &durations, shard, 2, None).unwrap();
            let part_b = compute_shard(&tests_b, &durations, shard, 2, None).unwrap();
            assert_eq!(part_a, part_b);
        }
    }

    #[test]
    fn compute_shard_balances_by_duration() {
        let tests = make_tests(&["fast1", "fast2", "slow1", "slow2"]);
        let mut durations = HashMap::new();
        durations.insert(TestId::new("fast1"), Duration::from_millis(100));
        durations.insert(TestId::new("fast2"), Duration::from_millis(100));
        durations.insert(TestId::new("slow1"), Duration::from_secs(5));
        durations.insert(TestId::new("slow2"), Duration::from_secs(5));

        let s0 = compute_shard(&tests, &durations, 0, 2, None).unwrap();
        let s1 = compute_shard(&tests, &durations, 1, 2, None).unwrap();

        // Each shard ends up with exactly one slow + one fast, not two slows.
        let dur =
            |part: &[TestId]| -> Duration { part.iter().filter_map(|id| durations.get(id)).sum() };
        assert!(dur(&s0).abs_diff(dur(&s1)) < Duration::from_millis(500));
    }

    #[test]
    fn compute_shard_respects_group_regex() {
        let tests = make_tests(&[
            "modA::test_a",
            "modA::test_b",
            "modA::test_c",
            "modB::test_d",
        ]);
        let durations = HashMap::new();
        let group = Some(r"^([^:]+)::");

        let s0 = compute_shard(&tests, &durations, 0, 2, group).unwrap();
        let s1 = compute_shard(&tests, &durations, 1, 2, group).unwrap();

        // Same module's tests must not be split across shards.
        let module_of = |t: &TestId| t.as_str().split("::").next().unwrap().to_string();
        let modules_in = |part: &[TestId]| -> std::collections::HashSet<String> {
            part.iter().map(module_of).collect()
        };
        let m0 = modules_in(&s0);
        let m1 = modules_in(&s1);
        assert!(
            m0.is_disjoint(&m1),
            "shards split a module: {:?} vs {:?}",
            m0,
            m1
        );
    }

    #[test]
    fn compute_shard_out_of_range_errors() {
        let tests = make_tests(&["a", "b", "c"]);
        let durations = HashMap::new();
        assert!(compute_shard(&tests, &durations, 4, 4, None).is_err());
    }

    #[test]
    fn compute_shard_invalid_regex_errors() {
        let tests = make_tests(&["a", "b"]);
        let durations = HashMap::new();
        assert!(compute_shard(&tests, &durations, 0, 2, Some("^(unclosed")).is_err());
    }
}
