//! User interface abstraction
//!
//! This module provides the UI trait for command input/output.

use crate::error::Result;
use std::io::{self, Write};

#[cfg(test)]
pub mod test_ui;

/// Abstract UI trait for command interaction
pub trait UI {
    /// Output a message to the user
    fn output(&mut self, message: &str) -> Result<()>;

    /// Output an error message
    fn error(&mut self, message: &str) -> Result<()>;

    /// Output a warning message
    fn warning(&mut self, message: &str) -> Result<()>;

    /// Output raw bytes (e.g., for subunit streams)
    fn output_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        // Default implementation: write to stdout
        io::stdout().write_all(bytes)?;
        Ok(())
    }
}

/// Command-line UI implementation
pub struct CliUI {
    stdout: Box<dyn Write>,
    stderr: Box<dyn Write>,
}

impl CliUI {
    /// Creates a new command-line UI instance using stdout and stderr.
    pub fn new() -> Self {
        CliUI {
            stdout: Box::new(io::stdout()),
            stderr: Box::new(io::stderr()),
        }
    }
}

impl Default for CliUI {
    fn default() -> Self {
        Self::new()
    }
}

impl UI for CliUI {
    fn output(&mut self, message: &str) -> Result<()> {
        writeln!(self.stdout, "{}", message)?;
        Ok(())
    }

    fn error(&mut self, message: &str) -> Result<()> {
        writeln!(self.stderr, "Error: {}", message)?;
        Ok(())
    }

    fn warning(&mut self, message: &str) -> Result<()> {
        writeln!(self.stderr, "Warning: {}", message)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestUI {
        output: Vec<String>,
        errors: Vec<String>,
    }

    impl TestUI {
        fn new() -> Self {
            TestUI {
                output: Vec::new(),
                errors: Vec::new(),
            }
        }
    }

    impl UI for TestUI {
        fn output(&mut self, message: &str) -> Result<()> {
            self.output.push(message.to_string());
            Ok(())
        }

        fn error(&mut self, message: &str) -> Result<()> {
            self.errors.push(message.to_string());
            Ok(())
        }

        fn warning(&mut self, message: &str) -> Result<()> {
            self.errors.push(format!("Warning: {}", message));
            Ok(())
        }
    }

    #[test]
    fn test_ui_output() {
        let mut ui = TestUI::new();
        ui.output("test message").unwrap();
        assert_eq!(ui.output, vec!["test message"]);
    }

    #[test]
    fn test_ui_error() {
        let mut ui = TestUI::new();
        ui.error("error message").unwrap();
        assert_eq!(ui.errors, vec!["error message"]);
    }

    #[test]
    fn test_ui_warning() {
        let mut ui = TestUI::new();
        ui.warning("warning message").unwrap();
        assert_eq!(ui.errors, vec!["Warning: warning message"]);
    }
}
