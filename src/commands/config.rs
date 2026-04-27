//! Show the resolved effective configuration

use crate::commands::Command;
use crate::config::{TestrConfig, TimeoutSetting};
use crate::error::Result;
use crate::ordering::TestOrder;
use crate::ui::UI;
use std::path::Path;
use std::time::Duration;

/// Origin of a resolved configuration value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Cli,
    Config,
    Default,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Cli => "cli",
            Source::Config => "config",
            Source::Default => "default",
        }
    }
}

/// Print the resolved configuration that `inq run` would use, combining
/// values from the config file, CLI overrides, and built-in defaults.
pub struct ConfigCommand {
    /// Repository / project directory (defaults to current directory).
    pub base_path: Option<String>,
    /// Per-test timeout override from CLI (e.g. "5m", "auto", "disabled").
    pub test_timeout: Option<String>,
    /// Overall run-duration override from CLI.
    pub max_duration: Option<String>,
    /// No-output timeout override from CLI.
    pub no_output_timeout: Option<String>,
    /// Test ordering override from CLI.
    pub order: Option<String>,
    /// Concurrency override from CLI.
    pub concurrency: Option<usize>,
}

impl ConfigCommand {
    /// Create a new ConfigCommand. CLI overrides default to None.
    pub fn new(base_path: Option<String>) -> Self {
        ConfigCommand {
            base_path,
            test_timeout: None,
            max_duration: None,
            no_output_timeout: None,
            order: None,
            concurrency: None,
        }
    }
}

fn format_timeout(setting: &TimeoutSetting) -> String {
    match setting {
        TimeoutSetting::Disabled => "disabled".to_string(),
        TimeoutSetting::Auto => "auto".to_string(),
        TimeoutSetting::Fixed(d) => format_duration(*d),
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        return format!("{}ms", d.as_millis());
    }
    if secs >= 3600 && secs.is_multiple_of(3600) {
        return format!("{}h", secs / 3600);
    }
    if secs >= 60 && secs.is_multiple_of(60) {
        return format!("{}m", secs / 60);
    }
    format!("{}s", secs)
}

impl Command for ConfigCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let base = Path::new(self.base_path.as_deref().unwrap_or("."));

        let loaded = TestrConfig::find_in_directory(base).ok();

        match &loaded {
            Some((_, path)) => {
                ui.output(&format!("Config file: {}", path.display()))?;
            }
            None => {
                ui.output("Config file: (none found)")?;
            }
        }
        ui.output(&format!("Working directory: {}", base.display()))?;
        ui.output("")?;

        let config = loaded.as_ref().map(|(c, _)| c.clone()).unwrap_or_default();

        if !config.test_command.is_empty() {
            print_value(ui, "test_command", &config.test_command, Source::Config)?;
        } else {
            print_unset(ui, "test_command", "(unset)")?;
        }

        print_optional_string(ui, "test_id_option", &config.test_id_option)?;
        print_optional_string(ui, "test_list_option", &config.test_list_option)?;
        print_optional_string(ui, "test_id_list_default", &config.test_id_list_default)?;
        print_optional_string(ui, "test_run_concurrency", &config.test_run_concurrency)?;
        print_optional_string(ui, "filter_tags", &config.filter_tags)?;
        print_optional_string(ui, "group_regex", &config.group_regex)?;
        print_optional_string(ui, "instance_provision", &config.instance_provision)?;
        print_optional_string(ui, "instance_execute", &config.instance_execute)?;
        print_optional_string(ui, "instance_dispose", &config.instance_dispose)?;

        let (test_timeout_str, src) =
            resolve_timeout(self.test_timeout.as_deref(), &config.test_timeout)?;
        print_value(ui, "test_timeout", &test_timeout_str, src)?;

        let (max_duration_str, src) =
            resolve_timeout(self.max_duration.as_deref(), &config.max_duration)?;
        print_value(ui, "max_duration", &max_duration_str, src)?;

        let (no_output_str, src) = resolve_no_output_timeout(
            self.no_output_timeout.as_deref(),
            &config.no_output_timeout,
        )?;
        print_value(ui, "no_output_timeout", &no_output_str, src)?;

        let (order_str, src) = resolve_order(self.order.as_deref(), &config.test_order)?;
        print_value(ui, "test_order", &order_str, src)?;

        let (concurrency_str, src) = resolve_concurrency(self.concurrency);
        print_value(ui, "concurrency", &concurrency_str, src)?;

        Ok(0)
    }

    fn name(&self) -> &str {
        "config"
    }

    fn help(&self) -> &str {
        "Show the resolved effective configuration (config file + CLI flags + defaults)"
    }
}

fn print_value(ui: &mut dyn UI, key: &str, value: &str, src: Source) -> Result<()> {
    ui.output(&format!("{}: {} [{}]", key, value, src.label()))
}

fn print_unset(ui: &mut dyn UI, key: &str, placeholder: &str) -> Result<()> {
    ui.output(&format!("{}: {}", key, placeholder))
}

fn print_optional_string(ui: &mut dyn UI, key: &str, value: &Option<String>) -> Result<()> {
    match value {
        Some(v) => print_value(ui, key, v, Source::Config),
        None => print_unset(ui, key, "(unset)"),
    }
}

fn resolve_timeout(cli: Option<&str>, config_value: &Option<String>) -> Result<(String, Source)> {
    if let Some(s) = cli {
        let parsed = TimeoutSetting::parse(s)?;
        return Ok((format_timeout(&parsed), Source::Cli));
    }
    match config_value {
        Some(s) => {
            TimeoutSetting::parse(s)?;
            Ok((s.trim().to_string(), Source::Config))
        }
        None => Ok((format_timeout(&TimeoutSetting::Disabled), Source::Default)),
    }
}

fn resolve_no_output_timeout(
    cli: Option<&str>,
    config_value: &Option<String>,
) -> Result<(String, Source)> {
    if let Some(s) = cli {
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("disabled") {
            return Ok(("disabled".to_string(), Source::Cli));
        }
        let parsed = crate::config::parse_duration_string(trimmed)?;
        return Ok((format_duration(parsed), Source::Cli));
    }
    match config_value {
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("disabled") {
                Ok(("disabled".to_string(), Source::Config))
            } else {
                crate::config::parse_duration_string(trimmed)?;
                Ok((trimmed.to_string(), Source::Config))
            }
        }
        None => Ok(("disabled".to_string(), Source::Default)),
    }
}

fn resolve_order(cli: Option<&str>, config_value: &Option<String>) -> Result<(String, Source)> {
    if let Some(s) = cli {
        let parsed: TestOrder = s.parse()?;
        return Ok((parsed.as_str(), Source::Cli));
    }
    match config_value {
        Some(s) => {
            let parsed: TestOrder = s.parse()?;
            Ok((parsed.as_str(), Source::Config))
        }
        None => Ok((TestOrder::Discovery.as_str(), Source::Default)),
    }
}

fn resolve_concurrency(cli: Option<usize>) -> (String, Source) {
    match cli {
        Some(0) => (
            format!("{} (auto / cpu count)", num_cpus::get()),
            Source::Cli,
        ),
        Some(n) => (n.to_string(), Source::Cli),
        None => ("1 (serial)".to_string(), Source::Default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_ui::TestUI;
    use tempfile::TempDir;

    #[test]
    fn shows_no_config_file_when_missing() {
        let temp = TempDir::new().unwrap();
        let mut ui = TestUI::new();
        let cmd = ConfigCommand::new(Some(temp.path().to_string_lossy().to_string()));
        let exit = cmd.execute(&mut ui).unwrap();
        assert_eq!(exit, 0);

        let joined = ui.output.join("\n");
        assert!(
            joined.contains("Config file: (none found)"),
            "got: {}",
            joined
        );
        assert!(
            joined.contains("test_timeout: disabled [default]"),
            "got: {}",
            joined
        );
        assert!(
            joined.contains("max_duration: disabled [default]"),
            "got: {}",
            joined
        );
        assert!(
            joined.contains("no_output_timeout: disabled [default]"),
            "got: {}",
            joined,
        );
        assert!(
            joined.contains("test_order: discovery [default]"),
            "got: {}",
            joined
        );
        assert!(
            joined.contains("concurrency: 1 (serial) [default]"),
            "got: {}",
            joined
        );
    }

    #[test]
    fn reads_values_from_config_file() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("inquest.toml"),
            r#"
test_command = "python -m subunit.run $IDOPTION"
test_id_option = "--load-list $IDFILE"
test_list_option = "--list"
test_timeout = "5m"
max_duration = "1h"
no_output_timeout = "60s"
test_order = "alphabetical"
"#,
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = ConfigCommand::new(Some(temp.path().to_string_lossy().to_string()));
        cmd.execute(&mut ui).unwrap();

        let joined = ui.output.join("\n");
        assert!(joined.contains("test_command: python -m subunit.run $IDOPTION [config]"));
        assert!(joined.contains("test_id_option: --load-list $IDFILE [config]"));
        assert!(joined.contains("test_list_option: --list [config]"));
        assert!(joined.contains("test_timeout: 5m [config]"));
        assert!(joined.contains("max_duration: 1h [config]"));
        assert!(joined.contains("no_output_timeout: 60s [config]"));
        assert!(joined.contains("test_order: alphabetical [config]"));
    }

    #[test]
    fn cli_flags_override_config() {
        let temp = TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("inquest.toml"),
            r#"
test_command = "python -m test"
test_timeout = "5m"
test_order = "alphabetical"
"#,
        )
        .unwrap();

        let mut ui = TestUI::new();
        let cmd = ConfigCommand {
            base_path: Some(temp.path().to_string_lossy().to_string()),
            test_timeout: Some("auto".to_string()),
            max_duration: Some("30m".to_string()),
            no_output_timeout: Some("90s".to_string()),
            order: Some("failing-first".to_string()),
            concurrency: Some(4),
        };
        cmd.execute(&mut ui).unwrap();

        let joined = ui.output.join("\n");
        assert!(
            joined.contains("test_timeout: auto [cli]"),
            "got: {}",
            joined
        );
        assert!(
            joined.contains("max_duration: 30m [cli]"),
            "got: {}",
            joined
        );
        assert!(
            joined.contains("no_output_timeout: 90s [cli]"),
            "got: {}",
            joined
        );
        assert!(
            joined.contains("test_order: failing-first [cli]"),
            "got: {}",
            joined
        );
        assert!(joined.contains("concurrency: 4 [cli]"), "got: {}", joined);
    }

    #[test]
    fn invalid_cli_timeout_errors() {
        let temp = TempDir::new().unwrap();
        let mut ui = TestUI::new();
        let cmd = ConfigCommand {
            base_path: Some(temp.path().to_string_lossy().to_string()),
            test_timeout: Some("not-a-duration".to_string()),
            max_duration: None,
            no_output_timeout: None,
            order: None,
            concurrency: None,
        };
        assert!(cmd.execute(&mut ui).is_err());
    }

    #[test]
    fn shows_unset_for_optional_fields() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("inquest.toml"), r#"test_command = "echo""#).unwrap();

        let mut ui = TestUI::new();
        let cmd = ConfigCommand::new(Some(temp.path().to_string_lossy().to_string()));
        cmd.execute(&mut ui).unwrap();

        let joined = ui.output.join("\n");
        assert!(
            joined.contains("test_id_option: (unset)"),
            "got: {}",
            joined
        );
        assert!(
            joined.contains("test_list_option: (unset)"),
            "got: {}",
            joined
        );
        assert!(joined.contains("group_regex: (unset)"), "got: {}", joined);
    }
}
