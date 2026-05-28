//! List available tests

use crate::commands::Command;
use crate::error::Result;
use crate::testcommand::TestCommand;
use crate::ui::UI;
use std::path::Path;

/// Command to list all available tests.
///
/// Queries the test command to discover all available tests
/// in the test suite.
pub struct ListTestsCommand {
    base_path: Option<String>,
}

impl ListTestsCommand {
    /// Creates a new list-tests command.
    ///
    /// # Arguments
    /// * `base_path` - Optional base directory path for the repository
    pub fn new(base_path: Option<String>) -> Self {
        ListTestsCommand { base_path }
    }
}

impl Command for ListTestsCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = self
            .base_path
            .as_deref()
            .map_or_else(|| Path::new("."), Path::new);

        let test_cmd = TestCommand::from_directory(base)?;

        match test_cmd.list_tests() {
            Ok(test_ids) => {
                if test_ids.is_empty() {
                    ui.output("No tests found")?;
                } else {
                    for line in
                        format_test_list(&test_ids, test_cmd.config().group_regex.as_deref())
                    {
                        ui.output(&line)?;
                    }
                }
                Ok(0)
            }
            Err(e) => {
                ui.error(&format!("Failed to list tests: {}", e))?;
                Ok(1)
            }
        }
    }

    fn name(&self) -> &str {
        "list-tests"
    }

    fn help(&self) -> &str {
        "List all available tests"
    }
}

/// Render the test list for display, dropping a common group prefix when one
/// exists. When a prefix is dropped, the first line is an informational
/// `# common prefix: <prefix>` banner so the full IDs remain recoverable.
fn format_test_list(
    test_ids: &[crate::repository::TestId],
    group_regex: Option<&str>,
) -> Vec<String> {
    let prefix = crate::grouping::common_group_prefix(test_ids, group_regex);
    let mut lines = Vec::with_capacity(test_ids.len() + 1);
    match prefix {
        Some(ref p) => {
            lines.push(format!("# common prefix: {}", p));
            lines.extend(
                test_ids
                    .iter()
                    .map(|id| crate::grouping::strip_prefix(id.as_str(), p).to_string()),
            );
        }
        None => lines.extend(test_ids.iter().map(|id| id.as_str().to_string())),
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_ui::TestUI;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_list_tests_command_no_config() {
        let temp = TempDir::new().unwrap();

        let mut ui = TestUI::new();
        let cmd = ListTestsCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        // Should return an error because there's no config file
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::Error::Config(msg) => {
                assert!(
                    msg.contains("No configuration file found"),
                    "unexpected error: {}",
                    msg
                );
            }
            e => panic!("Expected Config error, got: {}", e),
        }
    }

    #[test]
    fn test_list_tests_command_with_config() {
        let temp = TempDir::new().unwrap();

        // Create a .testr.conf that lists some tests
        let config = r#"
[DEFAULT]
test_command=echo "test1\ntest2\ntest3" $LISTOPT
test_list_option=
"#;
        fs::write(temp.path().join(".testr.conf"), config).unwrap();

        let mut ui = TestUI::new();
        let cmd = ListTestsCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        // The echo command should output test1, test2, test3
        assert!(!ui.output.is_empty());
    }

    #[test]
    fn format_test_list_strips_common_prefix() {
        use crate::repository::TestId;
        let ids = vec![TestId::new("a::b::test_x"), TestId::new("a::b::test_y")];
        assert_eq!(
            format_test_list(&ids, Some(r"^(.*)::[^:]+$")),
            vec![
                "# common prefix: a::b::".to_string(),
                "test_x".to_string(),
                "test_y".to_string(),
            ]
        );
    }

    #[test]
    fn format_test_list_keeps_full_ids_when_no_common_prefix() {
        use crate::repository::TestId;
        let ids = vec![TestId::new("a::b::test_x"), TestId::new("a::c::test_y")];
        assert_eq!(
            format_test_list(&ids, Some(r"^(.*)::[^:]+$")),
            vec!["a::b::test_x".to_string(), "a::c::test_y".to_string()]
        );
    }

    #[test]
    fn format_test_list_no_group_regex_keeps_full_ids() {
        use crate::repository::TestId;
        let ids = vec![TestId::new("a::b::test_x"), TestId::new("a::b::test_y")];
        assert_eq!(
            format_test_list(&ids, None),
            vec!["a::b::test_x".to_string(), "a::b::test_y".to_string()]
        );
    }
}
