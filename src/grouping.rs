//! Test grouping based on regex patterns
//!
//! This module provides functionality to group tests together based on regex patterns.
//! Tests in the same group will be scheduled together on the same worker in parallel execution.

use crate::repository::TestId;
use regex::Regex;
use std::collections::HashMap;

/// Group tests by matching a regex pattern
///
/// The regex should contain a named capture group `(?P<group>...)` or use the first
/// capture group as the group name. Tests with the same group value will be grouped together.
///
/// # Examples
///
/// ```
/// use testrepository::grouping::group_tests;
/// use testrepository::repository::TestId;
///
/// let tests = vec![
///     TestId::new("package.module1.TestCase.test_a"),
///     TestId::new("package.module1.TestCase.test_b"),
///     TestId::new("package.module2.TestCase.test_c"),
/// ];
///
/// // Group by module (everything before the last dot)
/// let groups = group_tests(&tests, r"^(.*)\.[^.]+$").unwrap();
///
/// assert_eq!(groups.len(), 2); // Two modules
/// assert_eq!(groups.get("package.module1.TestCase").unwrap().len(), 2);
/// assert_eq!(groups.get("package.module2.TestCase").unwrap().len(), 1);
/// ```
pub fn group_tests(
    tests: &[TestId],
    group_regex: &str,
) -> Result<HashMap<String, Vec<TestId>>, regex::Error> {
    let re = Regex::new(group_regex)?;
    let mut groups: HashMap<String, Vec<TestId>> = HashMap::new();

    for test in tests {
        let test_str = test.as_str();

        // Try to extract group name using the regex
        let group_name = if let Some(captures) = re.captures(test_str) {
            // Try named capture group first
            if let Some(named) = captures.name("group") {
                named.as_str().to_string()
            } else if captures.len() > 1 {
                // Use first capture group
                captures.get(1).unwrap().as_str().to_string()
            } else {
                // No capture group, use the whole match
                captures.get(0).unwrap().as_str().to_string()
            }
        } else {
            // If regex doesn't match, put in a default group
            test_str.to_string()
        };

        groups.entry(group_name).or_default().push(test.clone());
    }

    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group_by_module() {
        let tests = vec![
            TestId::new("package.module1.TestCase.test_a"),
            TestId::new("package.module1.TestCase.test_b"),
            TestId::new("package.module2.TestCase.test_c"),
            TestId::new("package.module2.TestOther.test_d"),
        ];

        // Group by module (everything up to the last dot)
        let groups = group_tests(&tests, r"^(.*)\.[^.]+$").unwrap();

        assert_eq!(groups.len(), 3);
        assert_eq!(groups.get("package.module1.TestCase").unwrap().len(), 2);
        assert_eq!(groups.get("package.module2.TestCase").unwrap().len(), 1);
        assert_eq!(groups.get("package.module2.TestOther").unwrap().len(), 1);
    }

    #[test]
    fn test_group_by_test_class() {
        let tests = vec![
            TestId::new("test.module.TestFoo.test_a"),
            TestId::new("test.module.TestFoo.test_b"),
            TestId::new("test.module.TestBar.test_c"),
        ];

        // Group by test class
        let groups = group_tests(&tests, r"^(.+\.\w+)\.\w+$").unwrap();

        assert_eq!(groups.len(), 2);
        assert_eq!(groups.get("test.module.TestFoo").unwrap().len(), 2);
        assert_eq!(groups.get("test.module.TestBar").unwrap().len(), 1);
    }

    #[test]
    fn test_group_with_named_capture() {
        let tests = vec![
            TestId::new("tests::module1::test_a"),
            TestId::new("tests::module1::test_b"),
            TestId::new("tests::module2::test_c"),
        ];

        // Group by module using named capture
        let groups = group_tests(&tests, r"^tests::(?P<group>\w+)::").unwrap();

        assert_eq!(groups.len(), 2);
        assert_eq!(groups.get("module1").unwrap().len(), 2);
        assert_eq!(groups.get("module2").unwrap().len(), 1);
    }

    #[test]
    fn test_no_match_uses_full_name() {
        let tests = vec![
            TestId::new("test1"),
            TestId::new("test2"),
            TestId::new("other::test3"),
        ];

        // Regex that only matches :: separator
        let groups = group_tests(&tests, r"^(.+)::").unwrap();

        assert_eq!(groups.len(), 3); // Each non-matching test in its own group
        assert_eq!(groups.get("test1").unwrap().len(), 1);
        assert_eq!(groups.get("test2").unwrap().len(), 1);
        assert_eq!(groups.get("other").unwrap().len(), 1);
    }

    #[test]
    fn test_invalid_regex() {
        let tests = vec![TestId::new("test1")];

        let result = group_tests(&tests, r"^(unclosed");
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_tests() {
        let tests: Vec<TestId> = vec![];
        let groups = group_tests(&tests, r"^(.*)$").unwrap();
        assert_eq!(groups.len(), 0);
    }
}
