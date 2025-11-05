//! Test partitioning for parallel execution
//!
//! This module provides functions to partition tests across multiple workers
//! based on their historical durations to balance the load.

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
}
