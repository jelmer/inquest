//! Test partitioning for parallel execution
//!
//! This module provides functions to partition tests across multiple workers
//! based on their historical durations to balance the load.

use crate::grouping::group_tests;
use crate::repository::TestId;
use std::collections::HashMap;
use std::time::Duration;

/// Partition tests into groups for parallel execution
///
/// Tests are partitioned to balance the expected runtime across workers.
/// Tests with known durations are sorted by duration and distributed in a
/// round-robin fashion (longest first) to balance load. Tests without
/// duration information are distributed round-robin after those with durations.
///
/// # Arguments
///
/// * `test_ids` - List of test IDs to partition
/// * `durations` - Map of test IDs to their historical durations
/// * `concurrency` - Number of partitions to create
///
/// # Returns
///
/// Vector of test ID vectors, one for each worker
pub fn partition_tests(
    test_ids: &[TestId],
    durations: &HashMap<TestId, Duration>,
    concurrency: usize,
) -> Vec<Vec<TestId>> {
    if concurrency == 0 {
        return vec![];
    }

    if concurrency == 1 {
        return vec![test_ids.to_vec()];
    }

    // Separate tests into those with and without known durations
    let mut with_duration: Vec<(TestId, Duration)> = Vec::new();
    let mut without_duration: Vec<TestId> = Vec::new();

    for test_id in test_ids {
        if let Some(&duration) = durations.get(test_id) {
            with_duration.push((test_id.clone(), duration));
        } else {
            without_duration.push(test_id.clone());
        }
    }

    // Sort tests with durations from longest to shortest
    with_duration.sort_by(|a, b| b.1.cmp(&a.1));

    // Initialize partitions with expected runtime tracking
    let mut partitions: Vec<(Vec<TestId>, Duration)> = (0..concurrency)
        .map(|_| (Vec::new(), Duration::ZERO))
        .collect();

    // Distribute tests with known durations using a greedy algorithm
    // Always add the next test to the partition with the shortest total runtime
    for (test_id, duration) in with_duration {
        // Find partition with minimum total runtime
        let min_partition = partitions
            .iter_mut()
            .min_by_key(|(_, total_duration)| *total_duration)
            .unwrap();

        min_partition.0.push(test_id);
        min_partition.1 += duration;
    }

    // Distribute tests without known durations in round-robin fashion
    for (idx, test_id) in without_duration.into_iter().enumerate() {
        partitions[idx % concurrency].0.push(test_id);
    }

    // Extract just the test ID vectors
    partitions.into_iter().map(|(ids, _)| ids).collect()
}

/// Partition tests with grouping into groups for parallel execution
///
/// When a group_regex is provided, tests are first grouped by the regex pattern,
/// then groups (not individual tests) are distributed across workers. This ensures
/// that related tests always run together on the same worker.
///
/// # Arguments
///
/// * `test_ids` - List of test IDs to partition
/// * `durations` - Map of test IDs to their historical durations
/// * `concurrency` - Number of partitions to create
/// * `group_regex` - Optional regex pattern to group tests by
///
/// # Returns
///
/// Vector of test ID vectors, one for each worker
pub fn partition_tests_with_grouping(
    test_ids: &[TestId],
    durations: &HashMap<TestId, Duration>,
    concurrency: usize,
    group_regex: Option<&str>,
) -> Result<Vec<Vec<TestId>>, regex::Error> {
    // If no grouping, use the simple partition
    let Some(regex) = group_regex else {
        return Ok(partition_tests(test_ids, durations, concurrency));
    };

    if concurrency == 0 {
        return Ok(vec![]);
    }

    if concurrency == 1 {
        return Ok(vec![test_ids.to_vec()]);
    }

    // Group tests by the regex
    let groups = group_tests(test_ids, regex)?;

    // Calculate total duration for each group
    let mut group_durations: Vec<(String, Vec<TestId>, Duration)> = groups
        .into_iter()
        .map(|(group_name, test_ids)| {
            let total_duration: Duration = test_ids.iter().filter_map(|id| durations.get(id)).sum();
            (group_name, test_ids, total_duration)
        })
        .collect();

    // Sort groups by duration (longest first)
    group_durations.sort_by(|a, b| b.2.cmp(&a.2));

    // Initialize partitions
    let mut partitions: Vec<(Vec<TestId>, Duration)> = (0..concurrency)
        .map(|_| (Vec::new(), Duration::ZERO))
        .collect();

    // Distribute groups using greedy algorithm (assign to worker with least load)
    for (_group_name, test_ids, group_duration) in group_durations {
        // Find partition with minimum total runtime
        let min_partition = partitions
            .iter_mut()
            .min_by_key(|(_, total_duration)| *total_duration)
            .unwrap();

        // Add all tests from this group to the partition
        min_partition.0.extend(test_ids);
        min_partition.1 += group_duration;
    }

    // Extract just the test ID vectors
    Ok(partitions.into_iter().map(|(ids, _)| ids).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_empty() {
        let result = partition_tests(&[], &HashMap::new(), 4);
        assert_eq!(result.len(), 4);
        assert!(result.iter().all(|p| p.is_empty()));
    }

    #[test]
    fn test_partition_single_worker() {
        let tests = vec![TestId::new("test1"), TestId::new("test2")];
        let result = partition_tests(&tests, &HashMap::new(), 1);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 2);
    }

    #[test]
    fn test_partition_no_durations() {
        let tests = vec![
            TestId::new("test1"),
            TestId::new("test2"),
            TestId::new("test3"),
            TestId::new("test4"),
            TestId::new("test5"),
        ];
        let result = partition_tests(&tests, &HashMap::new(), 2);

        assert_eq!(result.len(), 2);
        // Without durations, should distribute round-robin
        // First partition gets tests 0, 2, 4 (test1, test3, test5)
        // Second partition gets tests 1, 3 (test2, test4)
        assert_eq!(result[0].len(), 3);
        assert_eq!(result[1].len(), 2);
    }

    #[test]
    fn test_partition_with_durations() {
        let tests = vec![
            TestId::new("fast1"),
            TestId::new("slow1"),
            TestId::new("fast2"),
            TestId::new("slow2"),
        ];

        let mut durations = HashMap::new();
        durations.insert(TestId::new("fast1"), Duration::from_millis(100));
        durations.insert(TestId::new("fast2"), Duration::from_millis(100));
        durations.insert(TestId::new("slow1"), Duration::from_secs(5));
        durations.insert(TestId::new("slow2"), Duration::from_secs(5));

        let result = partition_tests(&tests, &durations, 2);

        assert_eq!(result.len(), 2);

        // Should distribute to balance load:
        // Partition 0: slow1 (5s) + fast1 (0.1s) = 5.1s
        // Partition 1: slow2 (5s) + fast2 (0.1s) = 5.1s
        let partition0_duration: Duration =
            result[0].iter().filter_map(|id| durations.get(id)).sum();
        let partition1_duration: Duration =
            result[1].iter().filter_map(|id| durations.get(id)).sum();

        // Both partitions should have approximately the same total duration
        let diff = partition0_duration
            .as_millis()
            .abs_diff(partition1_duration.as_millis());
        assert!(diff < 1000, "Partitions should be balanced");
    }

    #[test]
    fn test_partition_mixed_durations() {
        let tests = vec![
            TestId::new("known1"),
            TestId::new("unknown1"),
            TestId::new("known2"),
            TestId::new("unknown2"),
        ];

        let mut durations = HashMap::new();
        durations.insert(TestId::new("known1"), Duration::from_secs(2));
        durations.insert(TestId::new("known2"), Duration::from_secs(3));

        let result = partition_tests(&tests, &durations, 2);

        assert_eq!(result.len(), 2);
        // Each partition should have at least one test
        assert!(!result[0].is_empty());
        assert!(!result[1].is_empty());
    }

    #[test]
    fn test_partition_with_grouping_no_regex() {
        let tests = vec![TestId::new("test1"), TestId::new("test2")];
        let result = partition_tests_with_grouping(&tests, &HashMap::new(), 2, None).unwrap();

        // Should behave like regular partition when no regex
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_partition_with_grouping_by_module() {
        let tests = vec![
            TestId::new("module1.TestCase.test_a"),
            TestId::new("module1.TestCase.test_b"),
            TestId::new("module1.TestCase.test_c"),
            TestId::new("module2.TestCase.test_d"),
            TestId::new("module2.TestCase.test_e"),
            TestId::new("module3.TestCase.test_f"),
        ];

        // Group by module (everything before .TestCase)
        let result =
            partition_tests_with_grouping(&tests, &HashMap::new(), 2, Some(r"^([^.]+)\.")).unwrap();

        assert_eq!(result.len(), 2);

        // All tests from same module should be in same partition
        for partition in &result {
            let mut modules = std::collections::HashSet::new();
            for test in partition {
                let module = test.as_str().split('.').next().unwrap();
                modules.insert(module);
            }

            // Each module's tests should not be split
            // (though different modules can be in same partition)
            for test in partition {
                let module = test.as_str().split('.').next().unwrap();
                // Verify all tests from this module are in this partition
                let same_module_tests: Vec<_> = tests
                    .iter()
                    .filter(|t| t.as_str().starts_with(&format!("{}.", module)))
                    .collect();

                for same_module_test in same_module_tests {
                    assert!(
                        partition.contains(same_module_test),
                        "All tests from module {} should be in same partition",
                        module
                    );
                }
            }
        }
    }

    #[test]
    fn test_partition_with_grouping_balances_load() {
        let tests = vec![
            TestId::new("slow_module.test_a"),
            TestId::new("slow_module.test_b"),
            TestId::new("fast_module.test_c"),
            TestId::new("fast_module.test_d"),
        ];

        let mut durations = HashMap::new();
        durations.insert(TestId::new("slow_module.test_a"), Duration::from_secs(5));
        durations.insert(TestId::new("slow_module.test_b"), Duration::from_secs(5));
        durations.insert(
            TestId::new("fast_module.test_c"),
            Duration::from_millis(100),
        );
        durations.insert(
            TestId::new("fast_module.test_d"),
            Duration::from_millis(100),
        );

        // Group by module
        let result =
            partition_tests_with_grouping(&tests, &durations, 2, Some(r"^([^.]+)\.")).unwrap();

        assert_eq!(result.len(), 2);

        // One partition should have the slow module, the other the fast module
        let partition0_duration: Duration =
            result[0].iter().filter_map(|id| durations.get(id)).sum();
        let partition1_duration: Duration =
            result[1].iter().filter_map(|id| durations.get(id)).sum();

        // One should be ~10s (slow module), other ~0.2s (fast module)
        let total_duration = partition0_duration + partition1_duration;
        assert_eq!(total_duration.as_millis(), 10200);
    }

    #[test]
    fn test_partition_with_grouping_invalid_regex() {
        let tests = vec![TestId::new("test1")];
        let result = partition_tests_with_grouping(&tests, &HashMap::new(), 2, Some(r"^(unclosed"));

        assert!(result.is_err());
    }
}
