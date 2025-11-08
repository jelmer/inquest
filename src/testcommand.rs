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

    /// Execute the test_run_concurrency callout to determine concurrency
    ///
    /// If `test_run_concurrency` is configured, executes the command and parses
    /// the output as a concurrency number. Returns None if not configured or on error.
    pub fn get_concurrency(&self) -> Result<Option<usize>> {
        let Some(ref cmd) = self.config.test_run_concurrency else {
            return Ok(None);
        };

        // Execute the command
        let output = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.base_dir)
            .output()
            .map_err(|e| {
                Error::CommandExecution(format!("Failed to execute test_run_concurrency: {}", e))
            })?;

        if !output.status.success() {
            return Err(Error::CommandExecution(format!(
                "test_run_concurrency command failed with status: {}",
                output.status
            )));
        }

        // Parse the output as a number
        let output_str = String::from_utf8_lossy(&output.stdout);
        let trimmed = output_str.trim();

        if trimmed.is_empty() {
            return Err(Error::CommandExecution(
                "test_run_concurrency command produced no output".to_string(),
            ));
        }

        let concurrency = trimmed.parse::<usize>().map_err(|e| {
            Error::CommandExecution(format!(
                "Failed to parse test_run_concurrency output '{}': {}",
                trimmed, e
            ))
        })?;

        if concurrency == 0 {
            return Err(Error::CommandExecution(
                "test_run_concurrency must be greater than 0".to_string(),
            ));
        }

        Ok(Some(concurrency))
    }

    /// Build the command to execute tests
    pub fn build_command(
        &self,
        test_ids: Option<&[TestId]>,
        list_only: bool,
    ) -> Result<(String, Option<NamedTempFile>)> {
        self.build_command_with_instance(test_ids, list_only, None)
    }

    /// Build the command to execute tests with optional instance ID
    pub fn build_command_with_instance(
        &self,
        test_ids: Option<&[TestId]>,
        list_only: bool,
        instance_id: Option<&str>,
    ) -> Result<(String, Option<NamedTempFile>)> {
        self.build_command_full(test_ids, list_only, instance_id, None)
    }

    /// Build the command to execute tests with all options
    pub fn build_command_full(
        &self,
        test_ids: Option<&[TestId]>,
        list_only: bool,
        instance_id: Option<&str>,
        test_args: Option<&[String]>,
    ) -> Result<(String, Option<NamedTempFile>)> {
        // If instance_execute is configured and we have an instance ID, use that
        let mut cmd = if let (Some(ref instance_exec), Some(id)) =
            (&self.config.instance_execute, instance_id)
        {
            instance_exec.replace("$INSTANCE_ID", id)
        } else {
            self.config.test_command.clone()
        };
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

        // Append test_args if provided (like Python's testargs)
        if let Some(args) = test_args {
            if !args.is_empty() {
                cmd.push(' ');
                cmd.push_str(&args.join(" "));
            }
        }

        Ok((cmd, temp_file))
    }

    /// List all available tests
    ///
    /// Parses the subunit stream to extract test IDs from enumeration events,
    /// matching the Python testrepository's parse_enumeration() behavior.
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

        // Parse subunit stream to extract test IDs from enumeration events
        // (matching Python's parse_enumeration which looks for 'exists' status)
        use subunit::io::sync::iter_stream;
        use subunit::types::stream::ScannedItem;
        use subunit::types::teststatus::TestStatus as SubunitTestStatus;

        let mut test_ids = Vec::new();

        for item in iter_stream(&output.stdout[..]) {
            match item {
                Ok(ScannedItem::Event(event)) => {
                    // Enumeration events indicate test existence
                    if event.status == SubunitTestStatus::Enumeration {
                        if let Some(test_id) = event.test_id {
                            test_ids.push(TestId::new(test_id));
                        }
                    }
                }
                Ok(ScannedItem::Bytes(_)) => {
                    // Skip interleaved non-event data
                    continue;
                }
                Ok(ScannedItem::Unknown(_, _)) => {
                    // Skip unknown/corrupted data
                    continue;
                }
                Err(e) => {
                    return Err(Error::CommandExecution(format!(
                        "Failed to parse test list: {}",
                        e
                    )));
                }
            }
        }

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

    /// Provision test instances for parallel execution
    ///
    /// Calls the instance_provision command to create N isolated test environments.
    /// Returns a vector of instance IDs (one per line from stdout).
    pub fn provision_instances(&self, count: usize) -> Result<Vec<String>> {
        let Some(ref cmd) = self.config.instance_provision else {
            // No provisioning configured, return simple numeric instance IDs
            return Ok((0..count).map(|i| i.to_string()).collect());
        };

        // Execute the provision command with the count
        let full_cmd = cmd.replace("$INSTANCE_COUNT", &count.to_string());

        let output = Command::new("sh")
            .arg("-c")
            .arg(&full_cmd)
            .current_dir(&self.base_dir)
            .output()
            .map_err(|e| {
                Error::CommandExecution(format!("Failed to execute instance_provision: {}", e))
            })?;

        if !output.status.success() {
            return Err(Error::CommandExecution(format!(
                "instance_provision command failed with status: {}",
                output.status
            )));
        }

        // Parse instance IDs from stdout (one per line)
        let output_str = String::from_utf8_lossy(&output.stdout);
        let instance_ids: Vec<String> = output_str
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if instance_ids.len() != count {
            return Err(Error::CommandExecution(format!(
                "instance_provision returned {} instances, expected {}",
                instance_ids.len(),
                count
            )));
        }

        Ok(instance_ids)
    }

    /// Dispose of test instances
    ///
    /// Calls the instance_dispose command to clean up test environments.
    pub fn dispose_instances(&self, instance_ids: &[String]) -> Result<()> {
        let Some(ref cmd) = self.config.instance_dispose else {
            // No disposal configured, nothing to do
            return Ok(());
        };

        // Execute dispose for each instance
        for instance_id in instance_ids {
            let full_cmd = cmd.replace("$INSTANCE_ID", instance_id);

            let output = Command::new("sh")
                .arg("-c")
                .arg(&full_cmd)
                .current_dir(&self.base_dir)
                .output()
                .map_err(|e| {
                    Error::CommandExecution(format!(
                        "Failed to execute instance_dispose for {}: {}",
                        instance_id, e
                    ))
                })?;

            if !output.status.success() {
                // Log warning but continue disposing other instances
                eprintln!(
                    "Warning: instance_dispose failed for {} with status: {}",
                    instance_id, output.status
                );
            }
        }

        Ok(())
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

    #[test]
    fn test_get_concurrency_not_configured() {
        let config = create_test_config();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        let result = tc.get_concurrency().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_get_concurrency_success() {
        let config_str = r#"
[DEFAULT]
test_command=echo ""
test_run_concurrency=echo 4
"#;
        let config = TestrConfig::parse(config_str).unwrap();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        let result = tc.get_concurrency().unwrap();
        assert_eq!(result, Some(4));
    }

    #[test]
    fn test_get_concurrency_with_nproc() {
        let config_str = r#"
[DEFAULT]
test_command=echo ""
test_run_concurrency=nproc
"#;
        let config = TestrConfig::parse(config_str).unwrap();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        let result = tc.get_concurrency().unwrap();
        assert!(result.is_some());
        assert!(result.unwrap() > 0);
    }

    #[test]
    fn test_get_concurrency_invalid_output() {
        let config_str = r#"
[DEFAULT]
test_command=echo ""
test_run_concurrency=echo "not a number"
"#;
        let config = TestrConfig::parse(config_str).unwrap();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        let result = tc.get_concurrency();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parse"));
    }

    #[test]
    fn test_get_concurrency_zero() {
        let config_str = r#"
[DEFAULT]
test_command=echo ""
test_run_concurrency=echo 0
"#;
        let config = TestrConfig::parse(config_str).unwrap();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        let result = tc.get_concurrency();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("greater than 0"));
    }

    #[test]
    fn test_get_concurrency_command_fails() {
        let config_str = r#"
[DEFAULT]
test_command=echo ""
test_run_concurrency=exit 1
"#;
        let config = TestrConfig::parse(config_str).unwrap();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        let result = tc.get_concurrency();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed"));
    }

    #[test]
    fn test_provision_instances_no_config() {
        let config = create_test_config();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        let instances = tc.provision_instances(3).unwrap();
        // Without configuration, should return simple numeric IDs
        assert_eq!(instances, vec!["0", "1", "2"]);
    }

    #[test]
    fn test_provision_instances_with_config() {
        let config_str = r#"
[DEFAULT]
test_command=echo ""
instance_provision=echo "db-0\ndb-1\ndb-2" | head -n $INSTANCE_COUNT
"#;
        let config = TestrConfig::parse(config_str).unwrap();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        let instances = tc.provision_instances(3).unwrap();
        assert_eq!(instances, vec!["db-0", "db-1", "db-2"]);
    }

    #[test]
    fn test_dispose_instances_no_config() {
        let config = create_test_config();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        let instances = vec!["0".to_string(), "1".to_string()];
        // Should succeed without error even without configuration
        tc.dispose_instances(&instances).unwrap();
    }

    #[test]
    fn test_build_command_with_instance() {
        let config_str = r#"
[DEFAULT]
test_command=python -m test
instance_execute=python -m test --instance=$INSTANCE_ID
"#;
        let config = TestrConfig::parse(config_str).unwrap();
        let temp_dir = TempDir::new().unwrap();
        let tc = TestCommand::new(config, temp_dir.path().to_path_buf());

        // Without instance ID, should use normal test_command
        let (cmd, _) = tc.build_command_with_instance(None, false, None).unwrap();
        assert_eq!(cmd, "python -m test");

        // With instance ID, should use instance_execute
        let (cmd, _) = tc
            .build_command_with_instance(None, false, Some("worker-0"))
            .unwrap();
        assert_eq!(cmd, "python -m test --instance=worker-0");
    }
}
