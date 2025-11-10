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
            .map(Path::new)
            .unwrap_or_else(|| Path::new("."));

        let test_cmd = TestCommand::from_directory(base)?;

        match test_cmd.list_tests() {
            Ok(test_ids) => {
                if test_ids.is_empty() {
                    ui.output("No tests found")?;
                } else {
                    for test_id in test_ids {
                        ui.output(test_id.as_str())?;
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

        // Should return an error because there's no .testr.conf
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::Error::Config(msg) => {
                assert_eq!(msg, "No .testr.conf file found");
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
}
