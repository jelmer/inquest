//! Help command for displaying command documentation

use crate::commands::Command;
use crate::error::Result;
use crate::ui::UI;

pub struct HelpCommand {
    command_name: Option<String>,
}

impl HelpCommand {
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
                    r#"testr init - Initialize a new test repository

Usage: testr init [PATH]

Creates a new test repository in the .testrepository directory.
If PATH is provided, initializes the repository at that location.

Examples:
  testr init              # Initialize in current directory
  testr init /path/to/dir # Initialize at specific path
"#
                }
                "load" => {
                    r#"testr load - Load test results from a subunit stream

Usage: testr load [OPTIONS]

Reads test results from stdin in subunit format and stores them in the repository.

Options:
  --partial    Add/update failing tests without clearing previous failures

Examples:
  python -m subunit.run discover | testr load
  testr load < test_results.subunit
  testr load --partial < new_results.subunit
"#
                }
                "run" => {
                    r#"testr run - Run tests and load results

Usage: testr run [OPTIONS]

Executes the test command from .testr.conf and loads the results.

Options:
  --failing         Only run tests that failed in the last run
  --load-list FILE  Run only tests listed in FILE
  --partial         Keep previous failures and add new ones

Examples:
  testr run
  testr run --failing
  testr run --load-list tests_to_run.txt
"#
                }
                "failing" => {
                    r#"testr failing - Show currently failing tests

Usage: testr failing [OPTIONS]

Lists all tests that failed in the most recent run.

Options:
  --list      Show test IDs only (one per line)
  --subunit   Output in subunit format

Examples:
  testr failing
  testr failing --list
  testr failing --subunit
"#
                }
                "last" => {
                    r#"testr last - Show results from the last test run

Usage: testr last [OPTIONS]

Displays test results from the most recent run.

Options:
  --subunit   Output in subunit format

Examples:
  testr last
  testr last --subunit
"#
                }
                "stats" => {
                    r#"testr stats - Show repository statistics

Usage: testr stats

Displays statistics about the test repository, including total runs,
test counts, and success/failure rates.

Example:
  testr stats
"#
                }
                "slowest" => {
                    r#"testr slowest - Show the slowest tests

Usage: testr slowest [N]

Shows the N slowest tests from the last run (default: 10).

Examples:
  testr slowest
  testr slowest 20
"#
                }
                "list-tests" => {
                    r#"testr list-tests - List available tests

Usage: testr list-tests

Lists all available tests by querying the test command with --list-tests.

Example:
  testr list-tests
"#
                }
                "quickstart" => {
                    r#"testr quickstart - Show quickstart documentation

Usage: testr quickstart

Displays introductory documentation for getting started with testrepository.

Example:
  testr quickstart
"#
                }
                "help" => {
                    r#"testr help - Show help information

Usage: testr help [COMMAND]

Shows general help or help for a specific command.

Examples:
  testr help
  testr help run
"#
                }
                _ => {
                    ui.error(&format!("Unknown command: {}", cmd_name))?;
                    ui.output("Run 'testr help' to see available commands.")?;
                    return Ok(1);
                }
            };
            ui.output(help_text)?;
        } else {
            // Show general help
            let help = r#"testr - Test Repository CLI

Usage: testr <command> [options]

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

Run 'testr help <command>' for more information on a specific command.

Examples:
  testr init
  testr run
  testr failing --list
  testr help run
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
        assert!(output.contains("testr run"));
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
