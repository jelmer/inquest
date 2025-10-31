//! Test command execution framework
//!
//! This module provides the TestCommand struct which handles executing
//! test commands based on .testr.conf configuration.

use crate::config::TestrConfig;
use crate::error::{Error, Result};
use crate::repository::TestId;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::NamedTempFile;

/// Test command executor
#[derive(Debug)]
pub struct TestCommand {
    config: TestrConfig,
    base_dir: PathBuf,
}

impl TestCommand {
    /// Create a new TestCommand from a configuration
    pub fn new(config: TestrConfig, base_dir: PathBuf) -> Self {
        TestCommand { config, base_dir }
    }

    /// Load TestCommand from .testr.conf in the given directory
    pub fn from_directory(dir: &Path) -> Result<Self> {
        let config_path = dir.join(".testr.conf");
        if !config_path.exists() {
            return Err(Error::Config("No .testr.conf file found".to_string()));
        }

        let config = TestrConfig::load_from_file(&config_path)?;
        Ok(TestCommand::new(config, dir.to_path_buf()))
    }

    /// Build the command to execute tests
    pub fn build_command(
        &self,
        test_ids: Option<&[TestId]>,
        list_only: bool,
    ) -> Result<(String, Option<NamedTempFile>)> {
        let mut cmd = self.config.test_command.clone();
        let mut vars = HashMap::new();
        let mut temp_file = None;

        // Handle test listing
        if list_only {
            if let Some(ref list_opt) = self.config.test_list_option {
                vars.insert("LISTOPT".to_string(), list_opt.clone());
            } else if cmd.contains("$LISTOPT") {
                return Err(Error::Config(
                    "test_list_option not configured but $LISTOPT used".to_string(),
                ));
            }
        } else {
            vars.insert("LISTOPT".to_string(), String::new());
        }

        // Handle test IDs
        if let Some(ids) = test_ids {
            if !ids.is_empty() {
                // Create IDLIST (space-separated)
                let id_list = ids
                    .iter()
                    .map(|id| id.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                vars.insert("IDLIST".to_string(), id_list.clone());

                // Create IDFILE (newline-separated in temp file)
                let mut temp = NamedTempFile::new().map_err(|e| {
                    Error::CommandExecution(format!("Failed to create temp file: {}", e))
                })?;

                for id in ids {
                    writeln!(temp, "{}", id.as_str()).map_err(|e| {
                        Error::CommandExecution(format!("Failed to write to temp file: {}", e))
                    })?;
                }

                let temp_path = temp.path().to_string_lossy().to_string();
                vars.insert("IDFILE".to_string(), temp_path);

                // Handle IDOPTION
                if let Some(ref id_option) = self.config.test_id_option {
                    let id_option_expanded = self.config.substitute_variables(id_option, &vars);
                    vars.insert("IDOPTION".to_string(), id_option_expanded);
                } else if cmd.contains("$IDOPTION") {
                    return Err(Error::Config(
                        "test_id_option not configured but $IDOPTION used".to_string(),
                    ));
                }

                temp_file = Some(temp);
            } else {
                // Empty test ID list
                vars.insert("IDLIST".to_string(), String::new());
                vars.insert("IDOPTION".to_string(), String::new());
            }
        } else {
            // No test IDs specified - use default
            let id_list = self.config.test_id_list_default.as_deref().unwrap_or("");
            vars.insert("IDLIST".to_string(), id_list.to_string());
            vars.insert("IDOPTION".to_string(), String::new());
        }

        // Substitute all variables
        cmd = self.config.substitute_variables(&cmd, &vars);

        Ok((cmd, temp_file))
    }

    /// List all available tests
    pub fn list_tests(&self) -> Result<Vec<TestId>> {
        let (cmd, _temp_file) = self.build_command(None, true)?;

        let output = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&self.base_dir)
            .output()
            .map_err(|e| {
                Error::CommandExecution(format!("Failed to execute test command: {}", e))
            })?;

        if !output.status.success() {
            return Err(Error::CommandExecution(format!(
                "Test listing command failed with exit code: {}",
                output.status
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let test_ids = stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| TestId::new(line.trim()))
            .collect();

        Ok(test_ids)
    }

    /// Execute tests and return the subunit output
    pub fn run_tests(&self, test_ids: Option<&[TestId]>) -> Result<std::process::Child> {
        let (cmd, _temp_file) = self.build_command(test_ids, false)?;

        // Spawn the test process
        let child = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&self.base_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::CommandExecution(format!("Failed to spawn test command: {}", e)))?;

        // Note: _temp_file will be kept alive for the duration of the command
        // We need to handle this more carefully in a real implementation

        Ok(child)
    }

    /// Get the configuration
    pub fn config(&self) -> &TestrConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_config() -> TestrConfig {
        let config_str = r#"
[DEFAULT]
test_command=python -m subunit.run $LISTOPT $IDOPTION
test_id_option=--load-list $IDFILE
test_list_option=--list
"#;
        TestrConfig::parse(config_str).unwrap()
    }

    #[test]
    fn test_build_command_no_ids() {
        let config = create_test_config();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        let (cmd, temp_file) = tc.build_command(None, false).unwrap();
        assert_eq!(cmd, "python -m subunit.run  ");
        assert!(temp_file.is_none());
    }

    #[test]
    fn test_build_command_with_list() {
        let config = create_test_config();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        let (cmd, _) = tc.build_command(None, true).unwrap();
        assert_eq!(cmd, "python -m subunit.run --list ");
    }

    #[test]
    fn test_build_command_with_ids() {
        let config = create_test_config();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        let test_ids = vec![TestId::new("test1"), TestId::new("test2")];
        let (cmd, temp_file) = tc.build_command(Some(&test_ids), false).unwrap();

        // Command should contain the --load-list option with temp file path
        assert!(cmd.starts_with("python -m subunit.run  --load-list "));
        assert!(temp_file.is_some());

        // Verify temp file contents
        if let Some(temp) = temp_file {
            let contents = fs::read_to_string(temp.path()).unwrap();
            assert_eq!(contents, "test1\ntest2\n");
        }
    }

    #[test]
    fn test_build_command_missing_list_option() {
        let config_str = r#"
[DEFAULT]
test_command=python -m test $LISTOPT
"#;
        let config = TestrConfig::parse(config_str).unwrap_err();
        assert!(config.to_string().contains("LISTOPT"));
    }

    #[test]
    fn test_from_directory_missing_config() {
        let temp_dir = TempDir::new().unwrap();
        let result = TestCommand::from_directory(temp_dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains(".testr.conf"));
    }

    #[test]
    fn test_from_directory_with_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join(".testr.conf");

        fs::write(
            &config_path,
            r#"
[DEFAULT]
test_command=python -m test
"#,
        )
        .unwrap();

        let tc = TestCommand::from_directory(temp_dir.path()).unwrap();
        assert_eq!(tc.config().test_command, "python -m test");
    }
}
