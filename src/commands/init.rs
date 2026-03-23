//! Initialize a new test repository

use crate::commands::Command;
use crate::error::Result;
use crate::repository::inquest::InquestRepositoryFactory;
use crate::repository::RepositoryFactory;
use crate::ui::UI;
use std::path::Path;

/// Command to initialize a new test repository.
///
/// Creates a `.inquest` directory with the necessary structure
/// to store test results and metadata.
pub struct InitCommand {
    base_path: Option<String>,
}

impl InitCommand {
    /// Creates a new init command.
    ///
    /// # Arguments
    /// * `base_path` - Optional base directory path where the repository will be created
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

        // Check for legacy .testrepository/ and suggest upgrade
        #[cfg(feature = "testr")]
        if base.join(".testrepository").exists() {
            ui.error(
                "A legacy .testrepository/ directory exists. Run 'inq upgrade' to convert it to the new .inquest/ format.",
            )?;
            return Ok(1);
        }

        let factory = InquestRepositoryFactory;

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
        "Initialize a new test repository"
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
        assert!(temp.path().join(".inquest").exists());
        assert!(temp.path().join(".inquest/metadata.db").exists());
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

    #[test]
    #[cfg(feature = "testr")]
    fn test_init_command_legacy_testrepository_exists() {
        let temp = TempDir::new().unwrap();

        // Create a legacy .testrepository/ directory
        let factory = crate::repository::testr::FileRepositoryFactory;
        crate::repository::RepositoryFactory::initialise(&factory, temp.path()).unwrap();

        let mut ui = TestUI::new();
        let cmd = InitCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 1);
        assert_eq!(ui.errors.len(), 1);
        assert!(
            ui.errors[0].contains("inq upgrade"),
            "got: {}",
            ui.errors[0]
        );
        // Should not have created .inquest/
        assert!(!temp.path().join(".inquest").exists());
    }
}
