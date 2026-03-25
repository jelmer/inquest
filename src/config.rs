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
use std::time::Duration;

/// Multiplier applied to historical test duration when computing auto timeouts.
/// A test that historically takes 10s gets a timeout of 10s * 10 = 100s.
pub const AUTO_TIMEOUT_MULTIPLIER: f64 = 10.0;

/// Minimum auto timeout to avoid killing tests that are just slightly slow.
pub const AUTO_TIMEOUT_MINIMUM: Duration = Duration::from_secs(30);

/// Minimum auto max-duration for the overall run.
pub const AUTO_MAX_DURATION_MINIMUM: Duration = Duration::from_secs(60);

/// Interval between polls when waiting for a child process with a timeout.
pub const TIMEOUT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum number of times a test process can be restarted due to per-test
/// timeouts before giving up. Prevents infinite restart loops when many tests hang.
pub const MAX_TEST_TIMEOUT_RESTARTS: usize = 10;

/// Multiplier for slow test warnings: warn if a test takes longer than this
/// multiple of its historical average duration.
pub const SLOW_TEST_WARNING_MULTIPLIER: f64 = 3.0;

/// Timeout configuration that supports "disabled", "auto", or an explicit duration.
///
/// Used for both per-test timeouts and overall run timeouts.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum TimeoutSetting {
    /// No timeout - tests can run indefinitely
    #[default]
    Disabled,
    /// Explicit timeout duration for all tests
    Fixed(Duration),
    /// Automatically determine timeout from historical test durations.
    /// Uses a multiplier of the historical average (default 10x, minimum 30s).
    Auto,
}

impl TimeoutSetting {
    /// Parse a timeout string value.
    ///
    /// Accepts:
    /// - "disabled" or "" - no timeout
    /// - "auto" - automatic from history
    /// - Duration string like "5m", "300s", "1h", "1h30m"
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("disabled") {
            return Ok(TimeoutSetting::Disabled);
        }
        if s.eq_ignore_ascii_case("auto") {
            return Ok(TimeoutSetting::Auto);
        }
        let duration = parse_duration_string(s)?;
        Ok(TimeoutSetting::Fixed(duration))
    }

    /// Compute the effective timeout for a specific test, given its historical duration.
    ///
    /// Returns `None` if there should be no timeout.
    pub fn effective_timeout(&self, historical: Option<Duration>) -> Option<Duration> {
        match self {
            TimeoutSetting::Disabled => None,
            TimeoutSetting::Fixed(d) => Some(*d),
            TimeoutSetting::Auto => {
                let base = historical
                    .map(|h| {
                        let computed =
                            Duration::from_secs_f64(h.as_secs_f64() * AUTO_TIMEOUT_MULTIPLIER);
                        computed.max(AUTO_TIMEOUT_MINIMUM)
                    })
                    .unwrap_or(AUTO_TIMEOUT_MINIMUM);
                Some(base)
            }
        }
    }
}

/// Parse a human-readable duration string like "5m", "300s", "1h", "1h30m", "90".
///
/// Supported suffixes: s (seconds), m (minutes), h (hours).
/// Plain numbers are treated as seconds.
pub fn parse_duration_string(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::Config("empty duration string".to_string()));
    }

    let mut total_secs: f64 = 0.0;
    let mut current_num = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            current_num.push(c);
        } else {
            if current_num.is_empty() {
                return Err(Error::Config(format!("invalid duration string: '{}'", s)));
            }
            let num: f64 = current_num.parse().map_err(|_| {
                Error::Config(format!("invalid number in duration string: '{}'", s))
            })?;
            current_num.clear();

            match c {
                's' => total_secs += num,
                'm' => total_secs += num * 60.0,
                'h' => total_secs += num * 3600.0,
                _ => {
                    return Err(Error::Config(format!(
                        "invalid duration suffix '{}' in '{}'",
                        c, s
                    )))
                }
            }
        }
    }

    // If there's a trailing number with no suffix, treat as seconds
    if !current_num.is_empty() {
        let num: f64 = current_num
            .parse()
            .map_err(|_| Error::Config(format!("invalid number in duration string: '{}'", s)))?;
        total_secs += num;
    }

    if total_secs <= 0.0 {
        return Err(Error::Config(format!("duration must be positive: '{}'", s)));
    }

    Ok(Duration::from_secs_f64(total_secs))
}

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

    /// Per-test timeout (e.g. "5m", "auto", "disabled")
    pub test_timeout: Option<String>,

    /// Overall run timeout (e.g. "30m", "1h", "auto", "disabled")
    pub max_duration: Option<String>,

    /// No-output timeout - kill test if no output for this duration (e.g. "60s")
    pub no_output_timeout: Option<String>,

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
            test_timeout: default.get("test_timeout").cloned(),
            max_duration: default.get("max_duration").cloned(),
            no_output_timeout: default.get("no_output_timeout").cloned(),
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

    /// Parse the test_timeout config value into a TimeoutSetting.
    pub fn parsed_test_timeout(&self) -> Result<TimeoutSetting> {
        match &self.test_timeout {
            None => Ok(TimeoutSetting::Disabled),
            Some(s) => TimeoutSetting::parse(s),
        }
    }

    /// Parse the max_duration config value into a TimeoutSetting.
    pub fn parsed_max_duration(&self) -> Result<TimeoutSetting> {
        match &self.max_duration {
            None => Ok(TimeoutSetting::Disabled),
            Some(s) => TimeoutSetting::parse(s),
        }
    }

    /// Parse the no_output_timeout config value into a Duration.
    pub fn parsed_no_output_timeout(&self) -> Result<Option<Duration>> {
        match &self.no_output_timeout {
            None => Ok(None),
            Some(s) => {
                let s = s.trim();
                if s.is_empty() || s.eq_ignore_ascii_case("disabled") {
                    Ok(None)
                } else {
                    Ok(Some(parse_duration_string(s)?))
                }
            }
        }
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

    #[test]
    fn test_parse_duration_string_seconds() {
        assert_eq!(
            parse_duration_string("30s").unwrap(),
            Duration::from_secs(30)
        );
        assert_eq!(
            parse_duration_string("300").unwrap(),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn test_parse_duration_string_minutes() {
        assert_eq!(
            parse_duration_string("5m").unwrap(),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn test_parse_duration_string_hours() {
        assert_eq!(
            parse_duration_string("1h").unwrap(),
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn test_parse_duration_string_combined() {
        assert_eq!(
            parse_duration_string("1h30m").unwrap(),
            Duration::from_secs(5400)
        );
    }

    #[test]
    fn test_parse_duration_string_invalid() {
        assert!(parse_duration_string("").is_err());
        assert!(parse_duration_string("abc").is_err());
        assert!(parse_duration_string("5x").is_err());
    }

    #[test]
    fn test_timeout_setting_parse_disabled() {
        assert_eq!(
            TimeoutSetting::parse("disabled").unwrap(),
            TimeoutSetting::Disabled
        );
        assert_eq!(TimeoutSetting::parse("").unwrap(), TimeoutSetting::Disabled);
        assert_eq!(
            TimeoutSetting::parse("DISABLED").unwrap(),
            TimeoutSetting::Disabled
        );
    }

    #[test]
    fn test_timeout_setting_parse_auto() {
        assert_eq!(TimeoutSetting::parse("auto").unwrap(), TimeoutSetting::Auto);
        assert_eq!(TimeoutSetting::parse("AUTO").unwrap(), TimeoutSetting::Auto);
    }

    #[test]
    fn test_timeout_setting_parse_fixed() {
        assert_eq!(
            TimeoutSetting::parse("5m").unwrap(),
            TimeoutSetting::Fixed(Duration::from_secs(300))
        );
    }

    #[test]
    fn test_timeout_setting_effective_disabled() {
        let t = TimeoutSetting::Disabled;
        assert_eq!(t.effective_timeout(None), None);
        assert_eq!(t.effective_timeout(Some(Duration::from_secs(10))), None);
    }

    #[test]
    fn test_timeout_setting_effective_fixed() {
        let t = TimeoutSetting::Fixed(Duration::from_secs(300));
        assert_eq!(t.effective_timeout(None), Some(Duration::from_secs(300)));
        assert_eq!(
            t.effective_timeout(Some(Duration::from_secs(10))),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn test_timeout_setting_effective_auto_with_history() {
        let t = TimeoutSetting::Auto;
        // 10s * 10x = 100s, but minimum is 30s so 100s wins
        assert_eq!(
            t.effective_timeout(Some(Duration::from_secs(10))),
            Some(Duration::from_secs(100))
        );
    }

    #[test]
    fn test_timeout_setting_effective_auto_with_small_history() {
        let t = TimeoutSetting::Auto;
        // 1s * 10x = 10s, below minimum of 30s
        assert_eq!(
            t.effective_timeout(Some(Duration::from_secs(1))),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn test_timeout_setting_effective_auto_no_history() {
        let t = TimeoutSetting::Auto;
        // No history, uses minimum of 30s
        assert_eq!(t.effective_timeout(None), Some(Duration::from_secs(30)));
    }

    #[test]
    fn test_config_parsed_timeout_fields() {
        let config_str = r#"
test_command = "python -m test"
test_timeout = "5m"
max_duration = "auto"
no_output_timeout = "60s"
"#;
        let config = TestrConfig::parse_toml(config_str).unwrap();
        assert_eq!(
            config.parsed_test_timeout().unwrap(),
            TimeoutSetting::Fixed(Duration::from_secs(300))
        );
        assert_eq!(config.parsed_max_duration().unwrap(), TimeoutSetting::Auto);
        assert_eq!(
            config.parsed_no_output_timeout().unwrap(),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn test_config_parsed_timeout_fields_defaults() {
        let config_str = r#"
test_command = "python -m test"
"#;
        let config = TestrConfig::parse_toml(config_str).unwrap();
        assert_eq!(
            config.parsed_test_timeout().unwrap(),
            TimeoutSetting::Disabled
        );
        assert_eq!(
            config.parsed_max_duration().unwrap(),
            TimeoutSetting::Disabled
        );
        assert_eq!(config.parsed_no_output_timeout().unwrap(), None);
    }
}
