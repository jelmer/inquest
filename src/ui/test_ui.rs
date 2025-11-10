//! Test utilities for UI testing

use crate::error::Result;
use crate::ui::UI;

/// A UI implementation for testing that captures output in vectors
pub struct TestUI {
    pub output: Vec<String>,
    pub errors: Vec<String>,
    pub bytes_output: Vec<Vec<u8>>,
}

impl TestUI {
    pub fn new() -> Self {
        TestUI {
            output: Vec::new(),
            errors: Vec::new(),
            bytes_output: Vec::new(),
        }
    }
}

impl Default for TestUI {
    fn default() -> Self {
        Self::new()
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

    fn output_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.bytes_output.push(bytes.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_test_ui_output() {
        let mut ui = TestUI::new();
        ui.output("test message").unwrap();
        assert_eq!(ui.output.len(), 1);
        assert_eq!(ui.output[0], "test message");
    }

    #[test]
    fn test_test_ui_error() {
        let mut ui = TestUI::new();
        ui.error("error message").unwrap();
        assert_eq!(ui.errors.len(), 1);
        assert_eq!(ui.errors[0], "error message");
    }

    #[test]
    fn test_test_ui_warning() {
        let mut ui = TestUI::new();
        ui.warning("warning message").unwrap();
        assert_eq!(ui.errors.len(), 1);
        assert_eq!(ui.errors[0], "Warning: warning message");
    }
}
