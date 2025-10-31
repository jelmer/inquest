//! Initialize a new test repository

use crate::commands::Command;
use crate::error::Result;
use crate::repository::file::FileRepositoryFactory;
use crate::repository::RepositoryFactory;
use crate::ui::UI;
use std::path::Path;

pub struct InitCommand {
    base_path: Option<String>,
}

impl InitCommand {
    pub fn new(base_path: Option<String>) -> Self {
        InitCommand { base_path }
    }
}

impl Command for InitCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = self
            .base_path
            .as_deref()
            .map(Path::new)
            .unwrap_or_else(|| Path::new("."));

        let factory = FileRepositoryFactory;

        match factory.initialise(base) {
            Ok(_) => {
                ui.output("Initialized empty test repository")?;
                Ok(0)
            }
            Err(e) => {
                ui.error(&format!("Failed to initialize repository: {}", e))?;
                Ok(1)
            }
        }
    }

    fn name(&self) -> &str {
        "init"
    }

    fn help(&self) -> &str {
        "Initialize a new test repository in .testrepository/"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::UI;
    use tempfile::TempDir;

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
    fn test_init_command() {
        let temp = TempDir::new().unwrap();
        let mut ui = TestUI::new();

        let cmd = InitCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert_eq!(ui.output.len(), 1);
        assert!(ui.output[0].contains("Initialized"));

        // Verify repository was created
        assert!(temp.path().join(".testrepository").exists());
        assert!(temp.path().join(".testrepository/format").exists());
    }

    #[test]
    fn test_init_command_already_exists() {
        let temp = TempDir::new().unwrap();
        let mut ui = TestUI::new();

        let cmd = InitCommand::new(Some(temp.path().to_string_lossy().to_string()));

        // Initialize once
        cmd.execute(&mut ui).unwrap();

        // Try to initialize again
        ui.output.clear();
        ui.errors.clear();
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 1);
        assert_eq!(ui.errors.len(), 1);
        assert!(ui.errors[0].contains("Failed"));
    }
}
