//! Handling of lists of tests - common code to --load-list etc.

use crate::repository::TestId;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

/// Parse a test list file into a vector of test IDs
///
/// The file should contain one test ID per line. Empty lines and leading/trailing
/// whitespace are ignored.
pub fn parse_list_file(path: &Path) -> io::Result<Vec<TestId>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);

    let mut test_ids = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            test_ids.push(TestId::new(trimmed));
        }
    }

    Ok(test_ids)
}

/// Parse a test list from a string
pub fn parse_list(content: &str) -> Vec<TestId> {
    content
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .map(TestId::new)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_list() {
        let content = "test1\ntest2\n\ntest3\n  test4  \n";
        let ids = parse_list(content);

        assert_eq!(ids.len(), 4);
        assert_eq!(ids[0].as_str(), "test1");
        assert_eq!(ids[1].as_str(), "test2");
        assert_eq!(ids[2].as_str(), "test3");
        assert_eq!(ids[3].as_str(), "test4");
    }

    #[test]
    fn test_parse_list_file() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("testlist.txt");

        fs::write(&file_path, "test.module.Test1\ntest.module.Test2\n").unwrap();

        let ids = parse_list_file(&file_path).unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0].as_str(), "test.module.Test1");
        assert_eq!(ids[1].as_str(), "test.module.Test2");
    }

    #[test]
    fn test_parse_list_file_not_found() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("nonexistent.txt");

        let result = parse_list_file(&file_path);
        assert!(result.is_err());
    }
}
