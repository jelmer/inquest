//! Help command for displaying command documentation

use crate::commands::Command;
use crate::error::Result;
use crate::ui::UI;

/// Command to display help information for commands.
///
/// Shows general help or detailed help for a specific command.
pub struct HelpCommand {
    command_name: Option<String>,
}

impl HelpCommand {
    /// Creates a new help command.
    ///
    /// # Arguments
    /// * `command_name` - Optional name of a specific command to show help for
    pub fn new(command_name: Option<String>) -> Self {
        HelpCommand { command_name }
    }
}

impl Command for HelpCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        if let Some(ref cmd_name) = self.command_name {
            // Show help for specific command
            let help_text = match cmd_name.as_str() {
                "init" => {
                    r#"inq init - Initialize a new test repository

Usage: inq init [PATH]

Creates a new test repository in the .testrepository directory.
If PATH is provided, initializes the repository at that location.

Examples:
  inq init              # Initialize in current directory
  inq init /path/to/dir # Initialize at specific path
"#
                }
                "load" => {
                    r#"inq load - Load test results from a subunit stream

Usage: inq load [OPTIONS]

Reads test results from stdin in subunit format and stores them in the repository.

Options:
  --partial    Add/update failing tests without clearing previous failures

Examples:
  python -m subunit.run discover | inq load
  inq load < test_results.subunit
  inq load --partial < new_results.subunit
"#
                }
                "run" => {
                    r#"inq run - Run tests and load results

Usage: inq run [OPTIONS]

Executes the test command from .testr.conf and loads the results.

Options:
  --failing         Only run tests that failed in the last run
  --load-list FILE  Run only tests listed in FILE
  --partial         Keep previous failures and add new ones

Examples:
  inq run
  inq run --failing
  inq run --load-list tests_to_run.txt
"#
                }
                "failing" => {
                    r#"inq failing - Show currently failing tests

Usage: inq failing [OPTIONS]

Lists all tests that failed in the most recent run.

Options:
  --list      Show test IDs only (one per line)
  --subunit   Output in subunit format

Examples:
  inq failing
  inq failing --list
  inq failing --subunit
"#
                }
                "last" => {
                    r#"inq last - Show results from the last test run

Usage: inq last [OPTIONS]

Displays test results from the most recent run.

Options:
  --subunit   Output in subunit format

Examples:
  inq last
  inq last --subunit
"#
                }
                "stats" => {
                    r#"inq stats - Show repository statistics

Usage: inq stats

Displays statistics about the test repository, including total runs,
test counts, and success/failure rates.

Example:
  inq stats
"#
                }
                "slowest" => {
                    r#"inq slowest - Show the slowest tests

Usage: inq slowest [N]

Shows the N slowest tests from the last run (default: 10).

Examples:
  inq slowest
  inq slowest 20
"#
                }
                "list-tests" => {
                    r#"inq list-tests - List available tests

Usage: inq list-tests

Lists all available tests by querying the test command with --list-tests.

Example:
  inq list-tests
"#
                }
                "quickstart" => {
                    r#"inq quickstart - Show quickstart documentation

Usage: inq quickstart

Displays introductory documentation for getting started with inquest.

Example:
  inq quickstart
"#
                }
                "help" => {
                    r#"inq help - Show help information

Usage: inq help [COMMAND]

Shows general help or help for a specific command.

Examples:
  inq help
  inq help run
"#
                }
                _ => {
                    ui.error(&format!("Unknown command: {}", cmd_name))?;
                    ui.output("Run 'inq help' to see available commands.")?;
                    return Ok(1);
                }
            };
            ui.output(help_text)?;
        } else {
            // Show general help
            let help = r#"inq - Test Repository CLI

Usage: inq <command> [options]

Available commands:
  init          Initialize a new test repository
  load          Load test results from a subunit stream
  run           Run tests and load results
  failing       Show currently failing tests
  last          Show results from the last test run
  stats         Show repository statistics
  slowest       Show the slowest tests
  list-tests    List available tests
  quickstart    Show quickstart documentation
  help          Show this help message

Run 'inq help <command>' for more information on a specific command.

Examples:
  inq init
  inq run
  inq failing --list
  inq help run
"#;
            ui.output(help)?;
        }
        Ok(0)
    }

    fn name(&self) -> &str {
        "help"
    }

    fn help(&self) -> &str {
        "Show help information for commands"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_ui::TestUI;

    #[test]
    fn test_help_command_general() {
        let mut ui = TestUI::new();
        let cmd = HelpCommand::new(None);
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert!(!ui.output.is_empty());
        let output = ui.output.join("\n");
        assert!(output.contains("Available commands:"));
        assert!(output.contains("init"));
        assert!(output.contains("run"));
    }

    #[test]
    fn test_help_command_specific() {
        let mut ui = TestUI::new();
        let cmd = HelpCommand::new(Some("run".to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert!(!ui.output.is_empty());
        let output = ui.output.join("\n");
        assert!(output.contains("inq run"));
        assert!(output.contains("--failing"));
    }

    #[test]
    fn test_help_command_unknown() {
        let mut ui = TestUI::new();
        let cmd = HelpCommand::new(Some("nonexistent".to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 1);
        assert!(!ui.errors.is_empty());
        assert!(ui.errors[0].contains("Unknown command"));
    }
}
