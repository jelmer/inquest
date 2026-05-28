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
/// use inquest::grouping::group_tests;
/// use inquest::repository::TestId;
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

/// Compute the common, separator-aligned prefix shared by all tests, derived
/// from the grouping config. Returns the prefix *including* its trailing
/// separator (e.g. `"mycrate::foo::"` or `"package.module."`), or `None` if
/// there is no single droppable prefix.
///
/// The prefix is only returned when the tests fall into exactly one group whose
/// name is a strict, separator-aligned prefix of every test ID. This makes the
/// result safe to strip from display and to re-prepend to user input.
pub fn common_group_prefix(tests: &[TestId], group_regex: Option<&str>) -> Option<String> {
    let regex = group_regex?;
    if tests.len() < 2 {
        return None;
    }

    let groups = group_tests(tests, regex).ok()?;
    if groups.len() != 1 {
        return None;
    }
    let group_name = groups.into_keys().next()?;
    if group_name.is_empty() {
        return None;
    }

    let separator = detect_separator(tests, &group_name)?;
    let prefix = format!("{group_name}{separator}");

    // Every test must be the prefix followed by a non-empty remainder.
    if tests
        .iter()
        .all(|t| t.as_str().len() > prefix.len() && t.as_str().starts_with(&prefix))
    {
        Some(prefix)
    } else {
        None
    }
}

/// Infer the path separator that follows `group_name` in every test ID. Returns
/// `"::"` (Rust) or `"."` (Python/Go) when uniform, else `None`.
fn detect_separator(tests: &[TestId], group_name: &str) -> Option<&'static str> {
    ["::", "."].into_iter().find(|sep| {
        tests.iter().all(|t| {
            t.as_str()
                .strip_prefix(group_name)
                .is_some_and(|rest| rest.starts_with(sep))
        })
    })
}

/// Return `id` with `prefix` removed if it starts with it, otherwise `id`.
pub fn strip_prefix<'a>(id: &'a str, prefix: &str) -> &'a str {
    id.strip_prefix(prefix).unwrap_or(id)
}

/// Return `prefix + input` unless `input` already starts with `prefix`.
pub fn apply_prefix(input: &str, prefix: &str) -> String {
    if input.starts_with(prefix) {
        input.to_string()
    } else {
        format!("{prefix}{input}")
    }
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

    #[test]
    fn test_no_capture_group_uses_whole_match() {
        // A regex with no capture group at all groups by the whole match.
        let tests = vec![
            TestId::new("package.module1.test_a"),
            TestId::new("package.module2.test_b"),
            TestId::new("other.module.test_c"),
        ];

        // Matches the leading package segment; no parentheses, so the group
        // name is the matched text itself.
        let groups = group_tests(&tests, r"^[a-z]+").unwrap();

        assert_eq!(groups.len(), 2);
        assert_eq!(groups.get("package").unwrap().len(), 2);
        assert_eq!(groups.get("other").unwrap().len(), 1);
    }

    const RUST_REGEX: &str = r"^(.*)::[^:]+$";
    const PY_REGEX: &str = r"^(.*)\.[^.]+$";

    #[test]
    fn common_prefix_rust_double_colon() {
        let tests = vec![
            TestId::new("a::b::test_x"),
            TestId::new("a::b::test_y"),
            TestId::new("a::b::test_z"),
        ];
        assert_eq!(
            common_group_prefix(&tests, Some(RUST_REGEX)),
            Some("a::b::".to_string())
        );
    }

    #[test]
    fn common_prefix_python_dot() {
        let tests = vec![TestId::new("pkg.mod.test_a"), TestId::new("pkg.mod.test_b")];
        assert_eq!(
            common_group_prefix(&tests, Some(PY_REGEX)),
            Some("pkg.mod.".to_string())
        );
    }

    #[test]
    fn common_prefix_multiple_groups_none() {
        let tests = vec![TestId::new("a::b::test_x"), TestId::new("a::c::test_y")];
        assert_eq!(common_group_prefix(&tests, Some(RUST_REGEX)), None);
    }

    #[test]
    fn common_prefix_single_test_none() {
        let tests = vec![TestId::new("a::b::test_x")];
        assert_eq!(common_group_prefix(&tests, Some(RUST_REGEX)), None);
    }

    #[test]
    fn common_prefix_zero_tests_none() {
        let tests: Vec<TestId> = vec![];
        assert_eq!(common_group_prefix(&tests, Some(RUST_REGEX)), None);
    }

    #[test]
    fn common_prefix_no_regex_none() {
        let tests = vec![TestId::new("a::b::test_x"), TestId::new("a::b::test_y")];
        assert_eq!(common_group_prefix(&tests, None), None);
    }

    #[test]
    fn common_prefix_bad_regex_none() {
        let tests = vec![TestId::new("a::b::test_x"), TestId::new("a::b::test_y")];
        assert_eq!(common_group_prefix(&tests, Some(r"^(unclosed")), None);
    }

    #[test]
    fn common_prefix_separator_mismatch_none() {
        // One group name, but the separator after it differs between IDs, so no
        // uniform separator can be detected.
        let tests = vec![TestId::new("a::b::test_x"), TestId::new("a::b.test_y")];
        assert_eq!(common_group_prefix(&tests, Some(r"^(a::b).*$")), None);
    }

    #[test]
    fn common_prefix_group_name_not_literal_prefix_none() {
        // A regex whose captured group is not a literal prefix of the IDs.
        let tests = vec![TestId::new("a::b::test_x"), TestId::new("a::b::test_y")];
        assert_eq!(common_group_prefix(&tests, Some(r"^.*::(test_\w+)$")), None);
    }

    #[test]
    fn strip_prefix_removes_when_present() {
        assert_eq!(strip_prefix("a::b::test_x", "a::b::"), "test_x");
        assert_eq!(strip_prefix("other::test_x", "a::b::"), "other::test_x");
    }

    #[test]
    fn apply_prefix_prepends_when_absent() {
        assert_eq!(apply_prefix("test_x", "a::b::"), "a::b::test_x");
        assert_eq!(apply_prefix("a::b::test_x", "a::b::"), "a::b::test_x");
    }
}
