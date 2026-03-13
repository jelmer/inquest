//! Configuration file parsing and handling
//!
//! Supports three configuration file formats:
//! - `inquest.toml` or `.inquest.toml` - TOML format (preferred)
//! - `.testr.conf` - legacy INI format with a `[DEFAULT]` section

use crate::error::{Error, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// The configuration file names to search for, in order of priority
pub const CONFIG_FILE_NAMES: &[&str] = &["inquest.toml", ".inquest.toml", ".testr.conf"];

/// Configuration loaded from inquest.toml or .testr.conf
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TestrConfig {
    /// Command line to run to execute tests
    pub test_command: String,

    /// The value to substitute into test_command when specific test ids should be run
    pub test_id_option: Option<String>,

    /// The option to use to cause the test runner to report test ids it would run
    pub test_list_option: Option<String>,

    /// The value to use for $IDLIST when no specific test ids are being run
    pub test_id_list_default: Option<String>,

    /// Optional call out to establish concurrency
    pub test_run_concurrency: Option<String>,

    /// Tags which should be used to filter test counts
    pub filter_tags: Option<String>,

    /// If set, group tests by the matched section of the test id
    pub group_regex: Option<String>,

    /// Provision one or more test run environments
    pub instance_provision: Option<String>,

    /// Execute a test runner process in a given environment
    pub instance_execute: Option<String>,

    /// Dispose of one or more test running environments
    pub instance_dispose: Option<String>,
}

impl TestrConfig {
    /// Load configuration from a config file, detecting format from the file extension
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("Failed to read {}: {}", path.display(), e)))?;

        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if file_name.ends_with(".toml") {
            Self::parse_toml(&contents)
        } else {
            Self::parse_ini(&contents)
        }
    }

    /// Find and load the configuration file from a directory.
    ///
    /// Searches for config files in order of priority:
    /// `inquest.toml`, `.inquest.toml`, `.testr.conf`
    ///
    /// Returns the parsed config and the path to the file that was loaded.
    pub fn find_in_directory(dir: &Path) -> Result<(Self, std::path::PathBuf)> {
        for name in CONFIG_FILE_NAMES {
            let path = dir.join(name);
            if path.exists() {
                let config = Self::load_from_file(&path)?;
                return Ok((config, path));
            }
        }

        Err(Error::Config(format!(
            "No configuration file found (looked for {})",
            CONFIG_FILE_NAMES.join(", ")
        )))
    }

    /// Parse configuration from a TOML string
    pub fn parse_toml(contents: &str) -> Result<Self> {
        let config: TestrConfig = toml::from_str(contents)
            .map_err(|e| Error::Config(format!("Failed to parse TOML config: {}", e)))?;

        Self::validate(config)
    }

    /// Parse configuration from an INI string (legacy .testr.conf format)
    pub fn parse_ini(contents: &str) -> Result<Self> {
        // Parse as INI format
        let ini: HashMap<String, HashMap<String, String>> = serde_ini::from_str(contents)
            .map_err(|e| Error::Config(format!("Failed to parse .testr.conf: {}", e)))?;

        // Extract DEFAULT section
        let default = ini
            .get("DEFAULT")
            .ok_or_else(|| Error::Config("No [DEFAULT] section in .testr.conf".to_string()))?;

        let config = TestrConfig {
            test_command: default
                .get("test_command")
                .ok_or_else(|| Error::Config("No test_command option in .testr.conf".to_string()))?
                .clone(),
            test_id_option: default.get("test_id_option").cloned(),
            test_list_option: default.get("test_list_option").cloned(),
            test_id_list_default: default.get("test_id_list_default").cloned(),
            test_run_concurrency: default.get("test_run_concurrency").cloned(),
            filter_tags: default.get("filter_tags").cloned(),
            group_regex: default.get("group_regex").cloned(),
            instance_provision: default.get("instance_provision").cloned(),
            instance_execute: default.get("instance_execute").cloned(),
            instance_dispose: default.get("instance_dispose").cloned(),
        };

        Self::validate(config)
    }

    /// Parse configuration from an INI string (legacy .testr.conf format)
    ///
    /// Alias for `parse_ini` for backward compatibility.
    pub fn parse(contents: &str) -> Result<Self> {
        Self::parse_ini(contents)
    }

    /// Validate a parsed configuration
    fn validate(config: TestrConfig) -> Result<TestrConfig> {
        // Validate required fields
        if config.test_command.is_empty() {
            return Err(Error::Config("test_command cannot be empty".to_string()));
        }

        // Validate that if $IDOPTION is used, test_id_option is configured
        if config.test_command.contains("$IDOPTION") && config.test_id_option.is_none() {
            return Err(Error::Config(
                "test_command uses $IDOPTION but test_id_option is not configured".to_string(),
            ));
        }

        // Validate that if $LISTOPT is used, test_list_option is configured
        if config.test_command.contains("$LISTOPT") && config.test_list_option.is_none() {
            return Err(Error::Config(
                "test_command uses $LISTOPT but test_list_option is not configured".to_string(),
            ));
        }

        Ok(config)
    }

    /// Substitute variables in a command string
    pub fn substitute_variables(&self, cmd: &str, vars: &HashMap<String, String>) -> String {
        let mut result = cmd.to_string();

        for (key, value) in vars {
            let placeholder = format!("${}", key);
            result = result.replace(&placeholder, value);
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_basic_config() {
        let config_str = r#"
[DEFAULT]
test_command=python -m subunit.run discover
"#;

        let config = TestrConfig::parse(config_str).unwrap();
        assert_eq!(config.test_command, "python -m subunit.run discover");
        assert!(config.test_id_option.is_none());
        assert!(config.test_list_option.is_none());
    }

    #[test]
    fn test_parse_full_config() {
        let config_str = r#"
[DEFAULT]
test_command=python -m subunit.run $IDOPTION
test_id_option=--load-list $IDFILE
test_list_option=--list
test_id_list_default=
filter_tags=worker-0
group_regex=^(.*\.)[^.]+$
"#;

        let config = TestrConfig::parse(config_str).unwrap();
        assert_eq!(config.test_command, "python -m subunit.run $IDOPTION");
        assert_eq!(
            config.test_id_option,
            Some("--load-list $IDFILE".to_string())
        );
        assert_eq!(config.test_list_option, Some("--list".to_string()));
        assert_eq!(config.test_id_list_default, Some("".to_string()));
        assert_eq!(config.filter_tags, Some("worker-0".to_string()));
        assert_eq!(config.group_regex, Some("^(.*\\.)[^.]+$".to_string()));
    }

    #[test]
    fn test_missing_test_command() {
        let config_str = r#"
[DEFAULT]
test_list_option=--list
"#;

        let result = TestrConfig::parse(config_str);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("test_command"));
    }

    #[test]
    fn test_missing_default_section() {
        let config_str = r#"
[OTHER]
test_command=foo
"#;

        let result = TestrConfig::parse(config_str);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("DEFAULT"));
    }

    #[test]
    fn test_idoption_without_test_id_option() {
        let config_str = r#"
[DEFAULT]
test_command=python -m test $IDOPTION
"#;

        let result = TestrConfig::parse(config_str);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("IDOPTION"));
    }

    #[test]
    fn test_listopt_without_test_list_option() {
        let config_str = r#"
[DEFAULT]
test_command=python -m test $LISTOPT
"#;

        let result = TestrConfig::parse(config_str);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("LISTOPT"));
    }

    #[test]
    fn test_substitute_variables() {
        let config = TestrConfig {
            test_command: "python -m test $IDOPTION $LISTOPT".to_string(),
            test_id_option: Some("--load-list $IDFILE".to_string()),
            ..Default::default()
        };

        let mut vars = HashMap::new();
        vars.insert(
            "IDOPTION".to_string(),
            "--load-list failing.list".to_string(),
        );
        vars.insert("LISTOPT".to_string(), "--list".to_string());

        let result = config.substitute_variables(&config.test_command, &vars);
        assert_eq!(result, "python -m test --load-list failing.list --list");
    }

    #[test]
    fn test_substitute_nested_variables() {
        let config = TestrConfig::default();

        let mut vars = HashMap::new();
        vars.insert("IDFILE".to_string(), "test_ids.txt".to_string());

        let cmd = "--load-list $IDFILE";
        let result = config.substitute_variables(cmd, &vars);
        assert_eq!(result, "--load-list test_ids.txt");
    }

    #[test]
    fn test_parse_toml_basic() {
        let config_str = r#"
test_command = "python -m subunit.run discover"
"#;

        let config = TestrConfig::parse_toml(config_str).unwrap();
        assert_eq!(config.test_command, "python -m subunit.run discover");
        assert!(config.test_id_option.is_none());
        assert!(config.test_list_option.is_none());
    }

    #[test]
    fn test_parse_toml_full() {
        let config_str = r#"
test_command = "python -m subunit.run $IDOPTION"
test_id_option = "--load-list $IDFILE"
test_list_option = "--list"
test_id_list_default = ""
filter_tags = "worker-0"
group_regex = '^(.*\.)[^.]+$'
"#;

        let config = TestrConfig::parse_toml(config_str).unwrap();
        assert_eq!(config.test_command, "python -m subunit.run $IDOPTION");
        assert_eq!(
            config.test_id_option,
            Some("--load-list $IDFILE".to_string())
        );
        assert_eq!(config.test_list_option, Some("--list".to_string()));
        assert_eq!(config.test_id_list_default, Some("".to_string()));
        assert_eq!(config.filter_tags, Some("worker-0".to_string()));
        assert_eq!(config.group_regex, Some("^(.*\\.)[^.]+$".to_string()));
    }

    #[test]
    fn test_parse_toml_missing_test_command() {
        let config_str = r#"
test_list_option = "--list"
"#;

        let result = TestrConfig::parse_toml(config_str);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("test_command"));
    }

    #[test]
    fn test_parse_toml_idoption_without_test_id_option() {
        let config_str = r#"
test_command = "python -m test $IDOPTION"
"#;

        let result = TestrConfig::parse_toml(config_str);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("IDOPTION"));
    }

    #[test]
    fn test_find_in_directory_inquest_toml() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("inquest.toml");
        std::fs::write(&config_path, r#"test_command = "python -m test""#).unwrap();

        let (config, path) = TestrConfig::find_in_directory(temp_dir.path()).unwrap();
        assert_eq!(config.test_command, "python -m test");
        assert_eq!(path, config_path);
    }

    #[test]
    fn test_find_in_directory_dot_inquest_toml() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join(".inquest.toml");
        std::fs::write(&config_path, r#"test_command = "python -m test""#).unwrap();

        let (config, path) = TestrConfig::find_in_directory(temp_dir.path()).unwrap();
        assert_eq!(config.test_command, "python -m test");
        assert_eq!(path, config_path);
    }

    #[test]
    fn test_find_in_directory_testr_conf() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join(".testr.conf");
        std::fs::write(&config_path, "[DEFAULT]\ntest_command=python -m test\n").unwrap();

        let (config, path) = TestrConfig::find_in_directory(temp_dir.path()).unwrap();
        assert_eq!(config.test_command, "python -m test");
        assert_eq!(path, config_path);
    }

    #[test]
    fn test_find_in_directory_priority() {
        let temp_dir = TempDir::new().unwrap();

        // Create both files; inquest.toml should win
        std::fs::write(
            temp_dir.path().join("inquest.toml"),
            r#"test_command = "from-inquest-toml""#,
        )
        .unwrap();
        std::fs::write(
            temp_dir.path().join(".testr.conf"),
            "[DEFAULT]\ntest_command=from-testr-conf\n",
        )
        .unwrap();

        let (config, path) = TestrConfig::find_in_directory(temp_dir.path()).unwrap();
        assert_eq!(config.test_command, "from-inquest-toml");
        assert_eq!(path, temp_dir.path().join("inquest.toml"));
    }

    #[test]
    fn test_find_in_directory_none_found() {
        let temp_dir = TempDir::new().unwrap();
        let result = TestrConfig::find_in_directory(temp_dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("inquest.toml"));
        assert!(err.contains(".testr.conf"));
    }
}
